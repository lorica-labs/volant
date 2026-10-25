// SPDX-License-Identifier: GPL-3.0-or-later
//! Connection names, resolved by the controller's ansible-core through the run's helper.

use serde_json::Value;

use super::Bridge;
use crate::transport::{Resolution, ResolveConnection};

impl ResolveConnection for Bridge {
    fn resolve_connection(&self, name: &str) -> anyhow::Result<Resolution> {
        let answer = self
            .defaults_request("connections", &[name.to_string()])
            .map_err(|e| anyhow::anyhow!("resolving the connection '{name}': {e}"))?;
        resolution_of(name, answer.get(name))
    }
}

/// One name's answer, as `connections` in the helper shapes it.
fn resolution_of(name: &str, answer: Option<&Value>) -> anyhow::Result<Resolution> {
    let shape = || {
        anyhow::anyhow!(
            "the Python helper answered the connection '{name}' in a shape it should not"
        )
    };
    let answer = answer.and_then(Value::as_object).ok_or_else(shape)?;
    if let Some(why) = answer.get("unusable") {
        return Ok(Resolution::Unusable(
            why.as_str().ok_or_else(shape)?.to_string(),
        ));
    }
    if answer.contains_key("missing") {
        let warning = match answer.get("warning") {
            None | Some(Value::Null) => None,
            Some(text) => Some(text.as_str().ok_or_else(shape)?.to_string()),
        };
        return Ok(Resolution::Missing { warning });
    }
    let fqcn = answer
        .get("fqcn")
        .and_then(Value::as_str)
        .ok_or_else(shape)?;
    let hops = answer
        .get("hops")
        .and_then(Value::as_array)
        .ok_or_else(shape)?
        .iter()
        .map(|hop| hop.as_str().map(str::to_string).ok_or_else(shape))
        .collect::<anyhow::Result<_>>()?;
    Ok(Resolution::Found {
        fqcn: fqcn.to_string(),
        hops,
    })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::resolution_of;
    use crate::transport::Resolution;

    #[rstest]
    #[case(
        json!({"fqcn": "community.general.chroot", "hops": ["chroot", "ansible.builtin.chroot", "community.general.chroot"]}),
        Resolution::Found {
            fqcn: "community.general.chroot".into(),
            hops: vec!["chroot".into(), "ansible.builtin.chroot".into(), "community.general.chroot".into()],
        }
    )]
    #[case(json!({"missing": true, "warning": null}), Resolution::Missing { warning: None })]
    #[case(
        json!({"missing": true, "warning": "Error loading plugin 'a.b.c': No module named 'ansible_collections.a'"}),
        Resolution::Missing { warning: Some("Error loading plugin 'a.b.c': No module named 'ansible_collections.a'".into()) }
    )]
    #[case(
        json!({"unusable": "AnsiblePluginRemovedError: gone"}),
        Resolution::Unusable("AnsiblePluginRemovedError: gone".into())
    )]
    fn the_helper_s_answer_reads_as_a_resolution(
        #[case] answer: serde_json::Value,
        #[case] want: Resolution,
    ) {
        assert_eq!(resolution_of("x", Some(&answer)).unwrap(), want);
    }

    #[rstest]
    #[case(None)]
    #[case(Some(json!("chroot")))]
    #[case(Some(json!({"fqcn": "a.b.c"})))]
    fn an_answer_out_of_shape_is_an_error(#[case] answer: Option<serde_json::Value>) {
        assert!(resolution_of("x", answer.as_ref()).is_err());
    }

    /// The controller's ansible-core routes the short names, with the pinned community.general
    /// installed and community.docker absent: the job that installs the collections runs it.
    #[test]
    fn the_controller_resolves_connection_names() {
        use crate::bridge::Bridge;
        use crate::transport::ResolveConnection;

        let pinned = std::env::var_os("VOLANT_PYTHON").is_some();
        let bridge = Bridge::new(Vec::new());
        let chroot = match bridge.resolve_connection("chroot") {
            Ok(Resolution::Found { fqcn, hops }) => (fqcn, hops),
            other => {
                assert!(!pinned, "VOLANT_PYTHON cannot resolve chroot: {other:?}");
                eprintln!("skipped: no ansible-core with community.general: {other:?}");
                return;
            }
        };
        let hops = |names: &[&str]| names.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(
            chroot,
            (
                "community.general.chroot".to_string(),
                hops(&[
                    "chroot",
                    "ansible.builtin.chroot",
                    "community.general.chroot"
                ])
            )
        );
        assert_eq!(
            bridge.resolve_connection("ssh").unwrap(),
            Resolution::Found {
                fqcn: "ansible.builtin.ssh".into(),
                hops: hops(&["ssh"])
            }
        );
        assert_eq!(
            bridge.resolve_connection("docker").unwrap(),
            Resolution::Missing {
                warning: Some(
                    "Error loading plugin 'community.docker.docker': No module named 'ansible_collections.community.docker'".into()
                )
            }
        );
        assert_eq!(
            bridge.resolve_connection("nosuch").unwrap(),
            Resolution::Missing { warning: None }
        );
    }
}
