// SPDX-License-Identifier: GPL-3.0-or-later
use std::process::Command;

#[test]
fn version_shows_semver_sha_and_date() {
    let out = Command::new(env!("CARGO_BIN_EXE_volant"))
        .arg("--version")
        .output()
        .expect("binary runs");
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).expect("utf-8");
    // Expected shape: "volant 0.1.0-alpha.1 (0123abcde 2026-09-05)"
    let mut parts = text.trim().splitn(3, ' ');
    assert_eq!(parts.next(), Some("volant"));
    assert_eq!(parts.next(), Some(env!("CARGO_PKG_VERSION")));
    let rest = parts.next().expect("build info");
    assert!(rest.starts_with('(') && rest.ends_with(')'), "{rest}");
    let (sha, date) = rest[1..rest.len() - 1].split_once(' ').expect("sha and date");
    assert!(!sha.is_empty());
    assert_eq!(date.len(), 10, "{date}");
    assert_eq!(&date[4..5], "-");
    assert_eq!(&date[7..8], "-");
}
