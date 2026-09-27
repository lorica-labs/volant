// SPDX-License-Identifier: GPL-3.0-or-later
//! The native `group` through the real agent binary: a `groupadd` that hangs ends at the task's
//! `timeout` with the Python path's own answer, and at the controller's cancel.
//!
//! Nothing here changes a group: the hanging `groupadd` is a fake, first on the agent's `PATH`,
//! for a group no host has. The answers themselves are compared with the reference module by
//! the native golden, and the arguments with the module's in the native's own tests.
#![cfg(target_os = "linux")]

#[expect(
    dead_code,
    reason = "the shared helpers include some this file does not use"
)]
mod common;

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Map, json};
use volant_protocol::{
    BatchOutcome, ExecPath, FromAgent, PROTOCOL_VERSION, PythonPayload, Task, TaskResult, ToAgent,
};

/// A directory holding a `groupadd` that hangs, first on the agent's `PATH`; removed at the end.
struct Hung(PathBuf);

impl Hung {
    fn new(name: &str) -> Hung {
        use std::os::unix::fs::PermissionsExt;
        let dir = PathBuf::from(format!(
            "/tmp/volant-hung-group-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let groupadd = dir.join("groupadd");
        std::fs::write(&groupadd, "#!/bin/sh\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&groupadd, std::fs::Permissions::from_mode(0o755)).unwrap();
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
                natives.contains(&"group".to_string()),
                "this agent does not run group natively: {natives:?}"
            ),
            other => panic!("expected Ready, got {other:?}"),
        }
        agent
    }
}

impl Drop for Hung {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Only a Debian or Ubuntu host reaches `groupadd`; elsewhere the native hands back first.
fn debian_here() -> bool {
    let here = std::fs::read_to_string("/etc/os-release").is_ok_and(|text| {
        text.lines()
            .any(|line| matches!(line, "ID=debian" | "ID=ubuntu"))
    });
    if !here {
        eprintln!("this machine is neither Debian nor Ubuntu: the native hands back first");
    }
    here
}

/// A task whose `groupadd` outlives its `timeout` gets the Python path's answer for it, on the
/// native path.
///
/// What would make this red: `groupadd` run without the task's deadline (the agent then waits the
/// whole sleep, past the test's own limit), or a result other than the timeout's.
#[test]
fn a_hung_groupadd_ends_at_the_task_timeout() {
    if !debian_here() {
        return;
    }
    let hung = Hung::new("timeout");
    let mut agent = hung.agent();
    let started = Instant::now();
    agent.send(&ToAgent::RunBatch {
        id: 1,
        tasks: vec![Task {
            timeout: Some(2),
            ..task()
        }],
    });
    let (result, ran) = loop {
        match agent.recv() {
            Some(FromAgent::TaskResult { result, ran, .. }) => break (result, ran.unwrap()),
            Some(FromAgent::Log { .. }) => {}
            other => panic!("expected a task result, got {other:?}"),
        }
    };
    assert_eq!(ran.path, ExecPath::Native, "{:?}", ran.reason);
    assert_eq!(result, TaskResult::timed_out(2));
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the hung command was waited for"
    );
}

/// The controller's cancel stops the native while `groupadd` hangs.
///
/// What would make this red: the cancel not asked while the command runs, which leaves the
/// batch running until the sleep ends.
#[test]
fn a_cancel_stops_a_hung_groupadd() {
    if !debian_here() {
        return;
    }
    let hung = Hung::new("cancel");
    let mut agent = hung.agent();
    let started = Instant::now();
    agent.send(&ToAgent::RunBatch {
        id: 7,
        tasks: vec![task()],
    });
    std::thread::sleep(Duration::from_millis(500));
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

/// A `group` task creating a group no host has, whose payload the agent does not hold: the
/// Python path would show as that path's own failure.
fn task() -> Task {
    let args = json!({"name": format!("volanthungg{}", std::process::id())});
    Task {
        module: "group".into(),
        args: args.as_object().unwrap().clone(),
        ignore_errors: true,
        payload: Some(PythonPayload {
            blob: "0".repeat(64),
            module_fqn: "ansible.modules.group".into(),
            profile: "legacy".into(),
            rlimit_nofile: 0,
            extensions: Map::new(),
            interpreter: "/usr/bin/python3".into(),
        }),
        ..Task::default()
    }
}
