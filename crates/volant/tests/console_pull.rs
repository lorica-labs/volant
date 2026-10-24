// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant console` and `volant pull`: ansible-core's own tools, run from the interpreter of the
//! reference. `pull` needs `git`, which every machine that runs the reference suite has.
#![cfg(unix)]

#[path = "common/process.rs"]
mod process;
#[path = "common/python.rs"]
mod python;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

const VOLANT: &str = env!("CARGO_BIN_EXE_volant");

fn run(command: &mut Command, input: Option<&[u8]>) -> Output {
    process::run_within(command, Duration::from_secs(180), input)
}

fn env_of(command: &mut Command, dir: &Path, python: &Path) {
    command
        .current_dir(dir)
        .env("HOME", dir)
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env("VOLANT_PYTHON", python)
        .env_remove("ANSIBLE_CONFIG")
        .env_remove("ANSIBLE_COLLECTIONS_PATH");
}

/// With the commands on a pipe, the console prints its banner and prompt, runs them, and ends
/// at the end of the input, as the reference does.
#[test]
fn the_console_reads_its_commands_from_a_pipe() {
    let Some(python) = python::reference_python() else {
        return;
    };
    let input = b"ping\nexit\n";
    let args = ["-i", "localhost,", "-c", "local"];
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut ours = Command::new(VOLANT);
    ours.arg("console").args(args);
    env_of(&mut ours, a.path(), &python);
    let mine = run(&mut ours, Some(input));
    let mut theirs = Command::new(&python);
    theirs
        .args([
            "-P",
            "-c",
            "import sys\nfrom ansible.cli.console import main\nsys.argv[0] = 'x'\nmain()",
        ])
        .args(args);
    env_of(&mut theirs, b.path(), &python);
    let theirs = run(&mut theirs, Some(input));
    assert_eq!(mine.status.code(), Some(0), "{mine:?}");
    assert_eq!(mine.status, theirs.status);
    assert_eq!(mine.stdout, theirs.stdout);
    let text = String::from_utf8_lossy(&mine.stdout);
    assert!(
        text.starts_with("Welcome to the ansible console. Type help or ? to list commands.\n\n"),
        "{text}"
    );
    assert!(text.contains("@all (1)[f:5]$ "), "{text}");
    assert!(text.ends_with("\nAnsible-console was exited.\n"), "{text}");
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.email=a@example.org", "-c", "user.name=n"])
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

fn pull(python: &Path, dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(VOLANT);
    command.arg("pull").args(["-i", "localhost,"]).args(args);
    env_of(&mut command, dir, python);
    run(&mut command, None)
}

/// A pull clones and runs the playbook through the links; a second pull with `-o` stops at once;
/// an unreachable URL exits 2. The links directory is private.
#[test]
fn a_pull_runs_the_playbook_through_the_links() {
    let Some(python) = python::reference_python() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(
        repo.join("site.yml"),
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - debug: {msg: local site}\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "site"]);
    let (repo, dest) = (repo.to_str().unwrap(), dir.path().join("dest"));
    let dest = dest.to_str().unwrap();

    let first = pull(&python, dir.path(), &["-U", repo, "-d", dest, "site.yml"]);
    let text = String::from_utf8_lossy(&first.stdout);
    assert_eq!(first.status.code(), Some(0), "{text}{:?}", first.stderr);
    assert!(text.contains("PLAY RECAP"), "{text}");
    assert!(Path::new(dest).join("site.yml").is_file());
    let links = dir.path().join("cache/volant/pull");
    assert_eq!(
        std::fs::metadata(&links).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(
        text.contains(&format!("{}/ansible-pull -", links.display())),
        "{text}"
    );
    for name in ["ansible", "ansible-playbook"] {
        let target = std::fs::read_link(links.join(name)).unwrap();
        assert_eq!(target.file_name().unwrap(), "volant", "{name}");
    }

    let again = pull(
        &python,
        dir.path(),
        &["-U", repo, "-d", dest, "-o", "site.yml"],
    );
    let text = String::from_utf8_lossy(&again.stdout);
    assert_eq!(again.status.code(), Some(0), "{text}");
    assert!(
        text.contains("Repository has not changed, quitting"),
        "{text}"
    );

    let missing = dir.path().join("absent");
    let gone = pull(
        &python,
        dir.path(),
        &["-U", missing.to_str().unwrap(), "-d", "d2", "site.yml"],
    );
    assert_eq!(gone.status.code(), Some(2), "{gone:?}");
}
