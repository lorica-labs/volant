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

/// `--version --build-id` adds the blake3 hash of the agent's own file, which is what the
/// controller compares with the binary it would upload before it reuses a cached agent. An agent
/// older than the flag ignores it and prints the bare version, which never matches.
#[test]
fn build_id_is_the_hash_of_the_agents_own_file() {
    let exe = env!("CARGO_BIN_EXE_volant-agent");
    let out = Command::new(exe)
        .args(["--version", "--build-id"])
        .output()
        .expect("binary runs");
    assert!(out.status.success(), "{out:?}");
    let hash = blake3::hash(&std::fs::read(exe).expect("the agent file reads"));
    assert_eq!(
        String::from_utf8(out.stdout).expect("utf-8").trim(),
        format!(
            "volant-agent {} {}",
            env!("CARGO_PKG_VERSION"),
            hash.to_hex()
        )
    );
}
