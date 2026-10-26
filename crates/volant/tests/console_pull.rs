// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant console` and `volant pull`: ansible-core's own tools, run from the interpreter of the
//! reference. `pull` needs `git`, which every machine that runs the reference suite has.
#![cfg(unix)]

#[path = "common/process.rs"]
mod process;
#[path = "common/python.rs"]
mod python;

use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
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

const INVENTORY: &str = "web:\n  hosts:\n    h1:\n    h2:\ndb:\n  hosts:\n    h3:\nall:\n  vars:\n    ansible_connection: local\n";

/// A piped session: modules, shell lines and every console command but the bare `help`, whose
/// listing depends on the collections installed.
const SESSION: &[u8] =
    b"ping\nshell echo hi\necho hi there\n!echo hi\ncommand echo mod\n# comment\n\n\
list\nlist groups\nlist x\ncd web\ncd nosuch\ncd h1:h2\nforks 2\nforks 0\nforks\nbecome yes\n\
become_user\nbecome_method\ncheck\ncheck yes\ndiff\ndiff no\ntimeout -1\ntimeout x\ntimeout\n\
verbosity x\nverbosity\nhelp forks\nhelp cd\nhelp help\nhelp nosuchxx\nhelp command\nhelp shell\n\
help exit\ncd\ncd /\nverbosity 1\nbecome_method\ncollections a.b, c.d\ncollections none\nexit\n";

/// With the commands on a pipe, the console prints what the reference prints on both outputs
/// and ends at `exit` with code 0; the history file it writes is private.
#[test]
fn the_console_reads_its_commands_from_a_pipe() {
    let Some(python) = python::reference_python() else {
        return;
    };
    let args = ["-i", "inventory.yml", "-f", "1"];
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    for dir in [a.path(), b.path()] {
        std::fs::write(dir.join("inventory.yml"), INVENTORY).unwrap();
    }
    let mut ours = Command::new(VOLANT);
    ours.arg("console").args(args).env("ANSIBLE_NOCOLOR", "1");
    env_of(&mut ours, a.path(), &python);
    let mine = run(&mut ours, Some(SESSION));
    let mut theirs = Command::new(&python);
    theirs
        .args([
            "-P",
            "-c",
            "import sys\nfrom ansible.cli.console import main\nsys.argv[0] = 'x'\nmain()",
        ])
        .args(args)
        .env("ANSIBLE_NOCOLOR", "1");
    env_of(&mut theirs, b.path(), &python);
    let theirs = run(&mut theirs, Some(SESSION));
    assert_eq!(mine.status.code(), Some(0), "{mine:?}");
    assert_eq!(mine.status, theirs.status);
    assert_eq!(
        String::from_utf8_lossy(&mine.stdout),
        String::from_utf8_lossy(&theirs.stdout)
    );
    assert_eq!(
        String::from_utf8_lossy(&mine.stderr),
        String::from_utf8_lossy(&theirs.stderr)
    );
    let history = std::fs::metadata(a.path().join(".ansible-console_history")).unwrap();
    assert_eq!(history.permissions().mode() & 0o777, 0o600);
}

