// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant doc` against a stand-in interpreter: what reaches the tool, and what comes back.
#![cfg(unix)]

#[path = "common/process.rs"]
mod process;
#[path = "common/python.rs"]
mod python;

use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;
use std::time::Duration;

/// A script that passes the ansible-core probe, then reports its arguments and ends as
/// `FAKE_END` says: `exit N`, or a signal sent to itself.
const FAKE: &str = "#!/bin/sh
[ \"$3\" = \"import ansible\" ] && exit 0
shift 3
printf '%s\\n' \"$@\"
eval \"$FAKE_END\"
";

fn volant_doc(end: &str, args: &[&str]) -> (std::process::Output, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let python = dir.path().join("python");
    std::fs::write(&python, FAKE).expect("write the stand-in");
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let mut command = Command::new(env!("CARGO_BIN_EXE_volant"));
    command
        .arg("doc")
        .args(args)
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("VOLANT_PYTHON", &python)
        .env("FAKE_END", end)
        .env_remove("ANSIBLE_CONFIG");
    let out = process::run_within(&mut command, Duration::from_secs(60), None);
    (out, dir)
}

/// Everything after `doc` reaches the tool, `-h` and unknown options included, after the tool
/// name and `sys.argv[0]`.
#[test]
fn every_argument_after_doc_reaches_the_tool() {
    let (out, _dir) = volant_doc("exit 0", &["-h", "--bogus", "-t", "keyword", "--", "x"]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "doc");
    assert!(lines[1].ends_with("/volant"), "{lines:?}");
    assert_eq!(lines[2..], ["-h", "--bogus", "-t", "keyword", "--", "x"]);
}

/// A leading `--` is the tool's to read, not clap's.
#[test]
fn a_leading_double_dash_reaches_the_tool() {
    let (out, _dir) = volant_doc("exit 0", &["--", "-l"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(text.lines().skip(2).collect::<Vec<_>>(), ["--", "-l"]);
}

/// The tool's exit code is Volant's, and a signal is `128 + signal`.
#[test]
fn the_exit_code_and_a_signal_pass_through() {
    assert_eq!(volant_doc("exit 3", &[]).0.status.code(), Some(3));
    assert_eq!(volant_doc("kill -KILL $$", &[]).0.status.code(), Some(137));
    assert_eq!(volant_doc("kill -TERM $$", &[]).0.status.code(), Some(143));
}

/// The script's own `main()` under the same interpreter, started as `argv0`.
fn reference(python: &std::path::Path, argv0: &str, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(python);
    command
        .args([
            "-P",
            "-c",
            "import sys
from ansible.cli.doc import main
sys.argv[0] = sys.argv[1]
del sys.argv[1]
main()",
            argv0,
        ])
        .args(args);
    process::run_within(&mut command, Duration::from_secs(120), None)
}

/// `volant doc` prints what the reference prints, bytes and exit code, for the cases of the
/// issue; `--version` differs in the path of the program only.
#[test]
fn the_output_is_the_references_for_the_same_runtime() {
    let Some(interpreter) = python::reference_python() else {
        return;
    };
    let volant = env!("CARGO_BIN_EXE_volant");
    let cases: [&[&str]; 9] = [
        &["ping"],
        &["-l"],
        &["-s", "ping"],
        &["--json", "ping"],
        &["-t", "keyword", "-l"],
        &["nosuch"],
        &["--version"],
        &["--", "-l"],
        &["--bogus"],
    ];
    for args in cases {
        let mut command = Command::new(volant);
        command
            .arg("doc")
            .args(args)
            .env("VOLANT_PYTHON", &interpreter);
        let ours = process::run_within(&mut command, Duration::from_secs(120), None);
        let theirs = reference(&interpreter, volant, args);
        assert_eq!(ours.status.code(), theirs.status.code(), "{args:?}");
        assert_eq!(ours.stdout, theirs.stdout, "{args:?}");
        assert_eq!(ours.stderr, theirs.stderr, "{args:?}");
    }
}
