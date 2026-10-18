// SPDX-License-Identifier: GPL-3.0-or-later
use std::os::unix::fs::PermissionsExt;

use rstest::rstest;

use super::*;
use crate::inventory::{Note, PluginConfig};

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

/// What the chain says for one script file, as `Note`s.
fn notes_of(body: &str, config: &PluginConfig) -> Vec<Note> {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("inv.sh");
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let loaded = crate::inventory::load_sources(&[Source::new(&script.to_string_lossy())], config);
    loaded.notes
}

#[rstest]
#[case::shown(
    true,
    "echo oops >&2\necho '{\"_meta\":{\"hostvars\":{}}}'",
    Some("oops")
)]
#[case::off(false, "echo oops >&2\necho '{\"_meta\":{\"hostvars\":{}}}'", None)]
fn a_successful_scripts_stderr_is_an_error_line(
    #[case] on: bool,
    #[case] body: &str,
    #[case] said: Option<&str>,
) {
    let config = PluginConfig {
        script_stderr: on,
        ..PluginConfig::default()
    };
    let notes = notes_of(&format!("#!/bin/sh\n{body}\n"), &config);
    let errors: Vec<&str> = notes
        .iter()
        .filter_map(|n| match n {
            Note::Error(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(errors, said.into_iter().collect::<Vec<_>>());
}

#[test]
fn a_script_whose_interpreter_is_missing_names_the_errno() {
    let notes = notes_of(
        "#!/nonexistent/interpreter\necho {}\n",
        &PluginConfig::default(),
    );
    let warned = notes.iter().any(|n| {
        matches!(n, Note::Warning(text)
            if text.contains("--list': [Errno 2] No such file or directory: '")
                && text.contains("inv.sh'"))
    });
    assert!(warned, "{notes:?}");
}