/// The console with no runtime installed and no interpreter named: a module needing Python would
/// print the runtime's installation into the session.
#[test]
fn the_console_runs_without_python() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("inventory.yml"), INVENTORY).unwrap();
    let mut command = Command::new(VOLANT);
    command
        .args(["console", "-i", "inventory.yml", "web"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .env("LOGNAME", "u")
        .env("ANSIBLE_NOCOLOR", "1")
        .env_remove("VOLANT_PYTHON")
        .env_remove("VIRTUAL_ENV")
        .env_remove("ANSIBLE_CONFIG");
    let out = run(&mut command, Some(b"list\nforks 1\nshell echo hi\nexit\n"));
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "Welcome to the ansible console. Type help or ? to list commands.\n\n\
         u@web (2)[f:5]$ h1\nh2\nu@web (2)[f:5]$ u@web (2)[f:1]$ \
         h1 | CHANGED | rc=0 >>\nhi\nh2 | CHANGED | rc=0 >>\nhi\nu@web (2)[f:1]$ \n\
         Ansible-console was exited.\n"
    );
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

/// A pseudo-terminal: our end, and the end the child gets.
fn terminal() -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: both descriptors are written by `openpty` and owned by nothing else.
    let opened = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(opened, 0, "a pseudo-terminal opens");
    // SAFETY: `openpty` just returned them, and each is wrapped once.
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

fn type_in(master: &OwnedFd, bytes: &[u8]) {
    // SAFETY: `bytes` is valid for its length and `master` is open.
    let n = unsafe { libc::write(master.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
    assert_eq!(usize::try_from(n).ok(), Some(bytes.len()));
}

/// Reads the terminal until `shown` holds `want` `times` times, or `limit` has passed.
fn read_until(
    master: &OwnedFd,
    shown: &mut String,
    want: &str,
    times: usize,
    limit: Duration,
) -> bool {
    let start = std::time::Instant::now();
    while shown.matches(want).count() < times {
        if start.elapsed() > limit {
            return false;
        }
        let mut poll = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid `pollfd`.
        if unsafe { libc::poll(&raw mut poll, 1, 100) } <= 0 {
            continue;
        }
        let mut chunk = [0_u8; 4096];
        // SAFETY: `chunk` is valid for its length and `master` is open.
        let n = unsafe { libc::read(master.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len()) };
        match usize::try_from(n) {
            Ok(n) if n > 0 => shown.push_str(&String::from_utf8_lossy(&chunk[..n])),
            _ => return false,
        }
    }
    true
}

/// [`read_until`] with a limit of a minute, failing with what the screen holds.
fn wait_for(master: &OwnedFd, shown: &mut String, want: &str, times: usize) {
    let seen = read_until(master, shown, want, times, Duration::from_secs(60));
    assert!(seen, "waiting for {want:?}, the screen holds {shown:?}");
}

/// On a terminal the console shows its banner and prompt, ignores ctrl-C at the prompt (Volant
/// stays up and the console goes on), and ends at `exit` with code 0.
#[test]
fn the_console_on_a_terminal_survives_ctrl_c() {
    let Some(python) = python::reference_python() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (master, slave) = terminal();
    let mut command = Command::new(VOLANT);
    command
        .arg("console")
        .args(["-i", "localhost,", "-c", "local"]);
    env_of(&mut command, dir.path(), &python);
    command
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave.try_clone().unwrap());
    // SAFETY: only async-signal-safe calls between fork and exec: a session of its own, with the
    // terminal as its controlling one, so ctrl-C reaches the child's process group.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            // The request's type differs between Linux and macOS.
            libc::ioctl(0, libc::TIOCSCTTY as _, 0);
            Ok(())
        });
    }
    let mut child = command.spawn().expect("volant starts");
    drop(command);
    drop(slave);
    let mut shown = String::new();
    wait_for(&master, &mut shown, "[f:5]$ ", 1);
    assert!(
        shown.contains("Welcome to the ansible console. Type help or ? to list commands."),
        "{shown:?}"
    );
    // The console takes ctrl-C as "start the prompt again": the banner and prompt come twice. A
    // ctrl-C sent before the console reads its first line can be lost, so it is sent again.
    let mut again = false;
    for _ in 0..10 {
        type_in(&master, &[3]);
        again = read_until(&master, &mut shown, "[f:5]$ ", 2, Duration::from_secs(3));
        if again {
            break;
        }
    }
    assert!(again, "no prompt after ctrl-C: {shown:?}");
    assert!(
        child.try_wait().unwrap().is_none(),
        "volant ended at ctrl-C: {shown:?}"
    );
    type_in(
        &master, b"exit
",
    );
    wait_for(&master, &mut shown, "Ansible-console was exited.", 1);
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0), "{shown:?}");
}

