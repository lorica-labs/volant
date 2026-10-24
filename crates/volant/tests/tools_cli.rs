// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant config` and `volant galaxy` against the script's own `main()` under the same
//! interpreter: the same bytes and exit code, with no network.
#![cfg(unix)]

#[path = "common/process.rs"]
mod process;
#[path = "common/python.rs"]
mod python;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

const VOLANT: &str = env!("CARGO_BIN_EXE_volant");

fn within(command: &mut Command) -> Output {
    process::run_within(command, Duration::from_secs(120), None)
}

/// A scratch directory that is both the working directory and `HOME` of a run.
fn scratch(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    for (name, text) in files {
        std::fs::write(dir.path().join(name), text).expect("write a file");
    }
    dir
}

fn isolate(command: &mut Command, dir: &Path, python: &Path) {
    command
        .current_dir(dir)
        .env("HOME", dir)
        .env("VOLANT_PYTHON", python)
        .env("ANSIBLE_FORKS", "9")
        .env_remove("ANSIBLE_CONFIG")
        .env_remove("ANSIBLE_COLLECTIONS_PATH")
        .env_remove("ANSIBLE_HOME");
}

/// The tool's `main()` started as `argv0`, as the script starts it.
fn reference(python: &Path, tool: &str, dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(python);
    command
        .args([
            "-P",
            "-c",
            "import sys, importlib
tool = sys.argv[1]
sys.argv = sys.argv[2:]
importlib.import_module('ansible.cli.' + tool).main()",
            tool,
            VOLANT,
        ])
        .args(args);
    isolate(&mut command, dir, python);
    within(&mut command)
}

fn ours(python: &Path, sub: &str, dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(VOLANT);
    command.arg(sub).args(args);
    isolate(&mut command, dir, python);
    within(&mut command)
}

/// What a run printed, with its scratch directory named `<dir>`.
fn text(out: &Output, dir: &Path) -> (Option<i32>, String, String) {
    let clean =
        |bytes: &[u8]| String::from_utf8_lossy(bytes).replace(&dir.display().to_string(), "<dir>");
    (out.status.code(), clean(&out.stdout), clean(&out.stderr))
}

fn same_as_reference(sub: &str, files: &[(&str, &str)], args: &[&str]) {
    let Some(interpreter) = python::reference_python() else {
        return;
    };
    let (a, b) = (scratch(files), scratch(files));
    let theirs = reference(&interpreter, sub, a.path(), args);
    let mine = ours(&interpreter, sub, b.path(), args);
    let (theirs, mine) = (text(&theirs, a.path()), text(&mine, b.path()));
    assert_eq!(mine, theirs, "{sub} {args:?}");
    assert!(
        !mine.1.is_empty() || !mine.2.is_empty(),
        "{sub} {args:?} printed nothing"
    );
}

const FORKS: &[(&str, &str)] = &[("ansible.cfg", "[defaults]\nforks=7\n")];
const VOLANT_SECTION: &[(&str, &str)] = &[("ansible.cfg", "[volant]\nruntime=system\n")];

#[test]
fn config_prints_what_ansible_config_prints() {
    same_as_reference("config", &[], &["dump", "--only-changed"]);
    same_as_reference("config", FORKS, &["dump", "--only-changed"]);
    same_as_reference("config", &[], &["list"]);
    same_as_reference("config", &[], &["view"]);
    same_as_reference("config", FORKS, &["view"]);
    same_as_reference("config", &[], &["init", "--disabled"]);
    same_as_reference("config", &[], &["--bogus"]);
}

/// The `[volant]` section is Volant's own and `validate` reports it, as the reference does.
#[test]
fn config_validate_reports_the_volant_section() {
    same_as_reference("config", VOLANT_SECTION, &["validate", "-c", "ansible.cfg"]);
    same_as_reference("config", FORKS, &["validate", "-c", "ansible.cfg"]);
}

#[test]
fn galaxy_prints_what_ansible_galaxy_prints() {
    same_as_reference("galaxy", &[], &["collection", "list"]);
    same_as_reference("galaxy", &[], &["role", "init", "myrole"]);
    same_as_reference("galaxy", &[], &["role", "list"]);
    same_as_reference("galaxy", &[], &["--bogus"]);
}

/// A collection built here installs from its tarball into `-p` and is listed there.
#[test]
fn a_collection_installs_from_a_tarball_and_is_listed() {
    let Some(interpreter) = python::reference_python() else {
        return;
    };
    let dir = scratch(&[]);
    let src: PathBuf = dir.path().join("demo");
    std::fs::create_dir_all(src.join("plugins")).expect("source tree");
    std::fs::write(
        src.join("galaxy.yml"),
        "namespace: acme\nname: demo\nversion: 1.0.0\nreadme: README.md\nauthors: [a]\n",
    )
    .expect("galaxy.yml");
    std::fs::write(src.join("README.md"), "demo\n").expect("readme");
    let built = ours(
        &interpreter,
        "galaxy",
        dir.path(),
        &["collection", "build", "demo"],
    );
    assert_eq!(built.status.code(), Some(0), "{built:?}");
    let target = dir.path().join("out");
    let target = target.to_str().expect("utf-8 path");
    let install = ours(
        &interpreter,
        "galaxy",
        dir.path(),
        &[
            "collection",
            "install",
            "acme-demo-1.0.0.tar.gz",
            "-p",
            target,
        ],
    );
    assert_eq!(install.status.code(), Some(0), "{install:?}");
    let listed = ours(
        &interpreter,
        "galaxy",
        dir.path(),
        &["collection", "list", "-p", target],
    );
    let out = String::from_utf8_lossy(&listed.stdout);
    assert!(out.contains("acme.demo") && out.contains("1.0.0"), "{out}");
}
