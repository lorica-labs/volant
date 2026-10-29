// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant lock` and a run against `volant.lock`: `--check` names each difference, and a playbook
//! warns, or with `--locked` stops before it connects.
#![cfg(unix)]

#[path = "common/process.rs"]
mod process;

use std::path::Path;
use std::process::Output;
use std::time::Duration;

/// A project with `ansible.cfg`, a collection, a role, an inventory whose host cannot be
/// reached, and a play that would connect to it.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let write = |rel: &str, text: &str| {
        let path = dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write(
        "ansible.cfg",
        "[defaults]\ncollections_path = ./cols\nroles_path = ./roles\n",
    );
    write(
        "cols/ansible_collections/demo/base/MANIFEST.json",
        r#"{"collection_info": {"namespace": "demo", "name": "base", "version": "1.0.0"}}"#,
    );
    write(
        "cols/ansible_collections/demo/base/plugins/modules/thing.py",
        "print('thing')\n",
    );
    write("roles/web/meta/main.yml", "dependencies: []\n");
    write(
        "hosts.ini",
        "box ansible_host=127.0.0.1 ansible_port=1 ansible_connection=ssh\n",
    );
    write(
        "site.yml",
        "- hosts: box\n  gather_facts: false\n  tasks:\n    - command: 'true'\n",
    );
    dir
}

fn volant(dir: &Path, args: &[&str]) -> (Option<i32>, String) {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_volant"));
    command
        .args(args)
        .current_dir(dir)
        .env("HOME", dir)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env_remove("ANSIBLE_CONFIG");
    let out: Output = process::run_within(&mut command, Duration::from_secs(120), None);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code(), text)
}

/// The lock is written twice to the same bytes, and `--check` passes until a file changes.
#[test]
fn check_names_a_changed_collection() {
    let dir = project();
    assert_eq!(volant(dir.path(), &["lock"]).0, Some(0));
    let first = std::fs::read(dir.path().join("volant.lock")).unwrap();
    assert_eq!(volant(dir.path(), &["lock"]).0, Some(0));
    assert_eq!(
        std::fs::read(dir.path().join("volant.lock")).unwrap(),
        first
    );
    assert_eq!(
        volant(dir.path(), &["lock", "--check"]),
        (Some(0), String::new())
    );
    std::fs::write(
        dir.path()
            .join("cols/ansible_collections/demo/base/plugins/modules/thing.py"),
        "print('changed')\n",
    )
    .unwrap();
    let (code, text) = volant(dir.path(), &["lock", "--check"]);
    assert_eq!(code, Some(1), "{text}");
    assert_eq!(
        text,
        "collection demo.base 1.0.0: its files differ from the lock\n"
    );
}

/// A run that differs from the lock warns and goes on; with `--locked` it is refused with exit 4
/// and the play never starts, so the host is never contacted.
#[test]
fn a_locked_run_stops_before_it_connects() {
    let dir = project();
    assert_eq!(volant(dir.path(), &["lock"]).0, Some(0));
    std::fs::remove_dir_all(dir.path().join("roles/web")).unwrap();

    let (_, text) = volant(dir.path(), &["playbook", "-i", "hosts.ini", "site.yml"]);
    assert!(
        text.contains("[WARNING]: volant.lock: role web: in the lock, not installed"),
        "{text}"
    );
    assert!(text.contains("PLAY [box]"), "{text}");

    let (code, text) = volant(
        dir.path(),
        &["playbook", "--locked", "-i", "hosts.ini", "site.yml"],
    );
    assert_eq!(code, Some(4), "{text}");
    assert!(text.contains("differs from"), "{text}");
    assert!(
        text.contains("  role web: in the lock, not installed"),
        "{text}"
    );
    assert!(!text.contains("PLAY ["), "{text}");
    assert!(!text.contains("UNREACHABLE"), "{text}");
}
