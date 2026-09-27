// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end runs over a real `ssh` to an sshd on localhost. Ignored by default: they need
//! `VOLANT_SSH_TEST_KEY` (a private key accepted by localhost) and `VOLANT_AGENT_DIR` holding
//! `volant-agent-<triple>`; `just ssh-test` and the `ssh` CI job provide both.
//!
//! The native module tests at the end run on the host `VOLANT_TARGET_HOST` names instead, or on
//! localhost only on a CI runner (see [`Machine`]): they create an account, a group and a unit.
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
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

#[path = "common/collections.rs"]
mod collections;

#[cfg(target_os = "linux")]
#[path = "common/native_compare.rs"]
mod native_compare;

/// The reasons the native `setup` hands back for a host outside its subset, the agent's own list.
#[cfg(target_os = "linux")]
#[path = "../../volant-agent/tests/setup_exits/mod.rs"]
mod setup_exits;

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
    // The second script prints exactly this controller's version: a cached file is reused only
    // when it is the binary the controller would upload, so a build reporting the same version
    // is replaced like any other. A reuse check on the version alone runs that script as the
    // agent, and the play fails on an agent that never answers.
    for (name, version) in [
        ("outdated", "0.0.0-stale"),
        ("same-version", env!("CARGO_PKG_VERSION")),
    ] {
        let dir = tmp(name);
        let inv = inventory(&dir, &[("box", "")]);
        let agent = cache_dir(&dir).join("volant-agent");
        write_executable(&agent, &format!("#!/bin/sh\necho volant-agent {version}\n"));
        let out = volant(&["playbook", "-i", &inv, &fixture("ssh/e2e.yml")]);
        assert_eq!(out.status.code(), Some(0), "{name}: {}", both(&out));
        let replaced = std::fs::read(&agent).unwrap();
        assert!(
            !replaced.starts_with(b"#!/bin/sh"),
            "{name}: the script must have been replaced by the real agent"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
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
    // Every name the agent tries on the managed host, read from the list the agent itself reads.
    for candidate in volant_protocol::interpreter::CANDIDATES {
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
    assert_passwordless_sudo("the play's package and service tasks escalate");
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

/// `sudo -n true`, failing with `why` the test needs it rather than as a task that happens to fail.
fn assert_passwordless_sudo(why: &str) {
    let sudo = Command::new("sudo")
        .args(["-n", "true"])
        .output()
        .unwrap_or_else(|e| panic!("running 'sudo -n true': {e}"));
    assert!(
        sudo.status.success(),
        "{why}, and 'sudo -n true' failed: {}",
        String::from_utf8_lossy(&sudo.stderr)
    );
}

/// [`volant`], ended at a deadline. A plugin is several sub-tasks over one link, and a sub-task
/// whose result never comes back is a run that waits for ever rather than one that fails.
fn volant_within(args: &[&str], deadline: Duration) -> Output {
    volant_env_within(args, &[], deadline)
}

/// [`volant_within`] with `envs` set on top of this process's environment.
fn volant_env_within(args: &[&str], envs: &[(&str, &str)], deadline: Duration) -> Output {
    let child = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(args)
        .envs(envs.iter().copied())
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

/// Every action plugin over a real ssh link, twice. The first pass writes five files and finds
/// the package and the unit already as asked; the second finds everything in place. Each file is
/// read back and compared byte for byte, and its mode, which the play sets so the host's umask
/// cannot decide it.
///
/// Measured on ansible-core 2.19.12, `-c local`, on the reference-comparable subset of the play:
/// eight tasks, seven backed by an action plugin and a `debug` reading the search path through
/// `first_found`: `ok=8 changed=5` then `ok=8 changed=0`. The play's ninth task, a `shell` reading
/// this engine's own agent cache, has no reference equivalent and does not run under it; it never
/// changes anything (`changed_when: false`) so it does not move either count under Volant either.
/// The `rendered.txt` template renders as `secret=<secret>\nline 1\nline 2\n` (`trim_blocks`, one
/// trailing newline kept); `notes.txt` renders as quoted below.
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
    let expected: [(&str, Vec<u8>, u32); 5] = [
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
        // Produced by running ansible-core 2.19.12's own `ansible-playbook` against `-c local`
        // with the same `notes.j2` template and `note`/`nested` vars: `comment` gave a blank
        // `#` line, the note, and a closing blank `#` line, and `to_nice_yaml` followed with no
        // blank line between them, the whole file ending in exactly one newline.
        (
            "notes.txt",
            b"#\n# rendered over a real ssh link\n#\nkey: value\nlist:\n- one\n- two\n".to_vec(),
            0o644,
        ),
    ];
    for (pass, changed) in [("first", 5), ("second", 0)] {
        let out = run_actions(&inv, secret);
        let text = both(&out);
        assert_eq!(out.status.code(), Some(0), "{pass} pass: {text}");
        assert_recap(&text, "box", changed);
        let found = fixture("ssh/files/greeting.txt");
        assert!(
            text.contains(&format!("\"msg\": \"found={found}\"")),
            "{pass} pass, first_found did not answer the resolved fixture path:\n{text}"
        );
        for (name, bytes, mode) in &expected {
            let path = dest.join(name);
            let written = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("{pass} pass, {}: {e}\n{text}", path.display()));
            assert!(
                written == *bytes,
                "{pass} pass, {} holds {:?} where {:?} was expected",
                path.display(),
                String::from_utf8_lossy(&written),
                String::from_utf8_lossy(bytes)
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
/// and both are read. A file kept past its own task but removed with its connection's directory
/// would not show here; the play's own `No staged file outlives its task` looks for that while
/// the link is still open, and fails the run.
///
/// What would make this red: the agent keeping a staged file after its module ran (that task
/// fails), or keeping a connection's staging directory after the controller left (a `stage-*`
/// entry beside the union).
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
    assert_recap(&text, "box", 5);
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
    for (pass, changed) in [("first", 5), ("second", 0)] {
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

/// Fails unless `VOLANT_PYTHON` sees every version `golden/COLLECTIONS` pins, on ansible-core's
/// own `C.COLLECTIONS_PATHS` (which reads `ANSIBLE_COLLECTIONS_PATH`), the rule `golden.rs`
/// applies to the collection golden. A collection read at another version, or not at all, would
/// make the play below prove something about a module nobody recorded. `ssh-test` always sets
/// `VOLANT_PYTHON`, so there is no skip.
fn assert_the_pinned_collections_are_installed() {
    let python = std::env::var("VOLANT_PYTHON").expect(
        "VOLANT_PYTHON names a python with ansible-core; ssh-test checks this before any ssh_* test runs",
    );
    let mismatched = collections::collection_mismatches(Path::new(&python));
    assert!(
        mismatched.is_empty(),
        "VOLANT_PYTHON's collections do not match COLLECTIONS: {}",
        mismatched.join("; ")
    );
}

/// Two collection modules over a real ssh link, twice: `ansible.posix.sysctl` writing its file
/// only (`sysctl_set` and `reload` both false) and `community.general.ini_file`. The host's
/// pythons have no ansible-core, so both modules and every `module_utils` they import came in the
/// run's union; the first pass changes both files and the second finds them in place.
///
/// The value written is the opposite of the kernel's own, so a `sysctl -w` that ran anyway would
/// either fail the task (this account cannot write `/proc/sys`) or move the value read back.
/// `test.ini` is 23 bytes: the size recorded for the same section, option and value by the
/// collection golden, measured on ansible-core 2.19.12 with community.general 13.4.0.
///
/// What would make this red: a collection module reached by its short name, or missing from the
/// union (a failed task, since no host python could import it); a module that reports `changed`
/// without writing (a file missing or holding something else); a second pass above 0.
#[test]
#[ignore = "needs sshd on localhost and the pinned collections, run through just ssh-test"]
fn ssh_a_collection_module_runs_from_the_union() {
    assert_host_and_controller_pythons_are_distinct();
    assert_the_pinned_collections_are_installed();
    let dir = tmp("collections");
    let inv = inventory(&dir, &[("box", "")]);
    let forward = "/proc/sys/net/ipv4/ip_forward";
    let kernel = std::fs::read_to_string(forward).unwrap();
    let value = if kernel.trim() == "1" { "0" } else { "1" };
    for (pass, changed) in [("first", 2), ("second", 0)] {
        let out = volant_within(
            &[
                "playbook",
                "-i",
                &inv,
                "-e",
                &format!("dest={}", dir.display()),
                "-e",
                &format!("sysctl_value={value}"),
                &fixture("ssh/collections.yml"),
            ],
            Duration::from_secs(180),
        );
        let text = both(&out);
        assert_eq!(out.status.code(), Some(0), "{pass} pass: {text}");
        assert_recap(&text, "box", changed);
        let sysctl = std::fs::read_to_string(dir.join("sysctl.conf"))
            .unwrap_or_else(|e| panic!("{pass} pass, sysctl.conf: {e}\n{text}"));
        assert!(
            sysctl
                .lines()
                .any(|l| l == format!("net.ipv4.ip_forward={value}")),
            "{pass} pass, sysctl.conf holds {sysctl:?}"
        );
        assert_eq!(
            std::fs::read_to_string(forward).unwrap(),
            kernel,
            "{pass} pass: the kernel's own value moved"
        );
        let ini = dir.join("test.ini");
        let written = std::fs::read_to_string(&ini)
            .unwrap_or_else(|e| panic!("{pass} pass, test.ini: {e}\n{text}"));
        assert!(
            written.lines().any(|l| l == "[golden]")
                && written.lines().any(|l| l == "color = blue")
                && written.len() == 23,
            "{pass} pass, test.ini holds {written:?}"
        );
        let mode = std::fs::metadata(&ini).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "{pass} pass, mode of test.ini");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Removes a directory as root however the test holding it ends: the escalated tasks, and the
/// files a test plants as root, leave entries this account cannot remove.
struct RemovedAsRoot(PathBuf);

impl Drop for RemovedAsRoot {
    fn drop(&mut self) {
        remove_as_root(&self.0);
    }
}

/// Every regular file under `root`, relative to it, sorted.
fn files_under(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(at) = todo.pop() {
        for entry in std::fs::read_dir(&at).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                todo.push(path);
            } else {
                found.push(path.strip_prefix(root).unwrap().display().to_string());
            }
        }
    }
    found.sort();
    found
}

/// `fetch` over a real ssh link, with and without `flat`, with and without `become`, twice, then
/// the relative `src` that climbs out of `dest`, with and without `become`, refused both times.
///
/// Measured on ansible-core 2.19.12 against a separate host: `src:
/// ../../../../../../../../tmp/<path>` without `flat` was written on the controller at
/// `/tmp/<path>`, outside `dest`, because the reference checks containment before it has built
/// the path. Here the host is the controller, so the file the host would send and the place the
/// reference would write are the same file: the marker. It is planted root-only, so the escalated
/// task can read it and this account cannot: a fetch that climbed out would find no local sum to
/// compare, write its copy beside the marker and rename it over, and the marker would no longer
/// be root's. The two escalated fetches of a root-only file are what prove `become` took effect.
///
/// What would make this red: the climbing path written (the marker replaced, or a temporary file
/// beside it), or refused for another reason (the sentence); a file written anywhere under `dest`
/// but where the reference puts it; a fetch that reports `changed` without writing the host's
/// bytes; a second pass above 0.
#[test]
#[ignore = "needs sshd on localhost and passwordless sudo, run through just ssh-test"]
fn ssh_fetch_writes_under_dest_and_nowhere_else() {
    assert_passwordless_sudo("the play fetches a root-only file under become, and one is planted");
    // `/tmp` by name rather than `temp_dir()`: the climbing `src` names `/tmp` itself.
    let dir = Path::new("/tmp").join(format!("volant-ssh-fetch-{}", std::process::id()));
    remove_as_root(&dir);
    let _cleanup = RemovedAsRoot(dir.clone());
    let (host, escape, dest) = (dir.join("host"), dir.join("escape"), dir.join("dest"));
    std::fs::create_dir_all(&host).unwrap();
    std::fs::create_dir_all(&escape).unwrap();
    let plain = b"fetched without become\n";
    std::fs::write(host.join("plain.txt"), plain).unwrap();
    let plant = |bytes: &[u8], at: &Path| {
        let staged = dir.join("staged");
        std::fs::write(&staged, bytes).unwrap();
        let (from, to) = (staged.display().to_string(), at.display().to_string());
        as_root(&["install", "-o", "0", "-g", "0", "-m", "0600", &from, &to]);
        std::fs::remove_file(&staged).unwrap();
    };
    let secret = b"fetched under become\n";
    plant(secret, &host.join("secret.txt"));
    let marker = escape.join("esc.txt");
    plant(b"the marker\n", &marker);
    let before = std::fs::symlink_metadata(&marker).unwrap();
    let climb = format!(
        "{}{}",
        "../".repeat(8),
        marker.strip_prefix("/").unwrap().display()
    );
    let refusal = format!(
        "the fetched path {} is outside '{}'",
        marker.display(),
        dest.display()
    );
    let inv = inventory(&dir, &[("box", "")]);
    let under_host = format!("box{}", host.display());
    let expected: [(String, &[u8]); 4] = [
        (format!("{under_host}/plain.txt"), plain),
        (format!("{under_host}/secret.txt"), secret),
        ("flat-become/secret.txt".into(), secret),
        ("flat/plain.txt".into(), plain),
    ];
    for (pass, changed) in [("first", 4), ("second", 0)] {
        let out = volant_within(
            &[
                "playbook",
                "-i",
                &inv,
                "-e",
                &format!("host_dir={}", host.display()),
                "-e",
                &format!("dest={}", dest.display()),
                "-e",
                &format!("climb={climb}"),
                &fixture("ssh/fetch.yml"),
            ],
            Duration::from_secs(180),
        );
        let text = both(&out);
        let after = std::fs::symlink_metadata(&marker)
            .unwrap_or_else(|e| panic!("{pass} pass, the marker is gone: {e}\n{text}"));
        assert!(
            (after.ino(), after.uid(), after.len()) == (before.ino(), 0, before.len()),
            "{pass} pass, something was written over the marker outside dest:\n{text}"
        );
        assert_eq!(
            files_under(&escape),
            ["esc.txt"],
            "{pass} pass, something was written beside the marker:\n{text}"
        );
        assert_eq!(
            text.matches(&refusal).count(),
            2,
            "{pass} pass, both climbing fetches are refused by name:\n{text}"
        );
        assert_eq!(out.status.code(), Some(0), "{pass} pass: {text}");
        assert_recap(&text, "box", changed);
        assert_eq!(
            recap(&text, "box").get("ignored"),
            Some(&2),
            "{pass} pass: {text}"
        );
        let mut want: Vec<&str> = expected.iter().map(|(p, _)| p.as_str()).collect();
        want.sort_unstable();
        assert_eq!(files_under(&dest), want, "{pass} pass: {text}");
        for (path, bytes) in &expected {
            assert!(
                std::fs::read(dest.join(path)).unwrap() == *bytes,
                "{pass} pass, {path} does not hold the host's bytes"
            );
        }
    }
}

/// The machine the native tests run on, and change: the host `VOLANT_TARGET_HOST` names, reached
/// through the caller's own ssh configuration (`just remote ssh-test` exports it on the
/// development machine), or localhost over the test key on a CI runner, which the job throws
/// away. Nowhere else: the play creates an account, a group and a unit, and a developer's own
/// machine is no place for them.
#[cfg(target_os = "linux")]
enum Machine {
    Named(String),
    Localhost,
}

#[cfg(target_os = "linux")]
impl Machine {
    fn from_env() -> Self {
        match std::env::var("VOLANT_TARGET_HOST") {
            Ok(host) if !host.is_empty() => Self::Named(host),
            _ => {
                assert!(
                    std::env::var_os("CI").is_some(),
                    "the native tests change the machine they run on: set VOLANT_TARGET_HOST to a \
                     throwaway host, or run them on a CI runner (CI set), whose localhost goes \
                     away with the job"
                );
                Self::Localhost
            }
        }
    }

    /// One inventory line: `name` reaching this machine, with its agent under `remote_tmp`.
    fn line(&self, name: &str, remote_tmp: &str) -> String {
        match self {
            Self::Named(host) => {
                format!("{name} ansible_host={host} ansible_remote_tmp={remote_tmp}")
            }
            Self::Localhost => format!(
                "{name} ansible_host=127.0.0.1 ansible_user={} ansible_ssh_private_key_file={} ansible_remote_tmp={remote_tmp} ansible_ssh_common_args='-F /dev/null'",
                user(),
                key()
            ),
        }
    }

    /// `script` run on the machine by the login shell, over an `ssh` of its own.
    fn run(&self, script: &str) -> Output {
        let mut ssh = Command::new("ssh");
        ssh.args(["-o", "BatchMode=yes"]);
        match self {
            Self::Named(host) => ssh.arg(host),
            Self::Localhost => ssh
                .args(["-F", "/dev/null", "-o", "StrictHostKeyChecking=no"])
                .args(["-o", "UserKnownHostsFile=/dev/null", "-i", &key()])
                .arg(format!("{}@127.0.0.1", user())),
        };
        ssh.arg(script).output().unwrap()
    }

    /// `rm -rf` of the directories a test left on the machine, as root: an escalated agent's
    /// cache is root's.
    fn remove(&self, paths: &[&str]) {
        let quoted: Vec<String> = paths.iter().map(|p| format!("'{p}'")).collect();
        let out = self.run(&format!("sudo -n rm -rf {}", quoted.join(" ")));
        assert!(
            out.status.success(),
            "removing {paths:?} on the machine: {}",
            both(&out)
        );
    }
}

/// What the native play needs, checked before anything runs so a missing piece fails here, by
/// name, rather than as a case that happens to differ: the recorded ansible-core as the
/// controller's Python, and on the machine `sudo -n`, systemd, apt, a Python for the modules, and
/// none of the throwaway names the play creates and removes.
#[cfg(target_os = "linux")]
fn assert_machine_can_run_the_native_play(machine: &Machine) {
    let recorded = include_str!("golden/ANSIBLE_VERSION").trim();
    let python = std::env::var("VOLANT_PYTHON").unwrap_or_else(|_| {
        panic!("VOLANT_PYTHON must name a python with ansible-core {recorded}")
    });
    let version = Command::new(&python)
        .args([
            "-c",
            "import ansible.release; print(ansible.release.__version__)",
        ])
        .output()
        .unwrap_or_else(|e| panic!("VOLANT_PYTHON does not run: {e}"));
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        recorded,
        "VOLANT_PYTHON must be ansible-core {recorded}, the version the native cases were recorded with"
    );
    let out = machine.run(
        r"sudo -n true 2>/dev/null || echo 'passwordless sudo -n'
test -d /run/systemd/system || echo 'a running systemd'
command -v apt-get >/dev/null || echo 'apt'
command -v python3 >/dev/null || echo 'python3'
getent passwd volantshape >/dev/null && echo 'no volantshape account'
getent passwd 64999 >/dev/null && echo 'no uid 64999'
getent group volantgrp >/dev/null && echo 'no volantgrp group'
getent group volantshape >/dev/null && echo 'no volantshape group'
getent group 64999 >/dev/null && echo 'no gid 64999'
test -e /run/systemd/system/volant-e2e.service && echo 'no volant-e2e unit'
dpkg-query -W volant-nonexistent >/dev/null 2>&1 && echo 'no volant-nonexistent package'
true",
    );
    let missing = String::from_utf8_lossy(&out.stdout)
        .trim()
        .replace('\n', ", ");
    assert!(
        out.status.success() && missing.is_empty(),
        "the native play needs, on the machine under test: {missing} (ssh exit {:?}: {})",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Every native an agent of this release declares, `systemd_service` being `systemd`'s alias.
#[cfg(target_os = "linux")]
const EXPECTED_NATIVES: &[&str] = &[
    "setup",
    "stat",
    "file",
    "copy",
    "lineinfile",
    "apt",
    "systemd",
    "systemd_service",
    "package_facts",
    "service_facts",
    "user",
    "group",
];

/// The cases of `ssh/natives.yml` that have no recording, each with the recorded case of the same
/// module whose index entry it is read under (what may move between two runs, what matches a
/// pattern), and the path its own module must take.
#[cfg(target_os = "linux")]
const EXTRA_CASES: &[(&str, &str, &str)] = &[
    (
        "systemd-unit-started-same",
        "systemd-unit-restarted",
        "native",
    ),
    ("setup-virtual", "setup-pkg-mgr", "fallback"),
    ("stat-sha256", "stat-file", "fallback"),
    (
        "systemd-unit-masked-false",
        "systemd-unit-started",
        "fallback",
    ),
    ("package-facts-all", "package-facts", "fallback"),
    ("group-non-unique", "group-created", "fallback"),
];

/// Task keys that are not the module.
#[cfg(target_os = "linux")]
const TASK_KEYWORDS: &[&str] = &["name", "register", "ignore_errors", "when", "tags"];

/// `tasks` with a `registered-<case>` task after each case, which shows the registered value (it
/// keeps `failed`, which a `fatal:` line drops), and the cases tagged `tag` pushed onto `cases`,
/// each with the index entry it is compared under.
#[cfg(target_os = "linux")]
fn registered_cases(
    tasks: &mut Vec<serde_json::Value>,
    tags: &serde_json::Value,
    tag: &str,
    index: &serde_json::Map<String, serde_json::Value>,
    cases: &mut Vec<(String, serde_json::Value)>,
) {
    use serde_json::{Value, json};
    for mut task in std::mem::take(tasks) {
        let inner = task.get("tags").unwrap_or(tags).clone();
        for part in ["block", "always"] {
            if let Some(Value::Array(nested)) = task.get_mut(part) {
                registered_cases(nested, &inner, tag, index, cases);
            }
        }
        let name = task["name"].as_str().unwrap_or_default().to_string();
        let registered = (task["register"] == "last")
            .then(|| json!({"name": format!("registered-{name}"), "debug": {"var": "last"}}));
        if registered.is_some() && inner.as_array().is_some_and(|t| t.contains(&json!(tag))) {
            let (mut spec, expect) = match EXTRA_CASES.iter().find(|(case, ..)| *case == name) {
                Some((_, like, expect)) => (index[*like].clone(), *expect),
                None => (index[&name].clone(), tag),
            };
            assert!(
                spec.is_object(),
                "{name}: no index entry to compare it under"
            );
            assert_eq!(
                expect, tag,
                "{name}: the `{tag}` block holds a case that expects {expect}"
            );
            let module = task
                .as_object()
                .and_then(|t| t.keys().find(|k| !TASK_KEYWORDS.contains(&k.as_str())))
                .expect("a case names its module")
                .clone();
            spec["module"] = module.into();
            spec["expect"] = expect.into();
            cases.push((name, spec));
        }
        tasks.push(task);
        tasks.extend(registered);
    }
}

/// `ssh/natives.yml` ready to run, and its cases tagged `tag`.
#[cfg(target_os = "linux")]
fn native_play(tag: &str) -> (serde_json::Value, Vec<(String, serde_json::Value)>) {
    let text = std::fs::read_to_string(fixture("ssh/natives.yml")).unwrap();
    let docs = volant::yaml::load(&text, "natives.yml").unwrap();
    let mut play = volant::yaml::to_json(&docs[0]).unwrap();
    let index: serde_json::Value =
        serde_json::from_str(include_str!("golden/native/index.json")).unwrap();
    let mut cases = Vec::new();
    registered_cases(
        play[0]["tasks"].as_array_mut().unwrap(),
        &serde_json::json!([]),
        tag,
        index.as_object().unwrap(),
        &mut cases,
    );
    assert!(
        !cases.is_empty(),
        "ssh/natives.yml has no case tagged {tag}"
    );
    (play, cases)
}

/// Where one test's runs of the native play live: the local directory with the inventory, the
/// play and the profiles, and on the machine the play's directory and the agent's `remote_tmp`.
#[cfg(target_os = "linux")]
struct NativeRun<'a> {
    machine: &'a Machine,
    local: PathBuf,
    dir: String,
    remote_tmp: String,
}

#[cfg(target_os = "linux")]
impl NativeRun<'_> {
    /// One run of `play`'s `tag` cases, natives on (`--facts native`, so `setup` goes to its
    /// native too) or off (`VOLANT_NATIVE_MODULES=0`): its output, and its profile for `box`.
    fn run(
        &self,
        play: &serde_json::Value,
        tag: &str,
        natives: bool,
    ) -> (Output, native_compare::NativeProfile) {
        let side = if natives { "native" } else { "python" };
        let inv = self.local.join("inventory.ini");
        std::fs::write(
            &inv,
            format!("{}\n", self.machine.line("box", &self.remote_tmp)),
        )
        .unwrap();
        // Read instead of the account's own `~/.ansible.cfg`, whose `[volant] native_modules` or
        // `become_user` would change one side's answer.
        let cfg = self.local.join("ansible.cfg");
        std::fs::write(&cfg, "").unwrap();
        let playbook = self.local.join("natives.yml");
        std::fs::write(&playbook, play.to_string()).unwrap();
        let profile = self.local.join(format!("{side}.jsonl"));
        let _ = std::fs::remove_file(&profile);
        let dir_var = format!("dir={}", self.dir);
        let mut args = vec!["playbook", "-vvv", "-i", inv.to_str().unwrap()];
        args.extend(["--tags", tag, "-e", &dir_var]);
        if natives {
            args.extend(["--facts", "native"]);
        }
        args.push(playbook.to_str().unwrap());
        let out = volant_env_within(
            &args,
            &[
                ("VOLANT_PROFILE_JSON", profile.to_str().unwrap()),
                ("ANSIBLE_CONFIG", cfg.to_str().unwrap()),
                ("VOLANT_NATIVE_MODULES", if natives { "1" } else { "0" }),
            ],
            Duration::from_secs(280),
        );
        (out, native_compare::native_profile(&profile, "box"))
    }
}

/// Both runs of the play's `tag` cases, natives on then off, compared case by case: the
/// registered value with the `invocation` its `-vvv` line shows and what the case left behind
/// (`_after`), key by key under the case's index entry, once the run's own directory and staged
/// paths are masked on both sides; each other read-back's `stdout`; and the path each case took,
/// from the profile. Natives on, the agent must declare every native of this release and each case
/// takes its index's path; natives off, every case takes `python`. Both runs must exit 0 and run
/// their cleanup.
#[cfg(target_os = "linux")]
fn compare_native_runs(tag: &str) -> Vec<String> {
    use native_compare::{
        NativeProfile, check_native_path, compare_native, failed_tasks, keep_live, native_result,
        redact_staged, replace_in_strings, results_by_task, same_names,
    };
    let machine = Machine::from_env();
    // Before the machine checks: another run holding the lock is between creating the accounts
    // and removing them.
    let _lock = native_compare::native_lock();
    assert_machine_can_run_the_native_play(&machine);
    let pid = std::process::id();
    let run = NativeRun {
        machine: &machine,
        local: tmp(&format!("natives-{tag}")),
        dir: format!("/var/tmp/volant-e2e-{tag}-{pid}"),
        remote_tmp: format!("/var/tmp/volant-e2e-{tag}-remote-{pid}"),
    };
    let (play, cases) = native_play(tag);
    let mut failures = Vec::new();
    let mut runs = Vec::new();
    for natives in [true, false] {
        let (out, profile) = run.run(&play, tag, natives);
        let side = if natives { "natives on" } else { "natives off" };
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        // Every failing case carries `ignore_errors`, so a sound run exits 0 and runs its
        // cleanup, which fails if anything the play created is left.
        if !out.status.success() {
            failures.push(format!(
                "{side}: volant exited {:?}, where every failure is ignored:\n{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        let results = results_by_task(&stdout);
        if !results.contains_key("cleanup") || failed_tasks(&stdout).contains("cleanup") {
            failures.push(format!(
                "{side}: the cleanup did not run, or left something behind"
            ));
        }
        runs.push((results, profile));
    }
    machine.remove(&[&run.dir, &run.remote_tmp]);
    let _ = std::fs::remove_dir_all(&run.local);
    let [
        (native_results, native_profile),
        (python_results, python_profile),
    ] = <[_; 2]>::try_from(runs).ok().expect("two runs");
    let declared = native_profile.natives.clone().unwrap_or_default();
    for name in EXPECTED_NATIVES {
        if !declared.iter().any(|d| d == name) {
            failures.push(format!("the agent declares no {name} native"));
        }
    }
    // Every case is held to its index's path whatever the agent declared: a native the agent
    // leaves out answers through Python, which `check_native_path` would otherwise accept.
    let native_profile = NativeProfile {
        natives: Some(EXPECTED_NATIVES.iter().map(|n| (*n).to_string()).collect()),
        tasks: native_profile.tasks,
    };
    let python_profile = NativeProfile {
        natives: Some(Vec::new()),
        tasks: python_profile.tasks,
    };
    let masked = |mut value: serde_json::Value| {
        redact_staged(&mut value, &format!("{}/", run.remote_tmp));
        replace_in_strings(&mut value, &run.dir, "<golden-tmp>");
        value
    };
    for (case, spec) in &cases {
        let (mut python, mut native) = match (
            native_result(case, spec, &python_results),
            native_result(case, spec, &native_results),
        ) {
            (Ok(python), Ok(native)) => (masked(python), masked(native)),
            (python, native) => {
                failures.push(format!(
                    "case {case}: natives off {:?}, natives on {:?}",
                    python.err(),
                    native.err()
                ));
                continue;
            }
        };
        if spec["compare"] == "live" {
            same_names(case, &python, &native, &mut failures);
            keep_live(&mut python);
            keep_live(&mut native);
        }
        match (python.as_object(), native.as_object()) {
            (Some(want), Some(got)) => compare_native(case, spec, "", want, got, &mut failures),
            _ => failures.push(format!(
                "case {case}: natives off {python}, natives on {native}"
            )),
        }
        check_native_path(case, spec, &native_profile, &mut failures);
        let mut forced = Vec::new();
        check_native_path(case, spec, &python_profile, &mut forced);
        failures.extend(
            forced
                .into_iter()
                .map(|f| format!("natives off, every task forced to python: {f}")),
        );
    }
    // The read-backs that are no case's `_after`: a package's or an account's state.
    for (task, python) in &python_results {
        if task.starts_with("after-") && python.get("stdout").is_some() {
            let native = native_results.get(task).and_then(|r| r.get("stdout"));
            if native != python.get("stdout") {
                failures.push(format!(
                    "{task}: natives off read {}, natives on {native:?}",
                    python["stdout"]
                ));
            }
        }
    }
    failures
}

/// Every native answers inside its subset what its Python module answers, on a real host, under
/// `become`: `ssh/natives.yml`'s `native` cases, run once natives on and once off.
///
/// What would make this red: a native answering a key, a value, a message or a mode the Python
/// module does not, or leaving a file, a unit or an account otherwise; a native disabled, or
/// handing back where its index says it answers (the path); an agent that answers natively a
/// task the controller told it to run in Python (`force_python`, natives off).
#[cfg(target_os = "linux")]
#[test]
#[ignore = "needs VOLANT_TARGET_HOST or a CI runner, run through just ssh-test"]
fn ssh_natives_match_python_on_a_real_host() {
    let failures = compare_native_runs("native");
    assert!(
        failures.is_empty(),
        "{} difference(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Just outside its subset, each native hands the task back and the Python module answers, with
/// the host in the state Python alone leaves it: `ssh/natives.yml`'s `fallback` cases (a link, a
/// package no archive has, `remote_src`, `backrefs`, a password, ...), run once natives on and
/// once off, their read-backs compared. `service_facts` has no case: it takes no argument, and
/// hands back only on a host outside its subset.
///
/// What would make this red: a native answering outside its subset (the path, or a result that
/// differs), or changing the host before it hands back, so that the Python module meets another
/// state and answers or leaves something else.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "needs VOLANT_TARGET_HOST or a CI runner, run through just ssh-test"]
fn ssh_a_native_falls_back_before_touching_the_host() {
    let failures = compare_native_runs("fallback");
    assert!(
        failures.is_empty(),
        "{} difference(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The play `copy`'s native was measured on: a directory, twenty changed `copy` tasks, the
/// directory removed.
#[cfg(target_os = "linux")]
const TWENTY_COPIES: &str = r#"- hosts: all
  gather_facts: false
  tasks:
    - block:
        - name: directory
          file: {path: "{{ dir }}", state: directory, mode: "0755"}
        - name: copy
          copy: {content: "{{ item }}\n", dest: "{{ dir }}/copy-{{ item }}.txt", mode: "0644"}
          loop: "{{ range(20) | list }}"
      always:
        - name: cleanup
          file: {path: "{{ dir }}", state: absent}
"#;

/// Twenty `copy` tasks that each change a file are twenty native `copy` sub-tasks on a real host.
/// The time the profile gives them is printed, not asserted.
///
/// What would make this red: the native `copy` disabled, or handing back where the plugin stages
/// its source on this host (a staging directory on another file system, for instance).
#[cfg(target_os = "linux")]
#[test]
#[ignore = "needs VOLANT_TARGET_HOST or a CI runner, run through just ssh-test"]
fn ssh_twenty_copies_take_the_native_path() {
    let machine = Machine::from_env();
    let local = tmp("copies");
    let pid = std::process::id();
    let dir = format!("/var/tmp/volant-e2e-copies-{pid}");
    let remote_tmp = format!("/var/tmp/volant-e2e-copies-remote-{pid}");
    let inv = local.join("inventory.ini");
    std::fs::write(&inv, format!("{}\n", machine.line("box", &remote_tmp))).unwrap();
    let playbook = local.join("copies.yml");
    std::fs::write(&playbook, TWENTY_COPIES).unwrap();
    let profile = local.join("profile.jsonl");
    let dir_var = format!("dir={dir}");
    let out = volant_env_within(
        &[
            "playbook",
            "-i",
            inv.to_str().unwrap(),
            "-e",
            &dir_var,
            playbook.to_str().unwrap(),
        ],
        &[("VOLANT_PROFILE_JSON", profile.to_str().unwrap())],
        Duration::from_secs(300),
    );
    machine.remove(&[&dir, &remote_tmp]);
    let text = both(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    // The directory, the loop over the twenty files, and the removal.
    assert_recap(&text, "box", 3);
    let profile = native_compare::native_profile(&profile, "box");
    let _ = std::fs::remove_dir_all(&local);
    let copies: Vec<&serde_json::Value> = profile
        .tasks
        .iter()
        .filter(|line| {
            line["task"] == "copy" && line["module"].as_str().is_some_and(|m| m.ends_with("copy"))
        })
        .collect();
    let paths: Vec<&str> = copies
        .iter()
        .map(|line| line["path"].as_str().unwrap_or("unreported"))
        .collect();
    assert_eq!(
        paths, ["native"; 20],
        "the paths of the twenty copy sub-tasks: {copies:?}"
    );
    let mut micros: Vec<u64> = copies
        .iter()
        .filter_map(|line| line["micros"].as_u64())
        .collect();
    micros.sort_unstable();
    eprintln!(
        "native copy: median {} us over {} sub-tasks",
        micros.get(micros.len() / 2).copied().unwrap_or_default(),
        micros.len()
    );
}

/// Two inventory names for one machine are two hosts, each with a shared connection of its own:
/// one control master per name, left in the control directory for `ControlPersist` after the run.
/// `XDG_RUNTIME_DIR` points that directory under a short `/tmp` path of this test's own, where a
/// socket path stays under the 104 bytes a Unix socket allows.
///
/// What would make this red: no sharing (no socket), or one master keyed by the address alone and
/// shared by both names (one socket).
#[cfg(target_os = "linux")]
#[test]
#[ignore = "needs VOLANT_TARGET_HOST or a CI runner, run through just ssh-test"]
fn ssh_one_connection_per_inventory_host() {
    use std::os::unix::fs::{DirBuilderExt as _, FileTypeExt as _};
    let machine = Machine::from_env();
    let local = tmp("control");
    let pid = std::process::id();
    let runtime = PathBuf::from(format!("/tmp/vcm-{pid}"));
    let _ = std::fs::remove_dir_all(&runtime);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&runtime)
        .unwrap();
    let remote_tmp = format!("/var/tmp/volant-e2e-control-remote-{pid}");
    let inv = local.join("inventory.ini");
    std::fs::write(
        &inv,
        format!(
            "{}\n{}\n",
            machine.line("one", &remote_tmp),
            machine.line("two", &remote_tmp)
        ),
    )
    .unwrap();
    let playbook = local.join("stat.yml");
    std::fs::write(
        &playbook,
        "- hosts: all\n  gather_facts: false\n  tasks:\n    - stat: {path: /}\n",
    )
    .unwrap();
    let out = volant_env_within(
        &[
            "playbook",
            "-i",
            inv.to_str().unwrap(),
            playbook.to_str().unwrap(),
        ],
        &[("XDG_RUNTIME_DIR", runtime.to_str().unwrap())],
        Duration::from_secs(120),
    );
    let sockets: Vec<PathBuf> = std::fs::read_dir(runtime.join("volant-cm"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_ok_and(|t| t.is_socket()))
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default();
    for socket in &sockets {
        let _ = Command::new("ssh")
            .arg("-o")
            .arg(format!("ControlPath={}", socket.display()))
            .args(["-O", "exit", "unused"])
            .output();
    }
    machine.remove(&[&remote_tmp]);
    let _ = std::fs::remove_dir_all(&runtime);
    let _ = std::fs::remove_dir_all(&local);
    let text = both(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert_eq!(
        sockets.len(),
        2,
        "one control master per inventory name, found {sockets:?}; with VOLANT_TARGET_HOST, an \
         ssh configuration that already multiplexes that host keeps Volant's own out:\n{text}"
    );
}
