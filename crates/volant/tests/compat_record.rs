// SPDX-License-Identifier: GPL-3.0-or-later
//! The `volant.compat` collection under the recorded ansible-core: it builds and installs, and its
//! `record` callback writes the run it sees without changing it.
#![cfg(unix)]

#[path = "common/process.rs"]
mod process;
#[path = "common/python.rs"]
mod python;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn collection() -> PathBuf {
    repository().join("collections/ansible_collections/volant/compat")
}

/// One of ansible-core's programs, started through `main()` under `python` in `dir`.
fn ansible(python: &Path, tool: &str, dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(python);
    command
        .args([
            "-P",
            "-c",
            &format!("import sys\nfrom ansible.cli.{tool} import main\nsys.argv[0] = 'ansible-{tool}'\nmain()"),
        ])
        .args(args)
        .current_dir(dir)
        .env("HOME", dir)
        .env_remove("ANSIBLE_CONFIG");
    command
}

fn run(command: &mut Command) -> Output {
    process::run_within(command, Duration::from_secs(120), None)
}

/// The sample play, copied into a scratch directory.
fn sample() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    copy(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/compat"),
        dir.path(),
    );
    dir
}

fn copy(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("read the sample") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            std::fs::create_dir_all(&target).expect("mkdir");
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy");
        }
    }
}

/// The sample play in `dir`, with the collection of the repository, recorded to `record` or run
/// without the callback.
fn play(python: &Path, dir: &Path, record: Option<&Path>) -> Output {
    let mut command = ansible(
        python,
        "playbook",
        dir,
        &["-i", "hosts.ini", "-f", "1", "site.yml"],
    );
    command.env("ANSIBLE_COLLECTIONS_PATH", repository().join("collections"));
    if let Some(path) = record {
        command
            .env("ANSIBLE_CALLBACKS_ENABLED", "volant.compat.record")
            .env("VOLANT_RECORD_PATH", path);
    }
    run(&mut command)
}

/// Every line of a recording, decompressed by `python` when it is gzip.
fn events(python: &Path, path: &Path) -> Vec<serde_json::Value> {
    let text = if path.extension().is_some_and(|e| e == "gz") {
        let out = Command::new(python)
            .args([
                "-c",
                "import gzip, sys; sys.stdout.buffer.write(gzip.open(sys.argv[1]).read())",
            ])
            .arg(path)
            .output()
            .expect("gunzip");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("utf-8")
    } else {
        std::fs::read_to_string(path).expect("read the recording")
    };
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect()
}

/// The collection carries the version of the Volant it ships with.
#[test]
fn the_collection_version_is_the_workspace_version() {
    let galaxy = std::fs::read_to_string(collection().join("galaxy.yml")).expect("galaxy.yml");
    let version = galaxy
        .lines()
        .find_map(|line| line.strip_prefix("version: "))
        .expect("a version line");
    assert_eq!(version.trim(), env!("CARGO_PKG_VERSION"));
}

/// `ansible-galaxy collection build` and `install` accept the collection.
#[test]
fn the_collection_builds_and_installs() {
    let Some(python) = python::reference_python() else {
        return;
    };
    let dir = tempfile::tempdir().expect("temp dir");
    let source = collection();
    let build = [
        "collection",
        "build",
        source.to_str().unwrap(),
        "--output-path",
        "built",
    ];
    let out = run(&mut ansible(&python, "galaxy", dir.path(), &build));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let tarball = format!("built/volant-compat-{}.tar.gz", env!("CARGO_PKG_VERSION"));
    let install = ["collection", "install", &tarball, "-p", "installed"];
    let out = run(&mut ansible(&python, "galaxy", dir.path(), &install));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        dir.path()
            .join("installed/ansible_collections/volant/compat/plugins/callback/record.py")
            .is_file()
    );
}

/// A recorded run: every line is JSON, from the header to the recap; a `no_log` result is
/// censored; the file is private; and the run prints and exits as it does without the callback.
#[test]
fn a_run_is_recorded_without_changing_it() {
    let Some(python) = python::reference_python() else {
        return;
    };
    let dir = sample();
    let path = dir.path().join("shadow/run.jsonl.gz");
    let recorded = play(&python, dir.path(), Some(&path));
    let plain = play(&python, dir.path(), None);
    assert_eq!(
        recorded.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&recorded.stderr)
    );
    assert_eq!(recorded.status.code(), plain.status.code());
    assert_eq!(
        String::from_utf8_lossy(&recorded.stdout),
        String::from_utf8_lossy(&plain.stdout)
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let events = events(&python, &path);
    let kind = |e: &serde_json::Value| e["event"].as_str().unwrap().to_owned();
    assert_eq!(kind(&events[0]), "header");
    assert_eq!(events[0]["format"], 1);
    assert_eq!(events[0]["ansible_core"], python::RECORDED.trim());
    assert_eq!(kind(events.last().unwrap()), "stats");
    for wanted in ["inventory", "play", "task", "result", "include", "notify"] {
        assert!(
            events.iter().any(|e| kind(e) == wanted),
            "no {wanted} event"
        );
    }
    let secret = events
        .iter()
        .find(|e| kind(e) == "task" && e["name"] == "secret")
        .expect("the no_log task")["uuid"]
        .clone();
    let results: Vec<_> = events
        .iter()
        .filter(|e| kind(e) == "result" && e["task"] == secret)
        .collect();
    assert_eq!(results.len(), 2);
    for result in results {
        assert_eq!(result["result"], serde_json::json!({"censored": true}));
        assert_eq!(result["status"], "ok");
    }
}

/// Without `.gz` the recording is plain JSON Lines.
#[test]
fn a_name_without_gz_is_plain_text() {
    let Some(python) = python::reference_python() else {
        return;
    };
    let dir = sample();
    let path = dir.path().join("run.jsonl");
    assert!(play(&python, dir.path(), Some(&path)).status.success());
    assert!(
        std::fs::read_to_string(&path)
            .expect("plain text")
            .starts_with("{\"event\": \"header\"")
    );
}

/// A path that cannot be written warns, and the run goes on as it would have.
#[test]
fn an_unwritable_path_warns_and_the_run_goes_on() {
    let Some(python) = python::reference_python() else {
        return;
    };
    let dir = sample();
    std::fs::write(dir.path().join("file"), "").unwrap();
    let out = play(
        &python,
        dir.path(),
        Some(&dir.path().join("file/run.jsonl")),
    );
    assert_eq!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cannot write the recording"), "{stderr}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("PLAY RECAP"));
}
