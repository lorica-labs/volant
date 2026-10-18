// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use crate::testing::scratch;

use std::os::unix::fs::{MetadataExt, PermissionsExt};

const AGENT: &[u8] = b"#!/bin/sh\necho embedded\n";

/// A second extraction of an agent already in place leaves the file alone.
#[test]
fn extracting_an_agent_already_in_place_writes_nothing() {
    let root = scratch();
    let dir = root.path().join("cache");
    let first = extract(&dir, "volant-agent", AGENT).unwrap();
    let inode = fs::metadata(&first).unwrap().ino();

    let second = extract(&dir, "volant-agent", AGENT).unwrap();
    assert_eq!(second, first);
    assert_eq!(
        fs::metadata(&second).unwrap().ino(),
        inode,
        "the agent was rewritten"
    );
    assert_eq!(fs::read(&second).unwrap(), AGENT);
}

/// A copy that was cut short, or altered without changing its length, is replaced.
#[test]
fn a_short_or_altered_copy_is_written_again() {
    let root = scratch();
    let dir = root.path().join("cache");
    extract(&dir, "volant-agent", AGENT).unwrap();
    let path = dir.join("volant-agent");

    let mut altered = AGENT.to_vec();
    altered[0] ^= 0xff;
    for wrong in [&AGENT[..4], &altered[..]] {
        fs::write(&path, wrong).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        extract(&dir, "volant-agent", AGENT).unwrap();
        assert_eq!(fs::read(&path).unwrap(), AGENT);
    }
}

/// The extracted agent is mode 0755 whatever the umask, and a copy that lost its execute bit is
/// replaced even though its bytes are right.
#[test]
fn the_extracted_agent_is_executable() {
    let root = scratch();
    let dir = root.path().join("cache");
    let path = extract(&dir, "volant-agent", AGENT).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o755);

    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    extract(&dir, "volant-agent", AGENT).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o755);
}

/// A cache directory another account could write is refused, nothing is written into it, and
/// the error gives its mode.
#[test]
fn a_cache_directory_others_can_write_is_refused() {
    let root = scratch();
    let dir = root.path().join("cache");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();

    let err = extract(&dir, "volant-agent", AGENT).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(err.to_string().contains("mode 777"), "{err}");
    assert!(!dir.join("volant-agent").exists());
}

/// A new cache directory is created private to this user.
#[test]
fn a_new_cache_directory_is_private() {
    let root = scratch();
    let dir = root.path().join("a").join("cache");
    // SAFETY: `umask` only swaps this process's file mode mask; nextest runs each test in a
    // process of its own, so no other thread is creating files meanwhile.
    let previous = unsafe { libc::umask(0o002) };
    let result = extract(&dir, "volant-agent", AGENT);
    unsafe { libc::umask(previous) };
    result.unwrap();
    assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
}

/// The cache is named after this controller's version, under `XDG_CACHE_HOME` when that is
/// absolute and under `HOME` otherwise.
#[test]
fn the_cache_directory_follows_xdg_then_home() {
    let version = env!("CARGO_PKG_VERSION");
    let with = |xdg: Option<&str>, home: Option<&str>| {
        // SAFETY: nextest runs each test in a process of its own, and nothing else in this
        // one reads the environment while it changes.
        unsafe {
            for (var, value) in [("XDG_CACHE_HOME", xdg), ("HOME", home)] {
                match value {
                    Some(v) => std::env::set_var(var, v),
                    None => std::env::remove_var(var),
                }
            }
        }
        cache_dir()
    };

    assert_eq!(
        with(Some("/xdg"), Some("/home/u")).unwrap(),
        PathBuf::from(format!("/xdg/volant/agents/{version}"))
    );
    assert_eq!(
        with(Some("relative"), Some("/home/u")).unwrap(),
        PathBuf::from(format!("/home/u/.cache/volant/agents/{version}"))
    );
    assert!(with(None, None).is_err());
}
