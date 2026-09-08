// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end runs over a real `ssh` to an sshd on localhost. Ignored by default: they need
//! `VOLANT_SSH_TEST_KEY` (a private key accepted by localhost) and `VOLANT_AGENT_DIR` holding
//! `volant-agent-<triple>`; `just ssh-test` and the `ssh` CI job provide both.
//!
//! Every test gets its own `ansible_remote_tmp`, so the cache the host is asked about is the
//! one this test put there. Sharing `~/.ansible/tmp` would let one test's upload decide
//! another's outcome, and a test of the "no agent is cached" path would pass or fail on the
//! order the suite happened to run in.
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

fn inventory(dir: &Path, hosts: &[(&str, &str)]) -> String {
    let mut text = String::new();
    for (name, extra) in hosts {
        text.push_str(&format!(
            "{name} ansible_host=127.0.0.1 ansible_user={} ansible_ssh_private_key_file={} ansible_remote_tmp={} {extra}\n",
            user(),
            key(),
            remote_tmp(dir).display(),
        ));
    }
    let path = dir.join("inventory.ini");
    std::fs::write(&path, text).unwrap();
    path.display().to_string()
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
}

#[test]
#[ignore = "needs sshd on localhost, run through just ssh-test"]
fn ssh_refused_connection_is_unreachable_with_a_recap() {
    let dir = tmp("refused");
    let inv = inventory(&dir, &[("dead", "ansible_port=1"), ("box", "")]);
    let out = volant(&["playbook", "-i", &inv, &fixture("ssh/e2e.yml")]);
    let text = both(&out);
    assert!(text.contains("fatal: [dead]: UNREACHABLE!"), "{text}");
    assert!(
        text.contains("ok: [box]"),
        "the other host still runs: {text}"
    );
    assert!(text.contains("PLAY RECAP"), "{text}");
    assert_eq!(out.status.code(), Some(4), "{text}");
}

#[test]
#[ignore = "needs sshd on localhost, run through just ssh-test"]
fn ssh_unknown_host_key_is_refused_when_checking_is_on() {
    let dir = tmp("hostkey");
    let inv = inventory(
        &dir,
        &[(
            "box",
            &format!(
                "ansible_ssh_common_args='-F /dev/null -o UserKnownHostsFile={}/empty_known_hosts'",
                dir.display()
            ),
        )],
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
}
