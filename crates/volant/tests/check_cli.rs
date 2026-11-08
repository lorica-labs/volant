// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant check` on a fixture repository with one playbook per verdict: the text and JSON
//! reports, the exit codes, and no Python under `--runtime-less`.
//!
//! The reports are compared with `tests/golden/check/`. `VOLANT_BLESS=1` writes them instead.
#![cfg(unix)]

use std::path::{Path, PathBuf};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/check")
}

/// Runs `volant check` in `dir` with nothing from this machine's Ansible setup in reach.
fn check(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> (Option<i32>, String, String) {
    let home = tempfile::tempdir().unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_volant"));
    command
        .arg("check")
        .args(args)
        .current_dir(dir)
        .env("HOME", home.path())
        .env("ANSIBLE_ROLES_PATH", "/nonexistent")
        .env_remove("ANSIBLE_CONFIG")
        .env_remove("VOLANT_PYTHON");
    for (key, value) in envs {
        command.env(key, value);
    }
    let out = command.output().expect("volant runs");
    let shown = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes).replace(&dir.display().to_string(), "<fixture>")
    };
    (out.status.code(), shown(&out.stdout), shown(&out.stderr))
}

fn golden(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/check")
        .join(name);
    if std::env::var_os("VOLANT_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        actual,
        expected,
        "{} differs; VOLANT_BLESS=1 rewrites it",
        path.display()
    );
}

#[test]
fn every_playbook_gets_a_verdict_in_text_and_json() {
    let (code, text, stderr) = check(&fixture(), &["--runtime-less", "-v"], &[]);
    assert_eq!(code, Some(1), "{stderr}");
    golden("check.txt", &text);
    let (code, json, stderr) = check(&fixture(), &["--runtime-less", "--json"], &[]);
    assert_eq!(code, Some(1), "{stderr}");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("one JSON document");
    let mut pretty = serde_json::to_string_pretty(&parsed).unwrap();
    pretty.push('\n');
    golden("check.json", &pretty.replace(volant::VERSION, "<version>"));
}

#[test]
fn unknown_fails_only_under_strict_and_nothing_found_is_3() {
    let dir = fixture();
    assert_eq!(check(&dir, &["templated.yml"], &[]).0, Some(0));
    assert_eq!(check(&dir, &["--strict", "templated.yml"], &[]).0, Some(1));
    assert_eq!(check(&dir, &["site.yml"], &[]).0, Some(0));
    let (code, _, stderr) = check(&dir, &["group_vars"], &[]);
    assert_eq!(code, Some(3));
    assert!(
        stderr.contains("no playbook found in group_vars"),
        "{stderr}"
    );
}

/// The interpreter named by `VOLANT_PYTHON` leaves a mark when it is started: none under
/// `--runtime-less`, one without it, which proves the mark is a fair witness.
#[test]
fn runtime_less_starts_no_python() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = tempfile::tempdir().unwrap();
    let mark = scratch.path().join("started");
    let python = scratch.path().join("python3");
    std::fs::write(
        &python,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", mark.display()),
    )
    .unwrap();
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).unwrap();
    let python = python.display().to_string();
    let envs = [("VOLANT_PYTHON", python.as_str())];
    let (code, out, stderr) = check(&fixture(), &["--runtime-less", "collection.yml"], &envs);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(
        out.contains("--runtime-less starts no ansible-core"),
        "{out}"
    );
    assert!(!mark.exists(), "--runtime-less started {python}");
    let (_, out, _) = check(&fixture(), &["collection.yml"], &envs);
    assert!(out.contains("unknown"), "{out}");
    assert!(mark.exists(), "the check never asked {python}");
}
