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

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
        text.push_str(&format!(
            "{name} ansible_host=127.0.0.1 ansible_user={} ansible_ssh_private_key_file={} ansible_remote_tmp={} ansible_ssh_common_args='{common_args}' {extra}\n",
            user(),
            key(),
            remote_tmp(dir).display(),
        ));
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
