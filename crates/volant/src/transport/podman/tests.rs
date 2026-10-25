// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::{Value, json};

use super::PodmanSettings;
use crate::transport::{
    ConnectionDefaults, ConnectionKeywords, ExecTarget, PluginSettings, Transport,
};

/// A directory with an executable `podman` in it.
fn fake_podman() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let podman = dir.path().join("podman");
    std::fs::write(&podman, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

fn target(vars: Value, defaults: &ConnectionDefaults) -> ExecTarget {
    let mut vars = vars.as_object().unwrap().clone();
    vars.insert(
        "ansible_connection".into(),
        json!("containers.podman.podman"),
    );
    match Transport::for_task("h1", &vars, defaults, &ConnectionKeywords::default()).unwrap() {
        Transport::Exec(target) => target,
        other => panic!("an exec target, got {other:?}"),
    }
}

/// The plugin's source, not its documentation: the play context's address and user, the extra
/// arguments split as shell words, and `-i` so the agent gets its stdin.
#[test]
fn the_command_line_follows_the_plugin_s_source() {
    let dir = fake_podman();
    // nextest gives each test a process of its own, so no other thread reads the variable.
    unsafe { std::env::set_var("PATH", dir.path()) };
    let podman = dir.path().join("podman").display().to_string();
    let target = target(
        json!({
            "ansible_host": "app1",
            "ansible_podman_host": "not-read",
            "ansible_user": "bob",
            "ansible_podman_extra_args": "--remote --connection \"my conn\"",
        }),
        &crate::testing::defaults(),
    );
    assert_eq!(
        target.prefix,
        [
            &podman,
            "--remote",
            "--connection",
            "my conn",
            "exec",
            "--user",
            "bob",
            "-i",
            "app1"
        ]
    );
    assert_eq!(target.name, "app1");
    assert_eq!(target.unavailable, None);
    let copy = target.copy.expect("a copy command");
    assert_eq!(
        copy.command,
        [
            &podman,
            "--remote",
            "--connection",
            "my conn",
            "cp",
            "-",
            "app1:{dir}"
        ]
    );
}

#[test]
fn the_executable_and_the_extra_arguments_come_from_the_settings_under_the_variables() {
    let dir = fake_podman();
    let podman = dir.path().join("podman").display().to_string();
    let defaults = ConnectionDefaults {
        plugins: PluginSettings {
            podman: PodmanSettings {
                executable: Some(podman.clone()),
                extra_args: Some("--log-level debug".into()),
            },
            ..PluginSettings::default()
        },
        ..crate::testing::defaults()
    };
    let target = target(json!({}), &defaults);
    assert_eq!(
        target.prefix,
        [&podman, "--log-level", "debug", "exec", "-i", "h1"]
    );
    let named = self::target(
        json!({"ansible_podman_executable": "/nonexistent/podman"}),
        &defaults,
    );
    assert_eq!(
        named.unavailable.as_deref(),
        Some("/nonexistent/podman command not found in PATH")
    );
}
