// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(unix)]
mod common;

use common::spawn_agent;
use serde_json::json;
use volant_protocol::{BatchOutcome, FromAgent, LogLevel, PROTOCOL_VERSION, Task, ToAgent};

fn command(cmd: &str, ignore_errors: bool) -> Task {
    Task {
        module: "command".into(),
        args: json!({"_raw_params": cmd}).as_object().unwrap().clone(),
        ignore_errors,
    }
}

fn result_of(msg: Option<FromAgent>) -> (usize, volant_protocol::TaskResult) {
    match msg {
        Some(FromAgent::TaskResult { index, result, .. }) => (index, result),
        other => panic!("expected TaskResult, got {other:?}"),
    }
}

#[test]
fn a_failure_stops_the_batch() {
    let mut agent = spawn_agent();
    agent.send(&ToAgent::Hello {
        protocol: PROTOCOL_VERSION,
    });
    assert!(matches!(agent.recv(), Some(FromAgent::Ready { .. })));
    agent.send(&ToAgent::RunBatch {
        id: 1,
        tasks: vec![
            command("echo first", false),
            command("false", false),
            command("echo never", false),
        ],
    });
    let (i, r) = result_of(agent.recv());
    assert_eq!((i, r.0["stdout"].as_str()), (0, Some("first")));
    let (i, r) = result_of(agent.recv());
    assert_eq!(i, 1);
    assert!(r.failed());
    assert_eq!(
        agent.recv(),
        Some(FromAgent::BatchDone {
            batch: 1,
            outcome: BatchOutcome::Failed { at: 1 }
        })
    );
    agent.close();
    agent.child.wait().unwrap();
}

#[test]
fn ignore_errors_lets_the_batch_continue() {
    let mut agent = spawn_agent();
    agent.send(&ToAgent::Hello {
        protocol: PROTOCOL_VERSION,
    });
    agent.recv();
    agent.send(&ToAgent::RunBatch {
        id: 2,
        tasks: vec![command("false", true), command("echo after", false)],
    });
    assert!(result_of(agent.recv()).1.failed());
    assert_eq!(result_of(agent.recv()).1.0["stdout"], "after");
    assert_eq!(
        agent.recv(),
        Some(FromAgent::BatchDone {
            batch: 2,
            outcome: BatchOutcome::Completed
        })
    );
    agent.close();
    agent.child.wait().unwrap();
}

#[test]
fn cancel_interrupts_a_running_task() {
    let mut agent = spawn_agent();
    agent.send(&ToAgent::Hello {
        protocol: PROTOCOL_VERSION,
    });
    agent.recv();
    agent.send(&ToAgent::RunBatch {
        id: 3,
        tasks: vec![command("sleep 30", false), command("echo never", false)],
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    let started = std::time::Instant::now();
    agent.send(&ToAgent::Cancel { id: 3 });
    assert_eq!(
        agent.recv(),
        Some(FromAgent::BatchDone {
            batch: 3,
            outcome: BatchOutcome::Cancelled { at: 0 }
        })
    );
    assert!(started.elapsed().as_secs() < 5);
    agent.close();
    agent.child.wait().unwrap();
}

#[test]
fn a_broken_stdin_during_a_batch_is_logged_and_exits_non_zero() {
    let mut agent = spawn_agent();
    agent.send(&ToAgent::Hello {
        protocol: PROTOCOL_VERSION,
    });
    agent.recv();
    agent.send(&ToAgent::RunBatch {
        id: 5,
        tasks: vec![command("sleep 30", false)],
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    // A frame header declaring a body, then stdin closes before the body arrives:
    // read_frame reports UnexpectedEof rather than a clean end of stream.
    agent.write_raw(&9u32.to_be_bytes());
    agent.close();
    let msg = agent.recv();
    assert!(
        matches!(
            &msg,
            Some(FromAgent::Log {
                level: LogLevel::Error,
                ..
            })
        ),
        "expected an error log, got {msg:?}"
    );
    let status = agent.child.wait().unwrap();
    assert!(!status.success());
}

#[test]
fn a_vanished_controller_stops_the_batch() {
    let mut agent = spawn_agent();
    agent.send(&ToAgent::Hello {
        protocol: PROTOCOL_VERSION,
    });
    agent.recv();
    agent.send(&ToAgent::RunBatch {
        id: 4,
        tasks: vec![command("sleep 30", false)],
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    let started = std::time::Instant::now();
    agent.close();
    let status = agent.child.wait().unwrap();
    assert!(status.success());
    assert!(
        started.elapsed().as_secs() < 5,
        "agent must not wait for the sleep"
    );
}
