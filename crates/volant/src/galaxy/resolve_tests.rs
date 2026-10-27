// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;

use rstest::rstest;

use super::Failure;
use super::resolve::{Index, Options, Request, resolve};
use super::version::{Version, meets};

/// Collections in memory: versions, each with its dependencies.
type Versions = Vec<(&'static str, Vec<(&'static str, &'static str)>)>;

struct Memory(HashMap<&'static str, Versions>);

impl Index for Memory {
    fn versions(&mut self, fqcn: &str) -> Result<Vec<String>, Failure> {
        Ok(self
            .0
            .get(fqcn)
            .map(|v| {
                v.iter()
                    .map(|(version, _)| (*version).to_string())
                    .collect()
            })
            .unwrap_or_default())
    }

    fn dependencies(
        &mut self,
        fqcn: &str,
        version: &str,
    ) -> Result<Vec<(String, String)>, Failure> {
        Ok(self.0[fqcn]
            .iter()
            .find(|(v, _)| *v == version)
            .map(|(_, deps)| {
                deps.iter()
                    .map(|(f, r)| ((*f).to_string(), (*r).to_string()))
                    .collect()
            })
            .unwrap_or_default())
    }
}

/// A slice of galaxy.ansible.com as the issue measured it.
fn galaxy() -> Memory {
    Memory(HashMap::from([
        (
            "ansible.utils",
            ["5.0.0", "5.1.0", "5.1.1", "5.1.2", "6.0.0", "6.1.1"]
                .into_iter()
                .map(|v| (v, Vec::new()))
                .collect(),
        ),
        (
            "ansible.netcommon",
            vec![
                ("1.5.0", vec![]),
                ("2.5.1", vec![("ansible.utils", ">=2.0.0")]),
                ("6.1.3", vec![("ansible.utils", ">=2.0.0")]),
                ("7.0.0", vec![("ansible.utils", ">=2.0.0")]),
                ("8.7.1", vec![("ansible.utils", ">=2.0.0")]),
            ],
        ),
        (
            "cisco.ios",
            vec![("5.0.0", vec![("ansible.netcommon", ">=2.5.1")])],
        ),
        // Three levels: a.top 2.0.0 needs b.mid >=2, whose 2.0.0 needs c.low >=3, which does
        // not exist; a.top 1.0.0 takes b.mid <2.
        (
            "a.top",
            vec![
                ("1.0.0", vec![("b.mid", "<2.0.0")]),
                ("2.0.0", vec![("b.mid", ">=2.0.0")]),
            ],
        ),
        (
            "b.mid",
            vec![
                ("1.0.0", vec![("c.low", "*")]),
                ("2.0.0", vec![("c.low", ">=3.0.0")]),
            ],
        ),
        ("c.low", vec![("1.0.0", vec![]), ("2.0.0", vec![])]),
        ("p.re", vec![("1.0.0", vec![]), ("1.1.0-beta1", vec![])]),
    ]))
}

fn pins(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(f, v)| ((*f).to_string(), (*v).to_string()))
        .collect()
}

#[rstest]
#[case::highest_in_range(&[("ansible.utils", ">=5.0.0,<6.0.0")], &[("ansible.utils", "5.1.2")])]
#[case::an_excluded_version(&[("ansible.utils", ">=5.0.0,<6.0.0,!=5.1.0")], &[("ansible.utils", "5.1.2")])]
#[case::dependencies_in_install_order(&[("cisco.ios", "5.0.0")], &[("cisco.ios", "5.0.0"), ("ansible.netcommon", "8.7.1"), ("ansible.utils", "6.1.1")])]
#[case::a_range_over_the_highest(&[("cisco.ios", "5.0.0"), ("ansible.netcommon", "<7.0.0")], &[("cisco.ios", "5.0.0"), ("ansible.netcommon", "6.1.3"), ("ansible.utils", "6.1.1")])]
#[case::three_levels_deep(&[("a.top", "*")], &[("a.top", "1.0.0"), ("b.mid", "1.0.0"), ("c.low", "2.0.0")])]
#[case::no_pre_release_by_default(&[("p.re", "*")], &[("p.re", "1.0.0")])]
#[case::a_pre_release_named(&[("p.re", "1.1.0-beta1")], &[("p.re", "1.1.0-beta1")])]
fn a_resolution(#[case] asked: &[(&str, &str)], #[case] want: &[(&str, &str)]) {
    let requests: Vec<Request> = asked.iter().map(|(f, r)| Request::direct(f, r)).collect();
    assert_eq!(
        resolve(&requests, &mut galaxy(), &Options::default()),
        Ok(pins(want))
    );
}

#[test]
fn pre_releases_count_with_pre() {
    let options = Options {
        pre: true,
        ..Options::default()
    };
    assert_eq!(
        resolve(&[Request::direct("p.re", "*")], &mut galaxy(), &options),
        Ok(pins(&[("p.re", "1.1.0-beta1")]))
    );
}

#[test]
fn an_installed_version_that_fits_is_kept_unless_upgrading() {
    let mut options = Options {
        preferred: pins(&[("ansible.utils", "6.0.0")]),
        ..Options::default()
    };
    let asked = [Request::direct("ansible.utils", "*")];
    assert_eq!(
        resolve(&asked, &mut galaxy(), &options),
        Ok(pins(&[("ansible.utils", "6.0.0")]))
    );
    options.upgrade = true;
    assert_eq!(
        resolve(&asked, &mut galaxy(), &options),
        Ok(pins(&[("ansible.utils", "6.1.1")]))
    );
}

const HINT: &str = "Hint: Pre-releases hosted on Galaxy or Automation Hub are not installed by default unless a specific version is requested. To enable pre-releases globally, use --pre.";

/// The measured message, up to the Python objects ansible-galaxy dumps after the hint.
#[test]
fn a_conflict_names_every_range_asked() {
    let asked = [
        Request::direct("cisco.ios", "5.0.0"),
        Request::direct("ansible.netcommon", "<2.0.0"),
    ];
    assert_eq!(
        resolve(&asked, &mut galaxy(), &Options::default()),
        Err(Failure::error(format!(
            "Failed to resolve the requested dependencies map. Could not satisfy the following requirements:\n* ansible.netcommon:<2.0.0 (direct request)\n* ansible.netcommon:>=2.5.1 (dependency of cisco.ios:5.0.0)\n{HINT}"
        )))
    );
}

#[test]
fn an_unknown_collection_is_a_direct_request_not_met() {
    assert_eq!(
        resolve(
            &[Request::direct("demo.nope", "*")],
            &mut galaxy(),
            &Options::default()
        ),
        Err(Failure::error(format!(
            "Failed to resolve the requested dependencies map. Could not satisfy the following requirements:\n* demo.nope:* (direct request)\n{HINT}"
        )))
    );
}

#[rstest]
#[case("1.0.0-alpha", "1.0.0-alpha.1")]
#[case("1.0.0-alpha.1", "1.0.0-alpha.beta")]
#[case("1.0.0-alpha.beta", "1.0.0-beta")]
#[case("1.0.0-beta.2", "1.0.0-beta.11")]
#[case("1.0.0-rc.1", "1.0.0")]
#[case("1.0.0", "1.0.1")]
#[case("1.9.0", "1.10.0")]
fn versions_order_as_semver_says(#[case] lower: &str, #[case] higher: &str) {
    assert!(Version::parse(lower).expect("a version") < Version::parse(higher).expect("a version"));
}

#[test]
fn build_metadata_does_not_order() {
    assert_eq!(Version::parse("1.0.0+a"), Version::parse("1.0.0+b"));
}

#[rstest]
#[case("5.1.2", "*", true)]
#[case("5.1.2", "5.1.2", true)]
#[case("5.1.2", "==5.1.2", true)]
#[case("5.1.2", ">=5.0.0,<6.0.0", true)]
#[case("6.0.0", ">=5.0.0,<6.0.0", false)]
#[case("1.9.9", "<2", true)]
#[case("2.0.0", "<2", false)]
#[case("5.1.0", "!=5.1.0", false)]
#[case("5.1.0", ">5.0", true)]
fn a_range(#[case] version: &str, #[case] range: &str, #[case] met: bool) {
    assert_eq!(meets(version, range), met);
}
