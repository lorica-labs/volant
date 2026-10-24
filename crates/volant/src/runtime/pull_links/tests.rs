// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use std::os::unix::fs::PermissionsExt as _;

/// A link is made once, left alone while it is right, and replaced when the binary moved.
#[test]
fn a_link_follows_the_binary() {
    let work = tempfile::tempdir().unwrap();
    let (a, b) = (work.path().join("a"), work.path().join("b"));
    let path = work.path().join("ansible");
    link(&path, &a).unwrap();
    assert_eq!(std::fs::read_link(&path).unwrap(), a);
    link(&path, &a).unwrap();
    link(&path, &b).unwrap();
    assert_eq!(std::fs::read_link(&path).unwrap(), b);
    let leftovers = std::fs::read_dir(work.path()).unwrap().count();
    assert_eq!(leftovers, 1);
}

/// The directory is private to its owner.
#[test]
fn the_directory_is_private() {
    let work = tempfile::tempdir().unwrap();
    let dir = work.path().join("pull");
    crate::agent::embedded::create_private(&dir).unwrap();
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(crate::agent::embedded::check_private(&dir).is_err());
}
