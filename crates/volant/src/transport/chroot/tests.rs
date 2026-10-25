// SPDX-License-Identifier: GPL-3.0-or-later

use std::os::unix::fs::PermissionsExt;

use serde_json::{Value, json};

use super::refusal;
use crate::transport::{ConnectionFailsTask, ConnectionKeywords, ExecTarget, Transport};

/// A tree with an executable `bin/sh`, which is all the plugin looks for.
fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("bin")).unwrap();
    let sh = dir.path().join("bin/sh");
    std::fs::write(&sh, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&sh, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

fn target(dir: &str, vars: Value) -> anyhow::Result<ExecTarget> {
    let mut vars = vars.as_object().unwrap().clone();
    vars.insert(
        "ansible_connection".into(),
        json!("community.general.chroot"),
    );
    match Transport::for_task(
        dir,
        &vars,
        &crate::testing::defaults(),
        &ConnectionKeywords::default(),
    )? {
        Transport::Exec(target) => Ok(target),
        other => panic!("an exec target, got {other:?}"),
    }
}

#[test]
fn the_plugin_s_refusals_keep_its_words_and_fail_the_task() {
    let empty = tempfile::tempdir().unwrap();
    let cases = [
        (
            "/nonexistent",
            "/nonexistent is not a directory".to_string(),
        ),
        (
            empty.path().to_str().unwrap(),
            format!(
                "[Errno 2] No such file or directory: '{}/bin/sh'",
                empty.path().display()
            ),
        ),
    ];
    for (dir, want) in cases {
        let err = target(dir, json!({})).unwrap_err();
        assert!(
            err.downcast_ref::<ConnectionFailsTask>().is_some(),
            "{err:#}"
        );
        assert_eq!(err.to_string(), want);
    }
}

#[test]
fn a_controller_not_running_as_root_is_refused_unless_told_otherwise() {
    let tree = tree();
    // SAFETY: `geteuid` cannot fail and touches nothing.
    let root = unsafe { libc::geteuid() } == 0;
    let refused = refusal(tree.path(), false);
    if root {
        assert_eq!(refused, Ok(()));
    } else {
        assert_eq!(
            refused.unwrap_err(),
            "chroot connection requires running as root. You can override this check with the `disable_root_check` option."
        );
    }
    assert_eq!(refusal(tree.path(), true), Ok(()));
}

#[test]
fn the_command_line_is_chroot_then_the_tree_then_the_executable() {
    let tree = tree();
    let dir = tree.path().to_str().unwrap();
    let target = target(
        dir,
        json!({
            "ansible_chroot_exe": "/usr/sbin/chroot",
            "ansible_executable": "/bin/bash",
            "ansible_chroot_disable_root_check": true,
        }),
    )
    .unwrap();
    assert_eq!(target.prefix, ["/usr/sbin/chroot", dir]);
    assert_eq!(target.shell, "/bin/bash");
    use super::super::reach::Reach;
    assert_eq!(
        target.command_line("echo hi", false),
        ["/usr/sbin/chroot", dir, "/bin/bash", "-c", "echo hi"]
    );
}
