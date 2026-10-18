// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::json;

#[cfg(target_os = "linux")]
use super::super::tests::debian_root;
use super::super::tests::{DEBIAN_OS_RELEASE, FakeRoot, UBUNTU_OS_RELEASE, probe};
use super::*;

fn distribution(fake: &FakeRoot) -> Result<Value, String> {
    distribution_with(fake, None)
}

fn distribution_with(fake: &FakeRoot, distro: Option<&str>) -> Result<Value, String> {
    let root = fake.root();
    let mut probe = probe();
    probe.distro = distro.map(str::to_string);
    let host = fake.host(&root, &probe);
    let lsb = LsbRelease::run(host.root, &host.env, super::super::unbounded()).unwrap();
    collect(&host, &lsb).map(Value::Object)
}

/// A system `distro` other than 1.9 hands back: the module would rank versions differently.
#[test]
#[cfg(target_os = "linux")]
fn a_system_distro_other_than_1_9_hands_back() {
    let fake = debian_root("system-distro");
    assert!(distribution_with(&fake, Some("1.9.0")).is_ok());
    let reason = distribution_with(&fake, Some("1.5.0")).unwrap_err();
    assert!(reason.contains("1.5.0"), "{reason}");
    assert!(
        distribution_with(&fake, Some("")).is_err(),
        "a version nobody could read"
    );
}

/// Ubuntu is read like the reference, another distribution hands back, os-release splits like shlex.
#[test]
fn distributions_are_read_like_the_reference() {
    {
        let fake = FakeRoot::new("ubuntu");
        fake.write("/etc/os-release", UBUNTU_OS_RELEASE)
            .write("/etc/debian_version", "trixie/sid\n")
            .write("/etc/lsb-release", "DISTRIB_ID=Ubuntu\n");
        assert_eq!(
            distribution(&fake).unwrap(),
            json!({
                "distribution_release": "noble",
                "distribution_version": "24.04",
                "distribution_major_version": "24",
                "distribution_file_path": "/etc/os-release",
                "distribution_file_variety": "Debian",
                "distribution_file_parsed": true,
                "distribution": "Ubuntu",
                "os_family": "Debian",
            })
        );
    }
    {
        let fake = FakeRoot::new("fedora");
        assert!(distribution(&fake).is_err(), "no os-release");
        fake.write(
            "/etc/os-release",
            "NAME=\"Fedora Linux\"\nVERSION_ID=40\nID=fedora\nVERSION_CODENAME=\"\"\n",
        );
        let reason = distribution(&fake).unwrap_err();
        assert!(reason.contains("fedora"), "{reason}");
        fake.write("/etc/os-release", UBUNTU_OS_RELEASE);
        assert!(distribution(&fake).is_ok());
        for leftover in ["/etc/redhat-release", "/etc/fedora-release"] {
            fake.write(leftover, "Fedora release 40 (Forty)\n");
            assert!(distribution(&fake).is_err(), "{leftover}");
            std::fs::remove_file(fake.0.join(leftover.trim_start_matches('/'))).unwrap();
        }
        fake.write("/etc/arch-release", "");
        assert!(distribution(&fake).is_err(), "an empty arch-release counts");
        std::fs::remove_file(fake.0.join("etc/arch-release")).unwrap();
        fake.write(
            "/etc/os-release",
            &UBUNTU_OS_RELEASE
                .replace("VERSION_CODENAME=noble\n", "")
                .replace("UBUNTU_CODENAME=noble\n", ""),
        );
        assert!(distribution(&fake).is_err(), "no codename");
        fake.write(
            "/etc/os-release",
            DEBIAN_OS_RELEASE
                .replace("12 (bookworm)\"\nNAME", "12.1 (bookworm)\"\nNAME")
                .as_str(),
        );
        assert!(
            distribution(&fake).is_err(),
            "a dotted pretty name on Debian"
        );
    }
    {
        let parsed = parse_os_release(
            "A=\"x y\"z\nB='q \\ r'\nC=\"a\\\"b\\$c\\\\\"\nD=u#rest=1\n# E=no\nF=\"\"\nG=a\\ b\n",
        )
        .unwrap();
        let expected: Vec<(String, String)> = [
            ("a", "x yz"),
            ("b", "q \\ r"),
            ("c", "a\"b\\$c\\"),
            ("d", "u"),
            ("f", ""),
            ("g", "a b"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(parsed, expected);
        assert!(parse_os_release("A=\"open\n").is_err());
    }
    {
        assert_eq!(
            pretty_name_release("PRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\nNAME=x"),
            Some("bookworm")
        );
        assert_eq!(
            pretty_name_release(
                "PRETTY_NAME=\"Debian GNU/Linux trixie/sid\"\nVERSION=\"13 (trixie)\""
            ),
            Some("trixie"),
            "[^(]+ runs across lines to the next parenthesis"
        );
        assert_eq!(pretty_name_release("PRETTY_NAME=\"Debian\"\n"), None);
        assert_eq!(pretty_name_release("PRETTY_NAME=a b)"), Some("b"));
        assert_eq!(minor_version("12.7"), Some("7"));
        assert_eq!(minor_version("v 1.23.4"), Some("23"));
        assert_eq!(minor_version("trixie/sid"), None);
        assert_eq!(minor_version("12. 3.4"), Some("4"));
        assert!(release_file_name("fedora-release"));
        assert!(release_file_name("debian_version"));
        assert!(!release_file_name("-release"));
        assert!(!release_file_name("os.release"));
    }
}

/// Debian 12, sanitised: the version is `/etc/debian_version`'s because it has the most dots,
/// the release comes from `PRETTY_NAME`, the minor version from `debian_version`.
#[test]
#[cfg(target_os = "linux")]
fn debian_is_read_like_the_reference() {
    let fake = debian_root("debian");
    assert_eq!(
        distribution(&fake).unwrap(),
        json!({
            "distribution_release": "bookworm",
            "distribution_version": "12.7",
            "distribution_major_version": "12",
            "distribution_file_path": "/etc/os-release",
            "distribution_file_variety": "Debian",
            "distribution_file_parsed": true,
            "distribution": "Debian",
            "distribution_minor_version": "7",
            "os_family": "Debian",
        })
    );
}
