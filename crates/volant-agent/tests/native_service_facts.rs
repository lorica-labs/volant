// SPDX-License-Identifier: GPL-3.0-or-later
//! The native `service_facts` through the real agent binary: a command that hangs ends at the
//! task's `timeout` with the Python path's own answer, and at the controller's cancel.
//!
//! The answer itself is compared with the reference module elsewhere, live, by the native
//! golden; the parsing is held to the reference's own on recorded outputs in the module's tests.
#![cfg(target_os = "linux")]

#[expect(
    dead_code,
    reason = "the shared helpers include some this file does not use"
)]
mod common;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Map;
use volant_protocol::{
    BatchOutcome, ExecPath, FromAgent, PROTOCOL_VERSION, PythonPayload, Task, TaskResult, ToAgent,
};

/// A directory holding a `service` that hangs, first on the agent's `PATH`, so the SysV listing
/// never ends; removed at the end.
struct HungService(PathBuf);

impl HungService {
    fn new(name: &str) -> HungService {
        use std::os::unix::fs::PermissionsExt;
        let dir = PathBuf::from(format!(
            "/tmp/volant-hung-service-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let service = dir.join("service");
        std::fs::write(&service, "#!/bin/sh\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&service, std::fs::Permissions::from_mode(0o755)).unwrap();
        HungService(dir)
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
                natives.contains(&"service_facts".to_string()),
                "this agent does not run service_facts natively: {natives:?}"
            ),
            other => panic!("expected Ready, got {other:?}"),
        }
        agent
    }
}

impl Drop for HungService {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Only a host run by systemd reaches the commands; elsewhere the native hands back first.
fn systemd_here() -> bool {
    let here = Path::new("/run/systemd/system").is_dir();
    if !here {
        eprintln!("this machine is not run by systemd: the native hands back before any command");
    }
    here
}

/// A task that outlives its `timeout` while `service --status-all` hangs gets the Python path's
/// answer for it, on the native path.
///
/// What would make this red: the SysV listing run without the task's deadline (the agent then
/// waits the whole sleep, past the test's own limit), or a result other than the timeout's.
#[test]
fn a_hung_listing_ends_at_the_task_timeout() {
    if !systemd_here() {
        return;
    }
    let hung = HungService::new("timeout");
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

/// The controller's cancel stops the native while `service --status-all` hangs.
///
/// What would make this red: the cancel not asked while the listings run, which leaves the
/// batch running until the sleep ends.
#[test]
fn a_cancel_stops_a_hung_listing() {
    if !systemd_here() {
        return;
    }
    let hung = HungService::new("cancel");
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

/// A `service_facts` task whose payload the agent does not hold: the Python path would show as
/// that path's own failure.
fn task() -> Task {
    Task {
        module: "service_facts".into(),
        ignore_errors: true,
        payload: Some(PythonPayload {
            blob: "0".repeat(64),
            module_fqn: "ansible.modules.service_facts".into(),
            profile: "legacy".into(),
            rlimit_nofile: 0,
            extensions: Map::new(),
            interpreter: "/usr/bin/python3".into(),
        }),
        ..Task::default()
    }
}
