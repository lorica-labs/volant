// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::{Value, json};

use crate::transport::{ConnectionKeywords, ExecTarget, Feed, Transport};

fn target(vars: Value) -> ExecTarget {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let buildah = dir.path().join("buildah");
    std::fs::write(&buildah, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&buildah, std::fs::Permissions::from_mode(0o755)).unwrap();
    // nextest gives each test a process of its own, so no other thread reads the variable.
    unsafe { std::env::set_var("PATH", dir.path()) };
    let mut vars = vars.as_object().unwrap().clone();
    vars.insert(
        "ansible_connection".into(),
        json!("containers.podman.buildah"),
    );
    match Transport::for_task(
        "c1",
        &vars,
        &crate::testing::defaults(),
        &ConnectionKeywords::default(),
    )
    .unwrap()
    {
        Transport::Exec(mut target) => {
            // The directory goes with this function; the words keep its path.
            target.prefix[0] = "buildah".into();
            if let Some(copy) = &mut target.copy {
                copy.command[0] = "buildah".into();
                copy.arch[0][0] = "buildah".into();
            }
            target
        }
        other => panic!("an exec target, got {other:?}"),
    }
}

#[test]
fn a_user_adds_run_s_user_and_copy_s_chown() {
    let target = target(json!({"ansible_user": "bob"}));
    assert_eq!(
        target.prefix,
        ["buildah", "run", "--user", "bob", "c1", "--"]
    );
    let copy = target.copy.expect("a copy command");
    assert_eq!(copy.feed, Feed::LocalFile);
    assert_eq!(
        copy.command,
        [
            "buildah", "copy", "--chmod", "0755", "--chown", "bob", "c1", "{src}", "{dest}"
        ]
    );
    assert_eq!(
        copy.arch,
        [[
            "buildah",
            "inspect",
            "--format",
            "{{.OCIv1.Architecture}}",
            "c1"
        ]]
    );
}

#[test]
fn root_or_no_user_adds_no_flag() {
    for vars in [
        json!({}),
        json!({"ansible_user": "root", "ansible_host": "working"}),
    ] {
        let target = target(vars);
        assert_eq!(target.user, None);
        assert!(
            !target.prefix.iter().any(|word| word == "--user"),
            "{:?}",
            target.prefix
        );
        assert!(
            !target
                .copy
                .unwrap()
                .command
                .iter()
                .any(|word| word == "--chown")
        );
    }
}
