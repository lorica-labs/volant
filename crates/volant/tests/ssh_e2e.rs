// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end runs over a real `ssh` to an sshd on localhost. Ignored by default: they need
//! `VOLANT_SSH_TEST_KEY` (a private key accepted by localhost) and `VOLANT_AGENT_DIR` holding
//! `volant-agent-<triple>`; `just ssh-test` and the `ssh` CI job provide both.
//!
//! Every test gets its own `ansible_remote_tmp`, so the cache the host is asked about is the
//! one this test put there. Sharing `~/.ansible/tmp` would let one test's upload decide
//! another's outcome, and a test of the "no agent is cached" path would pass or fail on the
//! order the suite happened to run in. The one exception is
//! `ssh_become_to_an_unprivileged_user_reaches_the_agent`, which is about the default
//! `~/.ansible/tmp` itself and says why.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .display()
        .to_string()
}

fn key() -> String {
    std::env::var("VOLANT_SSH_TEST_KEY")
        .expect("VOLANT_SSH_TEST_KEY names a key accepted by localhost")
}

/// The account these tests log in as. Asked of the system rather than read from `USER`, which
/// a container or a service manager can leave unset while the account itself is perfectly
/// real; an inventory built from an empty user name reaches nobody.
fn user() -> String {
    let out = Command::new("id").arg("-un").output().unwrap();
    assert!(out.status.success(), "'id -un' failed");
    let name = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert!(!name.is_empty(), "'id -un' printed nothing");
    name
}

