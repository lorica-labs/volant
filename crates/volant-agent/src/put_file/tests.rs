// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use std::os::unix::fs::PermissionsExt;

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

/// Written once with the default mode, unchanged the second time, replaced when it differs.
#[test]
fn a_file_is_replaced_by_checksum_and_gets_its_mode() {
    let dir = tempfile::tempdir().unwrap();
    let staged = dir.path().join("staged");
    let at = dir.path().join("out");
    fs::write(&staged, b"one").unwrap();
    assert!(put(&staged, &at, None).unwrap());
    assert_eq!(fs::read(&at).unwrap(), b"one");
    assert_eq!(mode(&at), DEFAULT_MODE);
    assert!(!put(&staged, &at, Some(0o600)).unwrap());
    fs::write(&staged, b"two").unwrap();
    assert!(put(&staged, &at, Some(0o640)).unwrap());
    assert_eq!(fs::read(&at).unwrap(), b"two");
    assert_eq!(mode(&at), 0o640);
    let left: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
    assert_eq!(left.len(), 2, "no temporary file is left behind");
}

/// A mode alone is a change; no mode keeps the one the file has, in both branches.
#[test]
fn a_mode_change_counts_and_no_mode_keeps_the_existing_one() {
    let dir = tempfile::tempdir().unwrap();
    let staged = dir.path().join("staged");
    let at = dir.path().join("out");
    fs::write(&staged, b"same").unwrap();
    put(&staged, &at, Some(0o644)).unwrap();
    assert!(put(&staged, &at, Some(0o4755)).unwrap(), "mode alone");
    assert_eq!(mode(&at), 0o4755);
    assert!(!put(&staged, &at, Some(0o4755)).unwrap());
    assert!(!put(&staged, &at, None).unwrap());
    assert_eq!(mode(&at), 0o4755);
    fs::write(&staged, b"new").unwrap();
    assert!(put(&staged, &at, None).unwrap());
    assert_eq!(mode(&at), 0o4755, "a new content keeps the mode too");
}

#[test]
fn a_directory_is_refused_and_a_symlink_is_replaced_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let staged = dir.path().join("staged");
    fs::write(&staged, b"data").unwrap();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    let err = put(&staged, &sub, None).unwrap_err();
    assert_eq!(err.to_string(), "the path is a directory");
    assert!(sub.is_dir());

    let target = dir.path().join("target");
    fs::write(&target, b"data").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(put(&staged, &link, Some(0o600)).unwrap());
    assert!(!fs::symlink_metadata(&link).unwrap().is_symlink());
    assert_eq!(fs::read(&link).unwrap(), b"data");
    assert_eq!(mode(&link), 0o600);
    assert_eq!(mode(&target), 0o644, "the target is untouched");
}

#[test]
fn a_relative_path_and_a_missing_directory_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let staged = dir.path().join("staged");
    fs::write(&staged, b"x").unwrap();
    let err = put(&staged, Path::new("out"), None).unwrap_err();
    assert_eq!(err.to_string(), "the path is not absolute");
    assert!(put(&staged, &dir.path().join("no/such/out"), None).is_err());
}
