// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

/// What the callback computes, worked out by its own Python for the same arguments and files.
#[test]
fn the_extra_vars_hash_is_the_callback_s() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("vars.yml"), "x: 1\n").unwrap();
    assert_eq!(
        extra_vars_hash(&["a=1", "@vars.yml", "@missing.yml"], dir.path()),
        "430e1415e0bd874786e0fc304fea5ea91b6c68565db6578fe43eb58b22a124f4"
    );
}

fn header(dir: &Path, playbook_hash: &str) -> recording::Header {
    let site = sha256_of(&dir.join("site.yml")).unwrap();
    serde_json::from_value(serde_json::json!({
        "format": 1,
        "ansible_core": "2.19.12",
        "cli": {"extra_vars": ["@vars.yml"]},
        "cwd": "/recorded/here",
        "playbooks": [{"path": "/recorded/here/site.yml", "sha256": playbook_hash.replace("SITE", &site)}],
        "inventory": {"sources": ["/recorded/here/hosts.ini"], "files": []},
        "extra_vars_hash": extra_vars_hash(&["@vars.yml"], dir),
    }))
    .unwrap()
}

/// The files are found from the recording's working directory, and the first one that differs
/// is named as the checkout has it.
#[test]
fn a_changed_file_refuses_the_replay() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("site.yml"), "- hosts: all\n").unwrap();
    std::fs::write(dir.path().join("vars.yml"), "x: 1\n").unwrap();
    check(&header(dir.path(), "SITE"), dir.path()).unwrap();

    let err = check(&header(dir.path(), "0000"), dir.path()).unwrap_err();
    assert_eq!(err, "site.yml changed since the recording");

    let recorded = header(dir.path(), "SITE");
    std::fs::write(dir.path().join("vars.yml"), "x: 2\n").unwrap();
    let err = check(&recorded, dir.path()).unwrap_err();
    assert!(err.contains("-e @file"), "{err}");

    std::fs::remove_file(dir.path().join("site.yml")).unwrap();
    let err = check(&recorded, dir.path()).unwrap_err();
    assert!(err.starts_with("site.yml cannot be read"), "{err}");
}

#[rstest::rstest]
#[case("2.19.12", true)]
#[case("2.19.0", true)]
#[case("2.20.1", false)]
#[case("2.1.9", false)]
fn only_the_2_19_series_is_in_profile(#[case] version: &str, #[case] expected: bool) {
    assert_eq!(in_profile(version), expected);
}
