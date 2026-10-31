// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant shims`: the links it makes and the lines it prints, through the real binary.
#![cfg(unix)]

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Command, Output};

use rstest::rstest;
use tempfile::TempDir;

const VOLANT: &str = env!("CARGO_BIN_EXE_volant");

fn shims(args: &[&str], path: &str) -> Output {
    Command::new(VOLANT)
        .arg("shims")
        .args(args)
        .env("PATH", path)
        .env_remove("VOLANT_BIN_DIR")
        .output()
        .expect("binary runs")
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("utf-8")
}

fn real_program(dir: &Path, name: &str) {
    let path = dir.join(name);
    std::fs::write(&path, "#!/bin/sh\n").expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

#[test]
fn install_links_all_nine_names_then_says_so_again() {
    let dir = TempDir::new().expect("dir");
    let d = dir.path().to_str().expect("utf-8");
    let out = shims(&["install", "--dir", d], "/usr/bin:/bin");
    assert_eq!(out.status.code(), Some(0));
    let text = stdout(&out);
    assert_eq!(text.lines().count(), 9, "{text}");
    assert!(text.starts_with("ansible: installed\n"), "{text}");
    assert!(text.ends_with("ansible-vault: installed\n"), "{text}");
    let link = std::fs::read_link(dir.path().join("ansible-vault")).expect("link");
    assert_eq!(link, Path::new(VOLANT));

    let out = shims(&["install", "--dir", d, "ansible-vault"], "/usr/bin:/bin");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "ansible-vault: already installed\n");
}

#[test]
fn the_link_is_relative_when_the_directory_holds_volant() {
    let dir = TempDir::new().expect("dir");
    symlink(VOLANT, dir.path().join("volant")).expect("link");
    let d = dir.path().to_str().expect("utf-8");
    let out = shims(
        &["install", "--dir", d, "ansible-playbook"],
        "/usr/bin:/bin",
    );
    assert_eq!(stdout(&out), "ansible-playbook: installed\n");
    let link = std::fs::read_link(dir.path().join("ansible-playbook")).expect("link");
    assert_eq!(link, Path::new("volant"));
    let shim = Command::new(dir.path().join("ansible-playbook"))
        .arg("--version")
        .output()
        .expect("runs");
    let direct = Command::new(VOLANT)
        .args(["playbook", "--version"])
        .output()
        .expect("runs");
    assert!(shim.status.success());
    assert_eq!(shim.stdout, direct.stdout);
}

#[test]
fn a_file_in_the_way_is_kept_unless_forced() {
    let dir = TempDir::new().expect("dir");
    let d = dir.path().to_str().expect("utf-8");
    let file = dir.path().join("ansible-vault");
    std::fs::write(&file, "mine").expect("write");

    let out = shims(&["install", "--dir", d, "ansible-vault"], "/usr/bin:/bin");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stdout(&out),
        format!(
            "ansible-vault: not replaced, {} exists and is not a Volant link\n",
            file.display()
        )
    );
    assert_eq!(std::fs::read_to_string(&file).expect("read"), "mine");

    let out = shims(
        &["install", "--dir", d, "--force", "ansible-vault"],
        "/usr/bin:/bin",
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "ansible-vault: replaced\n");
    assert!(std::fs::symlink_metadata(&file).expect("meta").is_symlink());
}

#[rstest]
#[case::real_earlier(true, "still runs first, {dir} comes later in PATH")]
#[case::real_later(false, "is hidden by this link, {dir} comes first in PATH")]
fn a_real_program_on_path_is_named(#[case] real_first: bool, #[case] tail: &str) {
    let dir = TempDir::new().expect("dir");
    let real = TempDir::new().expect("real");
    real_program(real.path(), "ansible-vault");
    let (d, r) = (dir.path().display().to_string(), real.path().display());
    let path = if real_first {
        format!("{r}:{d}")
    } else {
        format!("{d}:{r}")
    };
    let out = shims(&["install", "--dir", &d, "ansible-vault"], &path);
    let found = real.path().join("ansible-vault");
    let tail = tail.replace("{dir}", &d);
    assert_eq!(
        stdout(&out),
        format!(
            "ansible-vault: installed. note: {} {tail}
",
            found.display()
        )
    );
}

#[test]
fn remove_deletes_links_and_leaves_the_rest() {
    let dir = TempDir::new().expect("dir");
    let d = dir.path().to_str().expect("utf-8");
    shims(&["install", "--dir", d, "ansible-vault"], "/usr/bin:/bin");
    std::fs::write(dir.path().join("ansible-doc"), "mine").expect("write");

    let out = shims(&["remove", "--dir", d, "ansible-vault"], "/usr/bin:/bin");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "ansible-vault: removed\n");
    assert!(!dir.path().join("ansible-vault").exists());

    let out = shims(
        &["remove", "--dir", d, "ansible-vault", "ansible-doc"],
        "/usr/bin:/bin",
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stdout(&out),
        "ansible-vault: not installed\nansible-doc: not a Volant link, left alone\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("ansible-doc")).expect("read"),
        "mine"
    );
}

#[test]
fn a_missing_directory_and_an_unknown_name_are_refused() {
    let out = shims(
        &["install", "--dir", "/nonexistent/volant-shims"],
        "/usr/bin",
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "ERROR! /nonexistent/volant-shims is not a directory\n"
    );
    let out = shims(&["install", "ansible-test"], "/usr/bin");
    assert_eq!(out.status.code(), Some(2));
}

/// A program started under an ansible-core name refuses a locale it cannot set, or one that is
/// not UTF-8, before it reads its arguments; a C locale left to Python's coercion runs.
#[rstest]
#[case::unknown(
    Some("ham_sandwich"),
    1,
    "ERROR: Ansible could not initialize the preferred locale: unsupported locale setting\n"
)]
#[case::c(
    Some("C"),
    1,
    "ERROR: Ansible requires the locale encoding to be UTF-8; Detected None.\n"
)]
#[case::c_utf8(Some("C.UTF-8"), 0, "")]
#[case::unset(None, 0, "")]
fn a_tool_name_checks_the_locale(#[case] all: Option<&str>, #[case] code: i32, #[case] err: &str) {
    let dir = TempDir::new().expect("dir");
    let ansible = dir.path().join("ansible");
    symlink(VOLANT, &ansible).expect("link");
    let mut command = Command::new(&ansible);
    command
        .arg("--version")
        .env_remove("LANG")
        .env_remove("LC_CTYPE");
    match all {
        Some(all) => command.env("LC_ALL", all),
        None => command.env_remove("LC_ALL"),
    };
    let out = command.output().expect("binary runs");
    assert_eq!(out.status.code(), Some(code));
    assert_eq!(String::from_utf8_lossy(&out.stderr), err);
}
