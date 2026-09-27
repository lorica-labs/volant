// SPDX-License-Identifier: GPL-3.0-or-later
//! The native `user` through the real agent binary: a `useradd` that hangs ends at the task's
//! `timeout` with the Python path's own answer, and at the controller's cancel.
//!
//! Nothing here changes an account: the hanging `useradd` is a fake, first on the agent's `PATH`,
//! for an account no host has. The answers themselves are compared with the reference module by
//! the native golden, and the arguments with the module's in the native's own tests.
#![cfg(target_os = "linux")]

#[expect(
    dead_code,
    reason = "the shared helpers include some this file does not use"
)]
mod common;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Map, json};
use volant_protocol::{
    BatchOutcome, ExecPath, FromAgent, PROTOCOL_VERSION, PythonPayload, Task, TaskResult, ToAgent,
};

/// A directory holding a `useradd` that marks it started and hangs, first on the agent's `PATH`;
/// removed at the end.
struct Hung(PathBuf);

impl Hung {
    fn new(name: &str) -> Hung {
        use std::os::unix::fs::PermissionsExt;
        let dir = PathBuf::from(format!(
            "/tmp/volant-hung-user-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let useradd = dir.join("useradd");
        let script = format!(
            "#!/bin/sh\ntouch {}/started\nexec sleep 60\n",
            dir.display()
        );
        std::fs::write(&useradd, script).unwrap();
        std::fs::set_permissions(&useradd, std::fs::Permissions::from_mode(0o755)).unwrap();
        Hung(dir)
    }

    fn agent(&self) -> common::Agent {
        let mut path = OsString::from(self.0.as_os_str());
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        let mut agent = common::spawn_agent_with_path(&path);
        agent.send(&ToAgent::Hello {
            protocol: PROTOCOL_VERSION,
        });
        match agent.recv() {
            Some(FromAgent::Ready { natives, .. }) => assert!(
                natives.contains(&"user".to_string()),
                "this agent does not run user natively: {natives:?}"
            ),
            other => panic!("expected Ready, got {other:?}"),
        }
        agent
    }

    /// Waits, at most 20 seconds, until the fake command has started.
    fn started(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.0.join("started").exists() {
            assert!(Instant::now() < deadline, "the hung command never started");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Hung {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Whether this host is one the native answers for, by the native's own gate: the last `ID=` of
/// `/etc/os-release` (quotes stripped) is Debian or Ubuntu, SELinux is off, and `nsswitch.conf`
/// asks `files` first for accounts and groups.
fn in_subset() -> bool {
    let id = std::fs::read_to_string("/etc/os-release")
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.strip_prefix("ID="))
        .next_back()
        .map(|id| id.trim_matches(['"', '\'']).to_lowercase());
    let conf = std::fs::read_to_string("/etc/nsswitch.conf").ok();
    let files_first = |db: &str| {
        conf.as_deref().is_none_or(|conf| {
            conf.lines()
                .filter_map(|line| line.split('#').next()?.trim().strip_prefix(db))
                .find_map(|rest| rest.trim_start().strip_prefix(':'))
                .is_none_or(|sources| {
                    matches!(sources.split_whitespace().next(), Some("files" | "compat"))
                })
        })
    };
    matches!(id.as_deref(), Some("debian" | "ubuntu"))
        && !Path::new("/sys/fs/selinux/enforce").exists()
        && files_first("passwd")
        && files_first("group")
}

/// Runs one task and returns its result and how the agent ran it.
fn run_one(agent: &mut common::Agent, task: Task) -> (TaskResult, volant_protocol::Ran) {
    agent.send(&ToAgent::RunBatch {
        id: 1,
        tasks: vec![task],
    });
    loop {
        match agent.recv() {
            Some(FromAgent::TaskResult { result, ran, .. }) => break (result, ran.unwrap()),
            Some(FromAgent::Log { .. }) => {}
            other => panic!("expected a task result, got {other:?}"),
        }
    }
}

/// On a host outside the subset the native hands the task back before `useradd`, which this
/// checks instead; `true` when it did.
fn handed_back_here(hung: &Hung) -> bool {
    if in_subset() {
        return false;
    }
    let (_, ran) = run_one(&mut hung.agent(), task());
    assert_eq!(ran.path, ExecPath::Fallback, "{:?}", ran.reason);
    assert!(!hung.0.join("started").exists(), "useradd ran");
    eprintln!("this host is outside the native's subset: {:?}", ran.reason);
    true
}

/// A task whose `useradd` outlives its `timeout` gets the Python path's answer for it, on the
/// native path.
///
/// What would make this red: `useradd` run without the task's deadline (the agent then waits the
/// whole sleep, past the test's own limit), or a result other than the timeout's.
#[test]
fn a_hung_useradd_ends_at_the_task_timeout() {
    let hung = Hung::new("timeout");
    if handed_back_here(&hung) {
        return;
    }
    let mut agent = hung.agent();
    let started = Instant::now();
    let (result, ran) = run_one(
        &mut agent,
        Task {
            timeout: Some(2),
            ..task()
        },
    );
    assert_eq!(ran.path, ExecPath::Native, "{:?}", ran.reason);
    assert_eq!(result, TaskResult::timed_out(2));
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the hung command was waited for"
    );
}

/// The controller's cancel stops the native while `useradd` hangs: sent once the command has
/// started, so it reaches the command, not the task before it.
///
/// What would make this red: the cancel not asked while the command runs, which leaves the
/// batch running until the sleep ends.
#[test]
fn a_cancel_stops_a_hung_useradd() {
    let hung = Hung::new("cancel");
    if handed_back_here(&hung) {
        return;
    }
    let mut agent = hung.agent();
    let started = Instant::now();
    agent.send(&ToAgent::RunBatch {
        id: 7,
        tasks: vec![task()],
    });
    hung.started();
    agent.send(&ToAgent::Cancel { id: 7 });
    let outcome = loop {
        match agent.recv() {
            Some(FromAgent::BatchDone { outcome, .. }) => break outcome,
            Some(FromAgent::TaskResult { .. } | FromAgent::Log { .. }) => {}
            other => panic!("expected the batch to end, got {other:?}"),
        }
    };
    assert_eq!(outcome, BatchOutcome::Cancelled { at: 0 });
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the hung command was waited for"
    );
}

/// A `user` task creating an account no host has, whose payload the agent does not hold: the
/// Python path would show as that path's own failure.
fn task() -> Task {
    let args = json!({"name": format!("volanthung{}", std::process::id()), "create_home": false});
    Task {
        module: "user".into(),
        args: args.as_object().unwrap().clone(),
        ignore_errors: true,
        payload: Some(PythonPayload {
            blob: "0".repeat(64),
            module_fqn: "ansible.modules.user".into(),
            profile: "legacy".into(),
            rlimit_nofile: 0,
            extensions: Map::new(),
            interpreter: "/usr/bin/python3".into(),
        }),
        ..Task::default()
    }
}
