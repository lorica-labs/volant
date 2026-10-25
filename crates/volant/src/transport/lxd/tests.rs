// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::{Value, json};

use crate::transport::{ConnectionKeywords, ExecTarget, Transport};

/// The exec target of `vars` over `plugin`, with the runtime's path made a bare name.
fn target(plugin: &str, vars: Value) -> ExecTarget {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    for name in ["lxc", "incus"] {
        let file = dir.path().join(name);
        std::fs::write(&file, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // nextest gives each test a process of its own, so no other thread reads the variable.
    unsafe { std::env::set_var("PATH", dir.path()) };
    let mut vars = vars.as_object().unwrap().clone();
    vars.insert("ansible_connection".into(), json!(plugin));
    let bare = |word: &mut String| {
        if let Some(name) = word.strip_prefix(&format!("{}/", dir.path().display())) {
            *word = name.to_string();
        }
    };
    match Transport::for_task(
        "web.example.com",
        &vars,
        &crate::testing::defaults(),
        &ConnectionKeywords::default(),
    )
    .unwrap()
    {
        Transport::Exec(mut target) => {
            target.prefix.iter_mut().for_each(bare);
            if let Some(copy) = &mut target.copy {
                copy.command.iter_mut().for_each(bare);
                copy.arch.iter_mut().flatten().for_each(bare);
            }
            target
        }
        other => panic!("an exec target, got {other:?}"),
    }
}

#[test]
fn lxd_runs_in_the_instance_before_the_first_dot() {
    let target = target("community.general.lxd", json!({}));
    assert_eq!(target.prefix, ["lxc", "exec", "local:web", "--"]);
    assert_eq!(target.name, "web");
    assert_eq!(target.user.as_deref(), Some("root"));
    let copy = target.copy.expect("root gets the copy strategy");
    assert_eq!(
        copy.command,
        [
            "lxc",
            "file",
            "push",
            "--mode",
            "0755",
            "{src}",
            "local:web/{dest}"
        ]
    );
    assert_eq!(
        copy.arch,
        [["lxc", "config", "get", "local:web", "image.architecture"]]
    );
}

#[test]
fn lxd_reads_its_own_variables_over_the_shared_ones() {
    let target = target(
        "community.general.lxd",
        json!({
            "ansible_host": "other",
            "ansible_lxd_host": "app1.lan",
            "ansible_lxd_project": "p1",
            "ansible_lxd_remote": "r1",
            "ansible_executable": "/bin/dash",
            "ansible_lxd_executable": "/bin/bash",
        }),
    );
    assert_eq!(
        target.prefix,
        ["lxc", "--project", "p1", "exec", "r1:app1", "--"]
    );
    assert_eq!(target.shell, "/bin/bash");
    assert_eq!(target.failures[0].1, "instance not running: app1");
}

#[test]
fn a_user_other_than_root_goes_through_the_become_method() {
    let target = target(
        "community.general.lxd",
        json!({"ansible_user": "bob", "lxd_become_method": "/usr/bin/sudo"}),
    );
    assert_eq!(
        target.prefix,
        [
            "lxc",
            "exec",
            "local:web",
            "--",
            "/usr/bin/sudo",
            "bob",
            "-c"
        ]
    );
    assert_eq!(target.copy, None, "su -c takes one command, so no copy");
    assert_eq!(
        target.info.as_deref(),
        Some(
            "INFO: Running as non-root user: bob, trying to run 'lxc exec' with become method: /usr/bin/sudo"
        )
    );
}

#[test]
fn incus_always_names_a_project_and_words_its_refusals_with_it() {
    let target = target(
        "community.general.incus",
        json!({"ansible_incus_remote": "r2"}),
    );
    assert_eq!(
        target.prefix,
        ["incus", "--project", "default", "exec", "r2:web", "--"]
    );
    assert_eq!(target.kind, "Incus");
    assert_eq!(
        target
            .failures
            .iter()
            .map(|(_, said)| said.as_str())
            .collect::<Vec<_>>(),
        [
            "instance not running: web (remote=r2, project=default)",
            "instance not found: web (remote=r2, project=default)",
            "instance access denied: web (remote=r2, project=default)",
            "instance access denied: web (remote=r2, project=default)",
        ]
    );
}
