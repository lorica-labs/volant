// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end runs over `community.general.chroot`. Ignored by default: they run as root, with
//! community.general on `ANSIBLE_COLLECTIONS_PATH`, `VOLANT_AGENT_DIR` holding the agent for this
//! machine, and `VOLANT_CHROOT_TEST_TREE` naming a tree with a shell and the usual tools, such as
//! an Alpine root filesystem. Each test works on a copy of it.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A copy of the test tree, removed when the test ends.
struct Tree(tempfile::TempDir);

impl Tree {
    fn copy() -> Tree {
        let source = std::env::var("VOLANT_CHROOT_TEST_TREE")
            .expect("VOLANT_CHROOT_TEST_TREE names a tree with a shell");
        let dir = tempfile::tempdir().unwrap();
        let out = Command::new("cp")
            .args(["-a", &format!("{source}/."), dir.path().to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Tree(dir)
    }

    fn path(&self) -> &Path {
        self.0.path()
    }
}

fn play(dir: &Path, host: &Path, vars: &str, tasks: &str) -> Output {
    std::fs::write(
        dir.join("inventory.yml"),
        format!(
            "all:\n  hosts:\n    {}: {{ansible_connection: chroot{vars}}}\n",
            host.display()
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("site.yml"),
        format!("- hosts: all\n  gather_facts: false\n  tasks:\n{tasks}"),
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(["playbook", "-i", "inventory.yml", "site.yml"])
        .current_dir(dir)
        .output()
        .expect("volant runs")
}

const WHO_AM_I: &str = "    - ansible.builtin.command: id -un\n      register: who\n    - ansible.builtin.debug:\n        var: who.stdout\n";

#[test]
#[ignore = "needs root, community.general and a tree"]
fn chroot_runs_a_play_in_a_tree() {
    let tree = Tree::copy();
    let work = tempfile::tempdir().unwrap();
    let out = play(work.path(), tree.path(), "", WHO_AM_I);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("\"who.stdout\": \"root\""), "{stdout}");
    assert!(
        stdout.contains("ok=2    changed=1    unreachable=0    failed=0"),
        "{stdout}"
    );
}

/// A second run finds the agent the first one cached and uploads nothing: an upload writes a new
/// copy and renames it over the old one, so the inode and the modification time stay as they were.
#[test]
#[ignore = "needs root, community.general and a tree"]
fn chroot_reuses_the_cached_agent_on_a_second_run() {
    use std::os::unix::fs::MetadataExt;
    let tree = Tree::copy();
    let work = tempfile::tempdir().unwrap();
    let cached = || {
        let dir = std::fs::read_dir(tree.path().join("root/.ansible/tmp"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("volant-agent-"))
            })
            .expect("a cached agent");
        let meta = dir.join("volant-agent").metadata().unwrap();
        (meta.ino(), meta.mtime())
    };
    let mut before = None;
    for _ in 0..2 {
        let out = play(work.path(), tree.path(), "", WHO_AM_I);
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        before.get_or_insert_with(cached);
    }
    assert_eq!(
        Some(cached()),
        before,
        "the second run uploaded the agent again"
    );
}

/// The upload runs inside the tree, so a `remote_tmp` linking outside it names a path that is not
/// there: the upload fails, and nothing outside the tree is written.
#[test]
#[ignore = "needs root, community.general and a tree"]
fn chroot_writes_nothing_outside_the_tree() {
    let tree = Tree::copy();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tree.path().join("root")).unwrap();
    let link: PathBuf = tree.path().join("root/.ansible");
    let _ = std::fs::remove_dir_all(&link);
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    let work = tempfile::tempdir().unwrap();
    let out = play(work.path(), tree.path(), "", WHO_AM_I);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("UNREACHABLE!"), "{stdout}");
    assert_eq!(
        std::fs::read_dir(outside.path()).unwrap().count(),
        0,
        "the link's target outside the tree was written to"
    );
}

#[test]
#[ignore = "needs root, community.general and a tree"]
fn chroot_refuses_what_is_no_tree_and_fails_the_task() {
    let work = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    for (host, said) in [
        (
            Path::new("/nonexistent"),
            "/nonexistent is not a directory".to_string(),
        ),
        (
            empty.path(),
            format!(
                "[Errno 2] No such file or directory: '{}/bin/sh'",
                empty.path().display()
            ),
        ),
    ] {
        let out = play(work.path(), host, "", "    - ansible.builtin.ping:\n");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let line = stdout
            .lines()
            .find(|line| line.starts_with("fatal: ["))
            .unwrap_or_else(|| panic!("no failure for {}: {stdout}", host.display()));
        assert!(line.contains("FAILED!"), "{line}");
        assert!(line.contains(&said), "{line}");
    }
}
