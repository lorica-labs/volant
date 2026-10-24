// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use std::os::unix::fs::PermissionsExt as _;

fn mode(path: &Path, bits: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits)).unwrap();
}

/// A fake binary to link to, in a scratch directory.
fn scratch() -> (tempfile::TempDir, PathBuf) {
    let work = tempfile::tempdir().unwrap();
    let volant = work.path().join("volant");
    std::fs::write(&volant, "").unwrap();
    (work, volant)
}

/// A link is made once, left alone while it is right, and replaced when the binary moved.
#[test]
fn a_link_follows_the_binary() {
    let (work, a) = scratch();
    let (b, path) = (work.path().join("b"), work.path().join("ansible"));
    link(&path, &a).unwrap();
    assert_eq!(std::fs::read_link(&path).unwrap(), a);
    link(&path, &a).unwrap();
    link(&path, &b).unwrap();
    assert_eq!(std::fs::read_link(&path).unwrap(), b);
    assert_eq!(std::fs::read_dir(work.path()).unwrap().count(), 2);
}

/// The directory is made private, holds both links, and a stale file of the same name is replaced.
#[test]
fn the_directory_is_private_and_a_stale_file_is_replaced() {
    let (work, volant) = scratch();
    let root = work.path().join("cache");
    let dir = prepare(&root, &volant).unwrap();
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    std::fs::remove_file(dir.join("ansible")).unwrap();
    std::fs::write(dir.join("ansible"), "#!/bin/sh\n").unwrap();
    prepare(&root, &volant).unwrap();
    for name in CHILDREN {
        assert_eq!(
            std::fs::read_link(dir.join(name)).unwrap(),
            volant,
            "{name}"
        );
    }
}

/// A link in place of `pull` or of the cache, or a `pull` others can write: refused. A cache that
/// only the group can write is tightened.
#[test]
fn a_planted_directory_is_refused() {
    let (work, volant) = scratch();
    let elsewhere = work.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();

    let root = work.path().join("one");
    std::fs::create_dir(&root).unwrap();
    std::os::unix::fs::symlink(&elsewhere, root.join("pull")).unwrap();
    let err = prepare(&root, &volant).unwrap_err();
    assert!(err.contains("symbolic link"), "{err}");
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);

    let root = work.path().join("two");
    std::fs::create_dir(&root).unwrap();
    mode(&root, 0o775);
    prepare(&root, &volant).unwrap();
    assert_eq!(
        std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o755
    );

    let root = work.path().join("three");
    prepare(&root, &volant).unwrap();
    mode(&root.join("pull"), 0o720);
    assert!(prepare(&root, &volant).is_err());

    let aside = work.path().join("four");
    std::os::unix::fs::symlink(&elsewhere, &aside).unwrap();
    let err = prepare(&aside, &volant).unwrap_err();
    assert!(err.contains("symbolic link"), "{err}");
}

/// A binary that is gone, as after an in-place upgrade, is named rather than linked to.
#[test]
fn a_missing_binary_is_refused() {
    let (work, _) = scratch();
    let err = prepare(
        &work.path().join("cache"),
        &work.path().join("volant (deleted)"),
    )
    .unwrap_err();
    assert!(err.contains("is gone"), "{err}");
}
