// SPDX-License-Identifier: GPL-3.0-or-later
use std::process::Command;

#[test]
fn version_shows_name_and_semver() {
    let out = Command::new(env!("CARGO_BIN_EXE_volant-agent"))
        .arg("--version")
        .output()
        .expect("binary runs");
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).expect("utf-8");
    assert_eq!(
        text.trim(),
        format!("volant-agent {}", env!("CARGO_PKG_VERSION"))
    );
}
