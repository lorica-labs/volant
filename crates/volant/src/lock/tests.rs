// SPDX-License-Identifier: GPL-3.0-or-later
use std::path::Path;

use super::{Entry, Lock, Runtime, collections, differences, parse, render, roles, tree_hash};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

/// A collections path with `demo.base` 1.0.0 and a roles path with `web`.
fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let base = dir.path().join("cols/ansible_collections/demo/base");
    write(
        &base.join("MANIFEST.json"),
        r#"{"collection_info": {"namespace": "demo", "name": "base", "version": "1.0.0"}}"#,
    );
    write(&base.join("plugins/modules/thing.py"), "print('thing')\n");
    write(
        &base.join("plugins/modules/__pycache__/thing.cpython-312.pyc"),
        "x",
    );
    let web = dir.path().join("roles/web");
    write(&web.join("meta/main.yml"), "dependencies: []\n");
    write(&web.join("tasks/main.yml"), "- debug: msg=hi\n");
    write(
        &web.join("meta/.galaxy_install_info"),
        "install_date: 'Mon Oct 27 10:00:00 2026'\nversion: 1.1.0\n",
    );
    dir
}

fn lock_of(dir: &Path) -> Lock {
    Lock {
        volant: "0.4.2".into(),
        core_version: "2.19".into(),
        runtime: Some(Runtime {
            python: "3.13.16".into(),
            ansible_core: "2.19.12".into(),
            sha256: "ab".repeat(32),
        }),
        collections: collections(&[dir.join("cols")]),
        roles: roles(&[dir.join("roles")]),
    }
}

/// The same tree gives the same file, byte for byte, and the file reads back as written.
#[test]
fn a_tree_locks_to_the_same_bytes_every_time() {
    let dir = tree();
    let first = render(&lock_of(dir.path()));
    let second = render(&lock_of(dir.path()));
    assert_eq!(first, second);
    assert_eq!(parse(&first).expect("it reads back"), lock_of(dir.path()));
    let lock = lock_of(dir.path());
    assert_eq!(lock.collections[0].name, "demo.base");
    assert_eq!(lock.collections[0].version, "1.0.0");
    assert_eq!(lock.roles[0].name, "web");
    assert_eq!(lock.roles[0].version, "1.1.0");
    assert!(first.starts_with("format = 1\nvolant = \"0.4.2\"\ncore_version = \"2.19\"\n"));
}

/// Neither `*.pyc` files nor a role's install date change a hash; a file's content does.
#[test]
fn a_hash_follows_the_content_only() {
    let dir = tree();
    let base = dir.path().join("cols/ansible_collections/demo/base");
    let web = dir.path().join("roles/web");
    let (collection, role) = (tree_hash(&base, &[]), roles(&[dir.path().join("roles")]));
    write(&base.join("plugins/modules/__pycache__/other.pyc"), "y");
    write(
        &web.join("meta/.galaxy_install_info"),
        "install_date: 'Tue Oct 28 10:00:00 2026'\nversion: 1.1.0\n",
    );
    assert_eq!(tree_hash(&base, &[]), collection);
    assert_eq!(roles(&[dir.path().join("roles")]), role);
    write(&base.join("plugins/modules/thing.py"), "print('changed')\n");
    assert_ne!(tree_hash(&base, &[]), collection);
}

/// A changed file, a missing collection and an extra one are each named.
#[test]
fn every_difference_is_named() {
    let dir = tree();
    let locked = lock_of(dir.path());
    let base = dir.path().join("cols/ansible_collections/demo/base");
    write(&base.join("plugins/modules/thing.py"), "print('changed')\n");
    let mut now = lock_of(dir.path());
    assert_eq!(
        differences(&locked, &now),
        ["collection demo.base 1.0.0: its files differ from the lock"]
    );
    now.collections.push(Entry {
        name: "demo.extra".into(),
        version: "2.0.0".into(),
        sha256: "00".repeat(32),
    });
    now.collections.remove(0);
    now.runtime = None;
    now.volant = "0.4.3".into();
    assert_eq!(
        differences(&locked, &now),
        [
            "volant: 0.4.2 in the lock, 0.4.3 running",
            "runtime: in the lock, not installed",
            "collection demo.base: in the lock, not installed",
            "collection demo.extra 2.0.0: installed, not in the lock",
        ]
    );
}

/// A file of another shape is refused with the reason.
#[test]
fn a_lock_that_is_not_one_is_refused() {
    assert_eq!(parse("format = 2\n").unwrap_err(), "`format` is not 1");
    assert!(
        parse("format = 1\nvolant = 3\n")
            .unwrap_err()
            .contains("volant")
    );
    assert!(parse("not toml [").is_err());
}