/// A directory of this test's own, emptied first so a leftover from an earlier run cannot
/// decide the outcome.
fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("volant-ssh-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Where the host caches the agent for this test. The host is localhost, so this path is
/// readable from here, which is what lets the assertions look at what actually landed.
fn remote_tmp(dir: &Path) -> PathBuf {
    dir.join("remote")
}

fn cache_dir(dir: &Path) -> PathBuf {
    remote_tmp(dir).join(format!("volant-agent-{}", env!("CARGO_PKG_VERSION")))
}

/// Every host gets `-F /dev/null`, so a developer's own `~/.ssh/config` cannot reroute a local
/// `just ssh-test` elsewhere through a `Host *` block carrying `ProxyJump`, `ProxyCommand` or a
/// rewritten `Hostname`. `extra_ssh_args` adds options on top of it, never in place of it.
fn inventory_with_ssh_args(dir: &Path, hosts: &[(&str, &str)], extra_ssh_args: &str) -> String {
    let common_args = if extra_ssh_args.is_empty() {
        "-F /dev/null".to_string()
    } else {
        format!("-F /dev/null {extra_ssh_args}")
    };
    let mut text = String::new();
    for (name, extra) in hosts {
        let _ = writeln!(
            text,
            "{name} ansible_host=127.0.0.1 ansible_user={} ansible_ssh_private_key_file={} ansible_remote_tmp={} ansible_ssh_common_args='{common_args}' {extra}",
            user(),
            key(),
            remote_tmp(dir).display(),
        );
    }
    let path = dir.join("inventory.ini");
    std::fs::write(&path, text).unwrap();
    path.display().to_string()
}

fn inventory(dir: &Path, hosts: &[(&str, &str)]) -> String {
    inventory_with_ssh_args(dir, hosts, "")
}

fn write_executable(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn volant(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(args)
        .env("NO_COLOR", "1")
        .env("ANSIBLE_HOST_KEY_CHECKING", "False")
        .output()
        .unwrap()
}

/// stdout and stderr together: an `UNREACHABLE` line goes to stdout, but a diagnostic that
/// explains an unexpected exit code is often on stderr, and a failure message that shows only
/// half of it costs another run to understand.
fn both(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
#[ignore = "needs sshd on localhost, run through just ssh-test"]
fn ssh_playbook_runs_and_the_agent_is_cached() {
    let dir = tmp("cache");
    let inv = inventory(&dir, &[("box", "")]);
    let first = volant(&["playbook", "-i", &inv, &fixture("ssh/e2e.yml")]);
    let text = both(&first);
    assert_eq!(first.status.code(), Some(0), "{text}");
    assert!(text.contains("\"msg\": \"ran on "), "{text}");
    let agent = cache_dir(&dir).join("volant-agent");
    let before = std::fs::metadata(&agent).unwrap().modified().unwrap();
    let second = volant(&["playbook", "-i", &inv, &fixture("ssh/e2e.yml")]);
    assert_eq!(second.status.code(), Some(0), "{}", both(&second));
    let after = std::fs::metadata(&agent).unwrap().modified().unwrap();
    assert_eq!(before, after, "the second run must reuse the cached agent");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "needs sshd on localhost, run through just ssh-test"]
fn ssh_an_outdated_cached_agent_is_replaced() {
    let dir = tmp("outdated");
    let inv = inventory(&dir, &[("box", "")]);
    let agent = cache_dir(&dir).join("volant-agent");
    write_executable(&agent, "#!/bin/sh\necho volant-agent 0.0.0-stale\n");
    let out = volant(&["playbook", "-i", &inv, &fixture("ssh/e2e.yml")]);
    assert_eq!(out.status.code(), Some(0), "{}", both(&out));
    let replaced = std::fs::read(&agent).unwrap();
    assert!(
        !replaced.starts_with(b"#!/bin/sh"),
        "the stale script must have been replaced by the real agent"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "needs sshd on localhost, run through just ssh-test"]
fn ssh_refused_connection_is_unreachable_with_a_recap() {
    let dir = tmp("refused");
    let inv = inventory(&dir, &[("dead", "ansible_port=1"), ("box", "")]);
    let out = volant(&["playbook", "-i", &inv, &fixture("ssh/e2e.yml")]);
    let text = both(&out);
    assert!(text.contains("fatal: [dead]: UNREACHABLE!"), "{text}");
    // The reference opens the `msg` of an UNREACHABLE with `Task failed: `, measured against
    // ansible-core 2.19.12, and a playbook of the operator's testing
    // `'Task failed' in result.msg` has to keep working here. What would make this red: the
    // prefix being dropped again, which is what two separate local decisions did before.
    assert!(
        text.contains("\"msg\": \"Task failed: Failed to connect to the host via ssh:"),
        "the unreachable message keeps the reference's prefix: {text}"
    );
    assert!(
        text.contains("ok: [box]"),
        "the other host still runs: {text}"
    );
    assert!(text.contains("PLAY RECAP"), "{text}");
    assert_eq!(out.status.code(), Some(4), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "needs sshd on localhost, run through just ssh-test"]
fn ssh_unknown_host_key_is_refused_when_checking_is_on() {
    let dir = tmp("hostkey");
    let inv = inventory_with_ssh_args(
        &dir,
        &[("box", "")],
        &format!("-o UserKnownHostsFile={}/empty_known_hosts", dir.display()),
    );
    let out = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(["playbook", "-i", &inv, &fixture("ssh/e2e.yml")])
        .env("NO_COLOR", "1")
        .env("ANSIBLE_HOST_KEY_CHECKING", "True")
        .output()
        .unwrap();
    let text = both(&out);
    assert!(
        text.contains("UNREACHABLE!") && text.contains("Host key verification failed"),
        "{text}"
    );
    assert_eq!(out.status.code(), Some(4), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "needs sshd on localhost, run through just ssh-test"]
fn ssh_missing_agent_for_the_architecture_is_unreachable() {
    let dir = tmp("noagent");
    let inv = inventory(&dir, &[("box", "")]);
    let empty = dir.join("agents");
    std::fs::create_dir_all(&empty).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(["playbook", "-i", &inv, &fixture("ssh/e2e.yml")])
        .env("NO_COLOR", "1")
        .env("ANSIBLE_HOST_KEY_CHECKING", "False")
        .env("VOLANT_AGENT_DIR", empty.display().to_string())
        .output()
        .unwrap();
    let text = both(&out);
    assert!(
        text.contains("UNREACHABLE!") && text.contains("no agent binary for"),
        "{text}"
    );
    assert_eq!(out.status.code(), Some(4), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The one path that cannot be reached without a real host: the cached agent still refuses to
/// run after a fresh upload, so trying again would not help and the message has to name the
/// file rather than blame a version mismatch. The uploaded agent is a script that exits
/// non-zero, which is what a `noexec` mount, SELinux or a foreign binary looks like from here.
#[test]
#[ignore = "needs sshd on localhost, run through just ssh-test"]
fn ssh_a_cached_agent_that_cannot_run_is_named() {
    let dir = tmp("unrunnable");
    let inv = inventory(&dir, &[("box", "")]);
    let triple = volant::transport::triple_for(std::env::consts::ARCH)
        .expect("this architecture has an agent triple");
    let agents = dir.join("agents");
    write_executable(
        &agents.join(format!("volant-agent-{triple}")),
        "#!/bin/sh\nexit 3\n",
    );
    let out = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(["playbook", "-i", &inv, &fixture("ssh/e2e.yml")])
        .env("NO_COLOR", "1")
        .env("ANSIBLE_HOST_KEY_CHECKING", "False")
        .env("VOLANT_AGENT_DIR", agents.display().to_string())
        .output()
        .unwrap();
    let text = both(&out);
    assert!(text.contains("UNREACHABLE!"), "{text}");
    assert!(text.contains("cannot be run"), "{text}");
    assert!(
        text.contains(&cache_dir(&dir).join("volant-agent").display().to_string()),
        "the message names the cached file: {text}"
    );
    assert!(
        !text.contains("version mismatch"),
        "a copy that cannot run is not a version mismatch: {text}"
    );
    assert_eq!(out.status.code(), Some(4), "{text}");
    assert!(
        cache_dir(&dir).join("volant-agent").exists(),
        "the upload landed, so the fault is the host's and not a missing file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Escalation to an account that cannot read the connecting user's home directory. This is the
/// case every other `become` test misses: they all escalate to `root`, which traverses anything.
///
/// A home directory at mode 0700 - the default wherever `HOME_MODE` says so, which includes RHEL
/// and Fedora - hides the connecting user's agent cache from everybody else, so an agent cached
/// there and then run through `sudo -u someone-else` cannot be reached at all, and every
/// escalated task on the host failed with whatever the remote shell said about the file. The
/// agent for an escalated link is cached under the target user's own home instead.
///
/// The test makes its own throwaway account and removes it again, because there is no ordinary
/// unprivileged account on a machine whose home directory an agent can be written to: the
/// service accounts that exist have `/nonexistent` or `/` for a home. It needs the same
/// passwordless `sudo` the escalation tests beside it already need, and it fails loudly if it
/// cannot have it rather than passing while proving nothing.
#[test]
#[ignore = "needs sshd on localhost and passwordless sudo, run through just ssh-test"]
fn ssh_become_to_an_unprivileged_user_reaches_the_agent() {
    let target = format!("volant-t{}", std::process::id());
    // Read before the account exists, so nothing that can panic sits between the `useradd` and
    // the guard that undoes it.
    let home = home_of(&user());
    let mode = std::fs::metadata(&home).unwrap().permissions().mode() & 0o777;
    let added = Command::new("sudo")
        .args(["-n", "useradd", "-m", "-s", "/bin/sh", &target])
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "creating the unprivileged account this test escalates to: {}",
        String::from_utf8_lossy(&added.stderr)
    );
    let mut restore = Restore {
        home: home.clone(),
        mode,
        target: target.clone(),
        done: false,
    };
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let dir = tmp("becomeunprivileged");
    // No `ansible_remote_tmp` here, unlike every other test in this file: the default
    // `~/.ansible/tmp` is the whole point, because it is the path that lands inside the home
    // directory the escalated user may not enter.
    let inv = dir.join("inventory.ini");
    std::fs::write(
        &inv,
        format!(
            "localhost ansible_host=127.0.0.1 ansible_user={} ansible_ssh_private_key_file={} \
             ansible_ssh_common_args='-F /dev/null'\n",
            user(),
            key(),
        ),
    )
    .unwrap();
    let play = dir.join("become-unprivileged.yml");
    std::fs::write(
        &play,
        format!(
            "- hosts: localhost\n  gather_facts: false\n  become: true\n  become_user: {target}\n  \
             tasks:\n    - shell: id -un\n      register: who\n    - debug:\n        msg: \
             \"{{{{ who.stdout }}}}\"\n"
        ),
    )
    .unwrap();
    let out = volant(&[
        "playbook",
        "-i",
        &inv.display().to_string(),
        &play.display().to_string(),
    ]);
    let text = both(&out);
    // The machine goes back to what it was before anything is asserted: a failure here must
    // leave neither the account behind nor the invoking user's own home at 0700.
    let removed = restore
        .now()
        .expect("the restore runs here rather than in Drop")
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains(&format!(r#""msg": "{target}""#)),
        "the task must run as {target}: {text}"
    );
    assert!(
        removed.status.success(),
        "removing {target} again: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Puts the machine back whichever way the test above leaves it. The account and the 0700 home
/// have to survive a panic between the `chmod` and the assertions - four `unwrap`s and a whole
/// playbook run sit in there - and a leaked account with somebody else's home locked at 0700 is
/// not something a later test can recover from. `Drop` is the only thing a panic still runs.
struct Restore {
    home: PathBuf,
    mode: u32,
    target: String,
    done: bool,
}

impl Restore {
    /// Undoes both changes and hands back what `userdel` said, once. Calling it explicitly
    /// before the assertions is what lets the test assert on that; the `Drop` after a panic
    /// then finds the work already done and returns `None`.
    fn now(&mut self) -> Option<std::io::Result<Output>> {
        if std::mem::replace(&mut self.done, true) {
            return None;
        }
        // Nothing panics here: on the panicking path a second panic inside `Drop` aborts the
        // process and buries the message that says what actually failed. The caller decides
        // what to make of the outcome instead.
        let _ = std::fs::set_permissions(&self.home, std::fs::Permissions::from_mode(self.mode));
        Some(
            Command::new("sudo")
                .args(["-n", "userdel", "-r", &self.target])
                .output(),
        )
    }
}

impl Drop for Restore {
    fn drop(&mut self) {
        self.now();
    }
}

/// One account's home directory, asked of the system rather than read from `HOME`, which the
/// account running the tests may have pointed somewhere else entirely.
fn home_of(name: &str) -> PathBuf {
    let out = Command::new("getent")
        .args(["passwd", name])
        .output()
        .unwrap();
    assert!(out.status.success(), "'getent passwd {name}' failed");
    let line = String::from_utf8(out.stdout).unwrap();
    let home = line
        .trim_end()
        .split(':')
        .nth(5)
        .expect("a passwd entry has a home field");
    assert!(!home.is_empty(), "'{name}' has no home directory");
    PathBuf::from(home)
}

/// Escalation over a real `ssh`, which is the only place the second link is actually a second
/// `ssh` process running the same cached agent under `sudo`. The play escalates and one task
/// declines, so the run proves both directions on one host: a `become` that quietly did nothing
/// would print the invoking account where `root` is expected.
///
/// Needs passwordless sudo for the account these tests log in as, as on the development machine
/// and on the CI runner.
#[test]
#[ignore = "needs sshd on localhost, run through just ssh-test"]
fn ssh_become_over_ssh() {
    let dir = tmp("become");
    // The fixture targets `localhost`, and an inventory that defines that name wins over the
    // implicit local one: the run goes over `ssh` to 127.0.0.1 like every other test here.
    let inv = inventory(&dir, &[("localhost", "")]);
    let out = volant(&["playbook", "-i", &inv, &fixture("become.yml")]);
    let text = both(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains(&format!(r#""msg": "root then {}""#, user())),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A delegated task travels over the **delegate's** connection, never the delegating host's.
///
/// Two inventory names for the same localhost, each with an `ansible_remote_tmp` of its own.
/// The play runs on `a` and its only task is delegated to `b`, so the agent has to be probed
/// and cached under `b`'s directory and `a`'s must stay empty: `a`'s own link is never opened
/// at all. Measured on ansible-core 2.19.12, that is exactly what the reference does - a task
/// delegated away from a host nothing can reach still runs.
///
/// This lives in the ssh suite because the local suite cannot see it: with every host on the
/// local connection there is no per-host connection state to tell the two apart.
///
/// What would make this red: the transport built from the delegating host's variables, which
/// puts the agent under `a`'s directory and leaves `b`'s empty; or the arrow missing from the
/// line, which hides which host actually ran the task.
#[test]
#[ignore = "needs sshd on localhost, run through just ssh-test"]
fn ssh_delegate_to_another_inventory_name() {
    let dir = tmp("delegate");
    let a_tmp = dir.join("remote-a");
    let b_tmp = dir.join("remote-b");
    let inv_path = dir.join("inventory.ini");
    let mut text = String::new();
    for (host, remote) in [("a", &a_tmp), ("b", &b_tmp)] {
        let _ = writeln!(
            text,
            "{host} ansible_host=127.0.0.1 ansible_user={} ansible_ssh_private_key_file={} ansible_remote_tmp={} ansible_ssh_common_args='-F /dev/null'",
            user(),
            key(),
            remote.display(),
        );
    }
    std::fs::write(&inv_path, text).unwrap();
    let play = dir.join("delegated.yml");
    std::fs::write(
        &play,
        "- hosts: a\n  gather_facts: false\n  tasks:\n    - name: delegated\n      command: echo delegated\n      delegate_to: b\n",
    )
    .unwrap();
    let out = volant(&[
        "playbook",
        "-i",
        &inv_path.display().to_string(),
        &play.display().to_string(),
    ]);
    let shown = both(&out);
    assert_eq!(out.status.code(), Some(0), "{shown}");
    assert!(shown.contains("changed: [a -> b]"), "{shown}");
    let cached = |root: &Path| root.join(format!("volant-agent-{}", env!("CARGO_PKG_VERSION")));
    assert!(
        cached(&b_tmp).join("volant-agent").exists(),
        "the delegate's own directory holds the agent: {shown}"
    );
    assert!(
        !cached(&a_tmp).join("volant-agent").exists(),
        "the delegating host's connection is never opened, so nothing lands in its directory: {shown}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The same order `crates/volant-agent/src/interpreter.rs`'s `CANDIDATES` tries on the managed
/// host, best first - kept in sync with it by hand, since ssh_e2e.rs links against `volant`, not
/// `volant-agent`, and the list is private there besides.
const HOST_PYTHON_CANDIDATES: [&str; 8] = [
    "python3.13",
    "python3.12",
    "python3.11",
    "python3.10",
    "python3.9",
    "python3.8",
    "/usr/bin/python3",
    "python3",
];

/// The controller's python and the managed host's must be two distinct interpreters, or a
/// python task passing here would prove nothing about the claim the whole milestone rests on:
/// the module_utils travel in the blob, so the host never needs ansible-core. `just ssh-test`
/// already refuses to run at all when `VOLANT_PYTHON` lacks ansible-core; this guards the other
/// half, probed the way the agent actually resolves an interpreter rather than by hoping it
/// picks `/usr/bin/python3`: over the ssh session's own `PATH`, not the controller's, and every
/// name the reference's fallback list tries before it, not that one path alone. The agent tries
/// `python3.13` down to `python3.8` first and leaves the host's `site-packages` on `sys.path`
/// behind the blob, so an ansible-core reachable under any earlier name - a venv's `bin` on the
/// ssh session's `PATH`, the ordinary place to put `VOLANT_PYTHON` - would let a blob missing a
/// `module_utils` entry still import it, and this guard would have missed exactly that.
fn assert_host_and_controller_pythons_are_distinct() {
    let controller = std::env::var("VOLANT_PYTHON").expect(
        "VOLANT_PYTHON names a python with ansible-core; ssh-test checks this before any ssh_* test runs",
    );
    assert_ne!(
        controller, "/usr/bin/python3",
        "VOLANT_PYTHON must not be the bare host interpreter this test relies on being ansible-core-free"
    );
    for candidate in HOST_PYTHON_CANDIDATES {
        // Resolved and checked in one remote shell: a name absent from the ssh session's PATH
        // exits 0 without checking anything, and a name present is asked whether it can import
        // ansible, failure meaning "good, it cannot".
        let probe = format!(
            "p=$(command -v '{candidate}' 2>/dev/null) || exit 0; \"$p\" -c 'import ansible' 2>/dev/null && exit 1; exit 0"
        );
        let out = Command::new("ssh")
            .args([
                "-F",
                "/dev/null",
                "-i",
                &key(),
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                &format!("{}@127.0.0.1", user()),
                &probe,
            ])
            .output()
            .unwrap_or_else(|e| {
                panic!("probing '{candidate}' over the ssh session's own PATH: {e}")
            });
        assert!(
            out.status.success(),
            "the host's '{candidate}', resolved on the ssh session's own PATH, has ansible-core; \
             this test cannot prove the module_utils came from the blob: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// A python module runs over a real ssh and changes the disk. `ping` proves the path, `file`
/// proves the effect - a module that reported `changed` without touching the filesystem would
/// pass a ping-only test.
#[test]
#[ignore = "needs sshd on localhost, run through just ssh-test"]
fn ssh_runs_a_python_module_and_changes_the_disk() {
    assert_host_and_controller_pythons_are_distinct();
    let dir = tmp("pythonmodule");
    let inv = inventory(&dir, &[("box", "")]);
    let target = dir.join("touched");
    let out = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args([
            "playbook",
            "-i",
            &inv,
            "-e",
            &format!("target={}", target.display()),
            &fixture("ssh/python-module.yml"),
        ])
        .env("NO_COLOR", "1")
        .env("ANSIBLE_HOST_KEY_CHECKING", "False")
        .output()
        .unwrap();
    let text = both(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains("\"msg\": \"pong\""),
        "ping did not prove the path: {text}"
    );
    assert!(
        target.exists(),
        "file did not prove the effect, {} was never created: {text}",
        target.display()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A python module's own result is untrusted like any other module's, all the way to the CLI a
/// user actually runs. The lower-level proof lives in `executor::run`'s own tests, against a
/// `VarStore` built by hand; nothing before this ran the claim through a real ssh, a real agent
/// and a real warm python server.
///
/// What would make this red: `register` losing the untrusted mark for a python task
/// specifically, which nothing below the CLI would catch if the mark were applied by module
/// kind rather than uniformly to every result a managed host returns.
#[test]
#[ignore = "needs sshd on localhost, run through just ssh-test"]
fn ssh_a_python_module_s_result_is_never_evaluated() {
    assert_host_and_controller_pythons_are_distinct();
    let dir = tmp("pytrust");
    let inv = inventory(&dir, &[("box", "")]);
    let marker = dir.join("marker");
    let payload = dir.join("payload.txt");
    std::fs::write(
        &payload,
        format!("{{{{ lookup('pipe', 'touch {}') }}}}", marker.display()),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args([
            "playbook",
            "-i",
            &inv,
            "-e",
            &format!("payload={}", payload.display()),
            &fixture("ssh/untrusted-python.yml"),
        ])
        .env("NO_COLOR", "1")
        .env("ANSIBLE_HOST_KEY_CHECKING", "False")
        .output()
        .unwrap();
    let text = both(&out);
    assert!(
        !marker.exists(),
        "the lookup embedded in a python module's own result ran on the controller:\n{text}"
    );
    assert!(
        text.contains("lookup('pipe'"),
        "the raw text should be shown as data:\n{text}"
    );
    assert_eq!(out.status.code(), Some(0), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Ctrl-C during a real ssh run exits 99 and leaves the host with no surviving task, which
/// nothing else proves at the CLI level.
///
/// It does **not** isolate `AgentLink::stop_batch`'s own forward to `cancel`: the CLI process
/// exiting drops the link, and `local_transport.rs::dropping_the_link_lets_the_agent_stop_its_task`
/// proves the agent kills its process group on a dropped connection alone, so this test would
/// still pass even if `stop_batch`'s forward did nothing at all.
/// `executor::run::tests::a_real_link_s_stop_batch_forward_reaches_the_agent` is the one that
/// isolates the forward, against a real agent, with the link kept open until after the check.
///
/// What would make this red: exit 99 not printed, or the task surviving on the host after
/// Ctrl-C - either is a regression this test catches, whichever piece caused it.
#[test]
#[ignore = "needs sshd on localhost, run through just ssh-test"]
fn ssh_ctrl_c_stops_the_task_on_the_real_host() {
    let dir = tmp("ctrlc");
    let inv = inventory(&dir, &[("box", "")]);
    let marker = format!("40.{}", std::process::id());
    let play = dir.join("sleep.yml");
    std::fs::write(
        &play,
        format!(
            "- hosts: all\n  gather_facts: false\n  tasks:\n    - name: Sleep\n      shell: \"sleep {marker} & wait\"\n"
        ),
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(["playbook", "-i", &inv, &play.display().to_string()])
        .env("NO_COLOR", "1")
        .env("ANSIBLE_HOST_KEY_CHECKING", "False")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let killed = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success(), "sending SIGINT to the volant process");
    let out = child.wait_with_output().unwrap();
    let text = both(&out);
    assert_eq!(out.status.code(), Some(99), "{text}");
    let survivors = Command::new("pgrep")
        .args(["-f", &marker])
        .output()
        .unwrap();
    assert!(
        survivors.stdout.is_empty(),
        "the task kept running on the host after Ctrl-C:\n{}\n{text}",
        String::from_utf8_lossy(&survivors.stdout)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The unit `ssh/actions.yml` asks to be started. Present and running wherever systemd is the
/// init system, which the CI runner and the development machine both are, so the task changes
/// nothing on either.
const RUNNING_UNIT: &str = "systemd-journald.service";

/// What `ssh/actions.yml` needs of the host beyond the ssh link, checked before anything runs so
/// a missing piece fails here, saying which, rather than as a task failure or a `changed` count
/// that happens to match. The host is localhost, so it is asked directly.
fn assert_host_can_run_the_action_play() {
    let sudo = Command::new("sudo")
        .args(["-n", "true"])
        .output()
        .unwrap_or_else(|e| panic!("running 'sudo -n true': {e}"));
    assert!(
        sudo.status.success(),
        "the play's package and service tasks escalate, and 'sudo -n true' failed: {}",
        String::from_utf8_lossy(&sudo.stderr)
    );
    let show = Command::new("systemctl")
        .args(["show", "-p", "LoadState", "--value", RUNNING_UNIT])
        .output()
        .unwrap_or_else(|e| panic!("the play's service task needs systemctl: {e}"));
    assert_eq!(
        String::from_utf8_lossy(&show.stdout).trim(),
        "loaded",
        "the play starts {RUNNING_UNIT}, which this host does not have"
    );
    let active = Command::new("systemctl")
        .args(["is-active", RUNNING_UNIT])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&active.stdout).trim(),
        "active",
        "{RUNNING_UNIT} must already run, or the first pass would count one change more"
    );
}

/// [`volant`], ended at a deadline. A plugin is several sub-tasks over one link, and a sub-task
/// whose result never comes back is a run that waits for ever rather than one that fails.
fn volant_within(args: &[&str], deadline: Duration) -> Output {
    let child = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(args)
        .env("NO_COLOR", "1")
        .env("ANSIBLE_HOST_KEY_CHECKING", "False")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id().to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    rx.recv_timeout(deadline).map_or_else(
        |_| {
            let _ = Command::new("kill").args(["-KILL", &pid]).status();
            panic!("volant did not finish within {deadline:?}, so a sub-task never came back");
        },
        Result::unwrap,
    )
}

/// One `ssh/actions.yml` run, `secret` being what its template renders.
fn run_actions(inv: &str, secret: &str) -> Output {
    volant_within(
        &[
            "playbook",
            "-i",
            inv,
            "-e",
            &format!("secret={secret}"),
            &fixture("ssh/actions.yml"),
        ],
        Duration::from_secs(180),
    )
}

/// A destination of its own for each inventory name, with the `unpacked` directory `unarchive`
/// needs already there, handed to the play as the host variable `dest`.
fn dest_for(dir: &Path, host: &str) -> (PathBuf, String) {
    let dest = dir.join(format!("dest-{host}"));
    std::fs::create_dir_all(dest.join("unpacked")).unwrap();
    let var = format!("dest={}", dest.display());
    (dest, var)
}

/// The `PLAY RECAP` counters of one host, `ok=6 changed=4 ...` read into a map. Empty when the
/// recap has no line for the host, which every caller asserts against.
fn recap(text: &str, host: &str) -> BTreeMap<String, u32> {
    let Some(line) = text.lines().find(|line| {
        let mut words = line.split_whitespace();
        words.next() == Some(host) && words.next() == Some(":")
    }) else {
        return BTreeMap::new();
    };
    line.split_whitespace()
        .filter_map(|word| word.split_once('='))
        .filter_map(|(key, value)| Some((key.to_string(), value.parse().ok()?)))
        .collect()
}

fn assert_recap(text: &str, host: &str, changed: u32) {
    let counts = recap(text, host);
    assert_eq!(counts.get("changed"), Some(&changed), "{host}: {text}");
    assert_eq!(counts.get("failed"), Some(&0), "{host}: {text}");
    assert_eq!(counts.get("unreachable"), Some(&0), "{host}: {text}");
}

/// A command run as root that has to succeed, for what the escalated tasks' own agent left under
/// `remote_tmp`: its cache is root's, mode 0700, and this account cannot read it.
fn as_root(args: &[&str]) -> String {
    let out = Command::new("sudo").arg("-n").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "'sudo -n {}': {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Every `volant-blobs-*` cache the run left under `remote_tmp`, one per account an agent ran as.
fn blob_caches(dir: &Path) -> Vec<PathBuf> {
    let mut caches: Vec<PathBuf> = std::fs::read_dir(remote_tmp(dir))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("volant-blobs-"))
        })
        .collect();
    caches.sort();
    caches
}

/// The name of the one entry a cache holds, asserted to be the union: a payload name, whose bytes
/// are a zip. A staged file is neither a zip nor, once its connection has ended, anywhere at all.
fn the_union_in(cache: &Path) -> String {
    let entries = as_root(&["ls", "-A", &cache.display().to_string()]);
    let entries: Vec<&str> = entries.lines().collect();
    assert_eq!(
        entries.len(),
        1,
        "{} holds more than the union: {entries:?}",
        cache.display()
    );
    let name = entries[0];
    assert!(
        name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()),
        "{} holds '{name}', which is no payload name",
        cache.display()
    );
    let magic = as_root(&[
        "od",
        "-An",
        "-c",
        "-N",
        "4",
        &cache.join(name).display().to_string(),
    ]);
    assert_eq!(
        magic.split_whitespace().collect::<Vec<_>>(),
        ["P", "K", "003", "004"],
        "{}/{name} is not the union's zip",
        cache.display()
    );
    name.to_string()
}

/// The escalated tasks leave root's cache behind, which this account cannot remove.
fn remove_as_root(dir: &Path) {
    let _ = Command::new("sudo")
        .args(["-n", "rm", "-rf", &dir.display().to_string()])
        .status();
}

/// Every action plugin over a real ssh link, twice. The first pass writes four files and finds
/// the package and the unit already as asked; the second finds everything in place. Each file is
/// read back and compared byte for byte, and its mode, which the play sets so the host's umask
/// cannot decide it.
///
/// Measured on ansible-core 2.19.12 with the same play over `-c local`: `ok=6 changed=4` then
/// `ok=6 changed=0`, and the template rendered as `secret=<secret>\nline 1\nline 2\n`
/// (`trim_blocks`, one trailing newline kept).
///
/// What would make this red: a plugin that reports success without its last sub-task having
/// written anything (a file missing or holding other bytes), a plugin that rewrites what is
/// already there (a second pass with `changed` above 0), or a sub-task's result lost between two
/// turns (a failed or unreachable count, or the deadline).
#[test]
#[ignore = "needs sshd on localhost and passwordless sudo, run through just ssh-test"]
fn ssh_copy_and_template_converge_on_a_real_host() {
    assert_host_can_run_the_action_play();
    assert_host_and_controller_pythons_are_distinct();
    let dir = tmp("actions");
    let (dest, var) = dest_for(&dir, "box");
    let inv = inventory(&dir, &[("box", var.as_str())]);
    let secret = "converge-secret";
    let expected: [(&str, Vec<u8>, u32); 4] = [
        (
            "copied.txt",
            std::fs::read(fixture("ssh/files/greeting.txt")).unwrap(),
            0o644,
        ),
        ("content.txt", b"inline content\n".to_vec(), 0o644),
        (
            "rendered.txt",
            format!("secret={secret}\nline 1\nline 2\n").into_bytes(),
            0o600,
        ),
        (
            "unpacked/unpacked.txt",
            b"from the archive\n".to_vec(),
            0o644,
        ),
    ];
    for (pass, changed) in [("first", 4), ("second", 0)] {
        let out = run_actions(&inv, secret);
        let text = both(&out);
        assert_eq!(out.status.code(), Some(0), "{pass} pass: {text}");
        assert_recap(&text, "box", changed);
        for (name, bytes, mode) in &expected {
            let path = dest.join(name);
            let written = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("{pass} pass, {}: {e}\n{text}", path.display()));
            assert_eq!(
                &written,
                bytes,
                "{pass} pass, {} holds other bytes",
                path.display()
            );
            let actual = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(actual, *mode, "{pass} pass, mode of {}", path.display());
        }
    }
    remove_as_root(&dir);
}

/// After the play, the agents' caches on the host hold the union and nothing else: no file a
/// task staged, taken or not, and no connection's staging directory. A rendered template often
/// carries a secret, and a staged copy of it outliving its task is a copy nobody would ever
/// remove; the rendered value is also looked for anywhere under `remote_tmp`.
///
/// The escalated tasks run an agent as root with a cache of its own, so there are two caches,
/// and both are read.
///
/// What would make this red: the agent keeping a staged file after its module ran, or keeping a
/// connection's staging directory after the controller left.
#[test]
#[ignore = "needs sshd on localhost and passwordless sudo, run through just ssh-test"]
fn ssh_a_rendered_file_leaves_nothing_in_the_agent_cache() {
    assert_host_can_run_the_action_play();
    assert_host_and_controller_pythons_are_distinct();
    let dir = tmp("nothingstaged");
    let (dest, var) = dest_for(&dir, "box");
    let inv = inventory(&dir, &[("box", var.as_str())]);
    let secret = format!("staged-secret-{}", std::process::id());
    let out = run_actions(&inv, &secret);
    let text = both(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert_recap(&text, "box", 4);
    assert!(
        std::fs::read_to_string(dest.join("rendered.txt"))
            .unwrap()
            .contains(&secret),
        "the template did not render the secret, so looking for it proves nothing"
    );
    let caches = blob_caches(&dir);
    assert_eq!(
        caches.len(),
        2,
        "one cache for the connecting account and one for root: {caches:?}"
    );
    let unions: Vec<String> = caches.iter().map(|cache| the_union_in(cache)).collect();
    assert_eq!(unions[0], unions[1], "both agents hold the run's one union");
    let found = Command::new("sudo")
        .args(["-n", "grep", "-rlF", &secret])
        .arg(remote_tmp(&dir))
        .output()
        .unwrap();
    assert_eq!(
        found.status.code(),
        Some(1),
        "the rendered secret is still under remote_tmp: {}{}",
        String::from_utf8_lossy(&found.stdout),
        String::from_utf8_lossy(&found.stderr)
    );
    remove_as_root(&dir);
}

/// Two inventory names for the one sshd, so two links, and two more for the escalated tasks,
/// all on one account's caches. Both hosts copy the same file, which is the same staged blob on
/// two links at once: each link stages its own copy, and both tasks succeed.
///
/// There is no trace of the wire to read, so this reads what the host keeps. The union's inode
/// and mtime are taken after a first run and again after a second one: a union that either
/// link's end, or the other agent's start, removed would come back as a new file, and so would
/// one written again. A `put_blob` of the bytes already there is not visible here, since `store`
/// returns before writing when the file already matches; `a_payload_travels_once_per_link` in
/// `executor::run` counts those against a scripted agent.
///
/// What would make this red: a copy that fails on one of the two links, or the union written
/// again, or removed and re-sent, on the second run.
#[test]
#[ignore = "needs sshd on localhost and passwordless sudo, run through just ssh-test"]
fn ssh_two_links_reuse_the_union_the_host_holds() {
    assert_host_can_run_the_action_play();
    assert_host_and_controller_pythons_are_distinct();
    let dir = tmp("twolinks");
    let (a_dest, a_var) = dest_for(&dir, "a");
    let (b_dest, b_var) = dest_for(&dir, "b");
    let inv = inventory(&dir, &[("a", a_var.as_str()), ("b", b_var.as_str())]);
    let greeting = std::fs::read(fixture("ssh/files/greeting.txt")).unwrap();
    let mut held = Vec::new();
    for (pass, changed) in [("first", 4), ("second", 0)] {
        let out = run_actions(&inv, "two-links-secret");
        let text = both(&out);
        assert_eq!(out.status.code(), Some(0), "{pass} pass: {text}");
        for host in ["a", "b"] {
            assert_recap(&text, host, changed);
        }
        for dest in [&a_dest, &b_dest] {
            assert_eq!(
                std::fs::read(dest.join("copied.txt")).unwrap(),
                greeting,
                "{pass} pass, {}",
                dest.display()
            );
        }
        let caches = blob_caches(&dir);
        assert_eq!(caches.len(), 2, "{pass} pass: {caches:?}");
        let stamps: Vec<String> = caches
            .iter()
            .map(|cache| {
                let union = cache.join(the_union_in(cache));
                as_root(&["stat", "-c", "%n %i %y", &union.display().to_string()])
            })
            .collect();
        held.push(stamps);
    }
    assert_eq!(
        held[0], held[1],
        "the second run wrote the union again instead of reusing it"
    );
    remove_as_root(&dir);
}
