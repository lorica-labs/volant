// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant config` against the script's own `main()` under the same interpreter: the same bytes
//! and exit code, with no network.
#![cfg(unix)]

#[path = "common/process.rs"]
mod process;
#[path = "common/python.rs"]
mod python;

use std::path::Path;
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

fn isolate(command: &mut Command, dir: &Path, python: &Path, env: &[(&str, &str)]) {
    command
        .current_dir(dir)
        .env("HOME", dir)
        .env("VOLANT_PYTHON", python)
        .envs(env.iter().copied())
        .env_remove("ANSIBLE_FORKS")
        .env_remove("ANSIBLE_CONFIG")
        .env_remove("ANSIBLE_COLLECTIONS_PATH")
        .env_remove("ANSIBLE_HOME");
}

/// The tool's `main()` started as `argv0`, as the script starts it.
fn reference(python: &Path, tool: &str, dir: &Path, env: &[(&str, &str)], args: &[&str]) -> Output {
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
    isolate(&mut command, dir, python, env);
    within(&mut command)
}

fn ours(python: &Path, sub: &str, dir: &Path, args: &[&str]) -> Output {
    started(python, VOLANT, Some(sub), dir, &[], args)
}

/// `volant` (or a link to it) started as `program`, with `sub` before the arguments when given.
fn started(
    python: &Path,
    program: &str,
    sub: Option<&str>,
    dir: &Path,
    env: &[(&str, &str)],
    args: &[&str],
) -> Output {
    let mut command = Command::new(program);
    command.args(sub).args(args);
    isolate(&mut command, dir, python, env);
    within(&mut command)
}

/// What a run printed, with its scratch directory named `<dir>`.
fn text(out: &Output, dir: &Path) -> (Option<i32>, String, String) {
    let clean =
        |bytes: &[u8]| String::from_utf8_lossy(bytes).replace(&dir.display().to_string(), "<dir>");
    (out.status.code(), clean(&out.stdout), clean(&out.stderr))
}

fn same_as_reference(sub: &str, files: &[(&str, &str)], args: &[&str]) {
    same_with_env(sub, files, &[], args);
}

fn same_with_env(sub: &str, files: &[(&str, &str)], env: &[(&str, &str)], args: &[&str]) {
    let Some(interpreter) = python::reference_python() else {
        return;
    };
    let (a, b) = (scratch(files), scratch(files));
    let theirs = reference(&interpreter, sub, a.path(), env, args);
    let mine = started(&interpreter, VOLANT, Some(sub), b.path(), env, args);
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
    same_with_env(
        "config",
        &[],
        &[("ANSIBLE_FORKS", "9")],
        &["dump", "--only-changed"],
    );
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

/// With no `ansible.cfg` setting in the way, the file's `forks` is the value shown, and it names
/// the file.
#[test]
fn a_setting_of_the_file_is_the_one_shown() {
    let Some(interpreter) = python::reference_python() else {
        return;
    };
    let dir = scratch(FORKS);
    let out = ours(
        &interpreter,
        "config",
        dir.path(),
        &["dump", "--only-changed"],
    );
    let (_, shown, _) = text(&out, dir.path());
    assert!(
        shown.contains("DEFAULT_FORKS(<dir>/ansible.cfg) = 7"),
        "{shown}"
    );
}

/// Once the runtime's `collections/` exists, the tool is given a `ANSIBLE_COLLECTIONS_PATH` that
/// ends with it, and `ansible-config` shows it as set by the environment.
#[test]
fn the_runtime_collections_directory_shows_as_an_environment_setting() {
    let Some(interpreter) = python::reference_python() else {
        return;
    };
    let dir = scratch(&[]);
    let data = dir.path().join("data");
    let status = Command::new(VOLANT)
        .args(["runtime", "status"])
        .env("XDG_DATA_HOME", &data)
        .env("HOME", dir.path())
        .env("VOLANT_PYTHON", &interpreter)
        .output()
        .expect("status runs");
    let status = String::from_utf8_lossy(&status.stdout).into_owned();
    let id = status
        .lines()
        .find_map(|line| line.strip_prefix("managed runtime: "))
        .expect("the runtime is named")
        .trim()
        .to_owned();
    let collections = data.join("volant/runtime").join(&id).join("collections");
    std::fs::create_dir_all(&collections).expect("collections directory");
    let out = started(
        &interpreter,
        VOLANT,
        Some("config"),
        dir.path(),
        &[("XDG_DATA_HOME", data.to_str().expect("utf-8"))],
        &["dump", "--only-changed"],
    );
    let (_, shown, _) = text(&out, dir.path());
    let line = shown
        .lines()
        .find(|line| line.starts_with("COLLECTIONS_PATHS("))
        .unwrap_or_else(|| panic!("no COLLECTIONS_PATHS line in {shown}"));
    let want = format!(
        "COLLECTIONS_PATHS(env: ANSIBLE_COLLECTIONS_PATH) = ['<dir>/.ansible/collections', '/usr/share/ansible/collections', '{}']",
        collections
            .display()
            .to_string()
            .replace(&dir.path().display().to_string(), "<dir>")
    );
    assert_eq!(line, want);
}

/// Started through a link named `ansible-config`, the binary answers as the subcommand, and the
/// usage error names the program.
#[test]
fn the_program_name_runs_the_tool() {
    let Some(interpreter) = python::reference_python() else {
        return;
    };
    let links = tempfile::tempdir().expect("temp dir");
    let link = links.path().join("ansible-config");
    std::os::unix::fs::symlink(VOLANT, &link).expect("symlink");
    let (a, b) = (scratch(&[]), scratch(&[]));
    let theirs = reference(&interpreter, "config", a.path(), &[], &["--bogus"]);
    let mine = started(
        &interpreter,
        link.to_str().expect("utf-8"),
        None,
        b.path(),
        &[],
        &["--bogus"],
    );
    assert_eq!(text(&mine, b.path()), text(&theirs, a.path()));
    assert!(text(&mine, b.path()).2.contains("ansible-config"));
}
