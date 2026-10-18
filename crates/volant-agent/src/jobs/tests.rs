// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

#[test]
fn a_leading_tilde_is_the_home_directory_and_nothing_else_is_touched() {
    let home = || Some(std::ffi::OsString::from("/home/u"));
    assert_eq!(expand_under("~", home()), PathBuf::from("/home/u"));
    assert_eq!(
        expand_under("~/.ansible_async", home()),
        PathBuf::from("/home/u/.ansible_async")
    );
    assert_eq!(expand_under("~bob/x", home()), PathBuf::from("~bob/x"));
    assert_eq!(
        expand_under("/var/jobs", home()),
        PathBuf::from("/var/jobs")
    );
    assert_eq!(expand_under("~/x", None), PathBuf::from("~/x"));
}

#[rstest::rstest]
#[case(1, 1)]
#[case(2, 2)]
#[case(5, 5)]
#[case(7, 10)]
#[case(10, 10)]
#[case(11, 15)]
fn a_job_is_killed_where_the_reference_kills_it(#[case] limit: u64, #[case] secs: u64) {
    assert_eq!(kill_after(limit), Duration::from_secs(secs));
}

#[test]
fn a_status_is_replaced_whole() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("j1.2");
    write_status(&path, &json!({"started": true})).unwrap();
    write_status(&path, &json!({"rc": 0, "cmd": ["a", "b"]})).unwrap();
    let read = std::fs::read_to_string(&path).unwrap();
    assert_eq!(read, r#"{"rc": 0, "cmd": ["a", "b"]}"#);
    assert!(!dir.path().join("j1.2.tmp").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let jobs = dir.path().join("jobs");
        private_dir(&jobs).unwrap();
        let mode = std::fs::metadata(&jobs).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
