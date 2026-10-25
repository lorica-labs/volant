// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use super::{UNSAFE, place};

fn uid() -> u32 {
    // SAFETY: `getuid` cannot fail and touches nothing.
    unsafe { libc::getuid() }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().mode() & 0o7777
}

fn dir_name() -> String {
    format!("volant-agent-{}-{}", env!("CARGO_PKG_VERSION"), uid())
}

#[test]
fn the_agent_lands_in_its_directory_with_the_script_s_modes() {
    let scratch = tempfile::tempdir().unwrap();
    let rt = scratch.path().join("tmp/deeper");
    let program = place(rt.to_str().unwrap()).unwrap_or_else(|r| panic!("{}", r.message));
    assert_eq!(program, rt.join(dir_name()).join("volant-agent"));
    assert_eq!(mode(&program), 0o755);
    assert_eq!(mode(program.parent().unwrap()), 0o700);
    assert_eq!(
        fs::read(&program).unwrap(),
        fs::read(std::env::current_exe().unwrap()).unwrap()
    );
}

#[test]
fn a_tilde_is_this_user_s_home() {
    let home = tempfile::tempdir().unwrap();
    // nextest gives each test a process of its own, so no other thread reads the variable.
    unsafe { std::env::set_var("HOME", home.path()) };
    let program = place("~/.ansible/tmp").unwrap_or_else(|r| panic!("{}", r.message));
    assert!(
        program.starts_with(home.path().join(".ansible/tmp")),
        "{}",
        program.display()
    );
}

#[test]
fn a_directory_of_this_user_left_too_open_is_made_again() {
    let scratch = tempfile::tempdir().unwrap();
    let dir = scratch.path().join(dir_name());
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("volant-agent"), b"stale").unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
    let program =
        place(scratch.path().to_str().unwrap()).unwrap_or_else(|r| panic!("{}", r.message));
    assert_eq!(mode(&dir), 0o700);
    assert_ne!(fs::read(program).unwrap(), b"stale");
}

#[test]
fn a_remote_tmp_others_can_write_without_the_sticky_bit_is_refused() {
    let scratch = tempfile::tempdir().unwrap();
    fs::set_permissions(scratch.path(), fs::Permissions::from_mode(0o777)).unwrap();
    let refusal = place(scratch.path().to_str().unwrap()).expect_err("refused");
    assert_eq!(refusal.code, UNSAFE);
    assert!(
        refusal.message.contains("other users could replace"),
        "{}",
        refusal.message
    );
    fs::set_permissions(scratch.path(), fs::Permissions::from_mode(0o1777)).unwrap();
    assert!(
        place(scratch.path().to_str().unwrap()).is_ok(),
        "a sticky directory is fine"
    );
}

#[test]
fn the_other_versions_of_this_user_are_swept() {
    let scratch = tempfile::tempdir().unwrap();
    let old = scratch.path().join(format!("volant-agent-0.0.1-{}", uid()));
    fs::create_dir(&old).unwrap();
    fs::set_permissions(&old, fs::Permissions::from_mode(0o700)).unwrap();
    place(scratch.path().to_str().unwrap()).unwrap_or_else(|r| panic!("{}", r.message));
    assert!(!old.exists());
}

#[test]
fn another_user_s_home_is_not_expanded() {
    let refusal = place("~root/tmp").expect_err("refused");
    assert!(
        refusal.message.contains("another user's home"),
        "{}",
        refusal.message
    );
}
