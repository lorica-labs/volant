// SPDX-License-Identifier: GPL-3.0-or-later
use std::os::unix::fs::PermissionsExt;

use rstest::rstest;

use super::*;
use crate::inventory::PluginConfig;

fn load(body: &str) -> (Vec<String>, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("calls.log");
    let script = dir.path().join("inv.sh");
    let text = format!("#!/bin/sh\necho \"$@\" >> {}\n{body}\n", log.display());
    std::fs::write(&script, text).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let loaded = crate::inventory::load_sources(
        &[Source::new(&script.to_string_lossy())],
        &PluginConfig::default(),
    );
    let hosts = loaded
        .inventory
        .resolve("all")
        .hosts
        .into_iter()
        .map(|h| h.name)
        .collect();
    let calls = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect();
    (hosts, calls)
}

#[rstest]
#[case::meta(
    r#"echo '{"_meta":{"hostvars":{"h1":{}}},"g":{"hosts":["h1"]}}'"#,
    &["--list"]
)]
#[case::no_meta(
    r#"[ "$1" = --list ] && echo '{"g":{"hosts":["h1","h2"]}}' || echo '{}'"#,
    &["--list", "--host h1", "--host h2"]
)]
fn host_calls_happen_only_without_meta(#[case] body: &str, #[case] calls: &[&str]) {
    let (hosts, got) = load(body);
    assert!(!hosts.is_empty());
    assert_eq!(got, calls);
}