/// A repository with a one-task playbook, and the `file://` URL of it.
fn repository(dir: &Path) -> String {
    let repo = dir.join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(
        repo.join("site.yml"),
        "- hosts: localhost
  gather_facts: false
  tasks:
    - ansible.builtin.debug: {msg: local site}
",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "site"]);
    format!("file://{}", repo.display())
}

/// What a pull printed, with its times, its destination and the program path of its command
/// line masked.
fn masked(out: &Output, dest: &Path) -> Vec<String> {
    String::from_utf8_lossy(&out.stdout)
        .replace(&dest.display().to_string(), "<dest>")
        .lines()
        .map(|line| {
            if line.starts_with("Starting Ansible Pull at") {
                "Starting Ansible Pull at <time>".to_owned()
            } else if let Some((_, rest)) = line.split_once("ansible-pull -") {
                format!("<argv0> -{rest}")
            } else {
                line.to_owned()
            }
        })
        .collect()
}

/// `volant pull` against the reference `ansible-pull` of the same interpreter and repository.
#[test]
fn a_pull_prints_what_the_reference_pull_prints() {
    // Compared: the exit codes, the two header lines of the pull itself (times and the program
    // path masked; it prints them after its children's output), the playbook run from its
    // `PLAY [` line on, and the line of a second pull with `-o`. Not compared: the checkout's
    // warnings and result. `ansible-pull` copies its children's standard output and error 10
    // bytes at a time, so where both hold data at once they cut each other mid-line, and when
    // that happens depends on timing.
    let Some(python) = python::reference_python() else {
        return;
    };
    let bin = python.parent().expect("the interpreter has a directory");
    for name in ["ansible-pull", "ansible", "ansible-playbook"] {
        assert!(
            bin.join(name).is_file(),
            "the reference needs {name} beside {}",
            python.display()
        );
    }
    let work = tempfile::tempdir().unwrap();
    let url = repository(work.path());
    let reference = |dir: &Path, args: &[&str]| {
        let mut command = Command::new(&python);
        command
            .args([
                "-P",
                "-c",
                "import sys
from ansible.cli.pull import main
sys.argv[0] = sys.argv[1]
del sys.argv[1]
main()",
            ])
            .arg(bin.join("ansible-pull"))
            .args(["-i", "localhost,"])
            .args(args);
        env_of(&mut command, dir, &python);
        run(&mut command, None)
    };
    let (mine_dir, theirs_dir) = (work.path().join("mine"), work.path().join("theirs"));
    std::fs::create_dir(&mine_dir).unwrap();
    std::fs::create_dir(&theirs_dir).unwrap();
    for (extra, label) in [(&[][..], "first"), (&["-o"][..], "second")] {
        let mut args = vec!["-U", url.as_str(), "-d", "dest"];
        args.extend_from_slice(extra);
        args.push("site.yml");
        let mine = pull(&python, &mine_dir, &args);
        let theirs = reference(&theirs_dir, &args);
        assert_eq!(mine.status.code(), theirs.status.code(), "{label}");
        let (mine, theirs) = (
            masked(&mine, &mine_dir.join("dest")),
            masked(&theirs, &theirs_dir.join("dest")),
        );
        let header = |lines: &[String]| -> Vec<String> {
            lines
                .iter()
                .filter(|l| l.starts_with("Starting Ansible Pull") || l.starts_with("<argv0>"))
                .cloned()
                .collect()
        };
        assert_eq!(header(&mine), header(&theirs), "{label}");
        assert_eq!(header(&mine).len(), 2, "{label}");
        if extra.is_empty() {
            let from = |lines: &[String]| {
                let start = lines
                    .iter()
                    .position(|l| l.starts_with("PLAY ["))
                    .expect("a play");
                lines[start..].to_vec()
            };
            assert_eq!(from(&mine), from(&theirs));
        } else {
            let once = |lines: &[String]| {
                lines
                    .iter()
                    .find(|l| l.starts_with("Repository has not changed"))
                    .cloned()
                    .expect("the repository did not change")
            };
            assert_eq!(once(&mine), once(&theirs));
        }
    }
}
