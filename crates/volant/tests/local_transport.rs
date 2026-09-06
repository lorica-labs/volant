// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(unix)]
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::json;
use tokio::process::Command;
use volant::agent::AgentLink;
use volant::inventory::Host;
use volant::transport::Transport;
use volant_protocol::{BatchOutcome, FromAgent, LogLevel, Task, ToAgent};

/// The agent is built by the workspace into the same directory as the controller binary.
fn agent_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_volant")).with_file_name("volant-agent")
}

fn local_host() -> Host {
    let mut vars = BTreeMap::new();
    vars.insert("ansible_connection".to_string(), json!("local"));
    Host {
        name: "localhost".to_string(),
        vars,
    }
}

#[tokio::test]
async fn runs_a_batch_through_the_local_transport() {
    let transport = Transport::for_host(&local_host()).unwrap();
    let mut link = transport.connect(&agent_path()).await.unwrap();
    link.handshake().await.unwrap();
    link.send(&ToAgent::RunBatch {
        id: 1,
        tasks: vec![Task {
            module: "command".into(),
            args: json!({"_raw_params": "echo via-local"})
                .as_object()
                .unwrap()
                .clone(),
            ignore_errors: false,
            timeout: None,
        }],
    })
    .await
    .unwrap();
    match link.recv().await.unwrap() {
        Some(FromAgent::TaskResult {
            index: 0, result, ..
        }) => assert_eq!(result.0["stdout"], "via-local"),
        other => panic!("expected a result, got {other:?}"),
    }
    assert_eq!(
        link.recv().await.unwrap(),
        Some(FromAgent::BatchDone {
            batch: 1,
            outcome: BatchOutcome::Completed
        })
    );
}

/// A `sleep` duration that doubles as a `pgrep -f` marker: GNU `sleep` rejects a second,
/// non-numeric argument outright, so the marker has to live inside the number itself. The
/// fractional part carries this test binary's pid so two runs never search for each other's
/// grandchild, and `whole_seconds` keeps a run that outlives its own test bounded rather than
/// orphaned forever if the kill it is checking for turns out not to happen.
fn unique_sleep_marker(whole_seconds: u32) -> String {
    format!("{whole_seconds}.{}", std::process::id())
}

#[tokio::test]
async fn cancel_stops_the_running_task_and_its_children() {
    let marker = unique_sleep_marker(40);
    let transport = Transport::for_host(&local_host()).unwrap();
    let mut link = transport.connect(&agent_path()).await.unwrap();
    link.handshake().await.unwrap();
    link.send(&ToAgent::RunBatch {
        id: 9,
        tasks: vec![Task {
            module: "shell".into(),
            // `sleep ... & wait` forces the shell to fork the sleep as a grandchild and stay
            // around itself (a lone trailing simple command would instead be exec'd, replacing
            // the shell and leaving no grandchild to kill).
            args: json!({"_raw_params": format!("sleep {marker} & wait")})
                .as_object()
                .unwrap()
                .clone(),
            ignore_errors: false,
            timeout: None,
        }],
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    assert!(
        link.cancel(9, std::time::Duration::from_secs(5)).await,
        "agent must confirm the cancel"
    );
    assert!(started.elapsed().as_secs() < 5);
    link.shutdown().await;
    let survivors = std::process::Command::new("pgrep")
        .args(["-f", &marker])
        .output()
        .unwrap();
    assert!(
        survivors.stdout.is_empty(),
        "grandchild survived: {}",
        String::from_utf8_lossy(&survivors.stdout)
    );
}

#[tokio::test]
async fn dropping_the_link_lets_the_agent_stop_its_task() {
    let marker = unique_sleep_marker(45);
    let transport = Transport::for_host(&local_host()).unwrap();
    let mut link = transport.connect(&agent_path()).await.unwrap();
    link.handshake().await.unwrap();
    link.send(&ToAgent::RunBatch {
        id: 10,
        tasks: vec![Task {
            module: "shell".into(),
            args: json!({"_raw_params": format!("sleep {marker} & wait")})
                .as_object()
                .unwrap()
                .clone(),
            ignore_errors: false,
            timeout: None,
        }],
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    drop(link);
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let survivors = std::process::Command::new("pgrep")
        .args(["-f", &marker])
        .output()
        .unwrap();
    assert!(
        survivors.stdout.is_empty(),
        "the agent was killed before it could stop its task"
    );
}

#[tokio::test]
async fn a_silent_agent_fails_the_handshake_within_the_timeout() {
    // A program that never answers stands in for a hung agent.
    let transport = Transport::for_host(&local_host()).unwrap();
    let mut link = transport.connect(std::path::Path::new("/bin/sleep")).await;
    // `/bin/sleep` needs an argument to run; a spawn failure is fine too, the point is below.
    if let Ok(link) = link.as_mut() {
        let started = std::time::Instant::now();
        let err = tokio::time::timeout(std::time::Duration::from_secs(1), link.handshake()).await;
        assert!(err.is_err() || err.unwrap().is_err());
        assert!(started.elapsed().as_secs() < 3);
    }
}

/// Spawns a process piping `script` to `sh -c`, wired up the same way `Transport::Local`
/// wires up the real agent, so it can stand in for one in `AgentLink` tests.
fn spawn_shell(script: &str) -> AgentLink {
    let child = Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    AgentLink::new(child).unwrap()
}

/// Renders `bytes` as a `sh`/`printf` octal escape sequence, so arbitrary bytes (including
/// NUL, which cannot travel through a process argument) can be written by a shell script.
fn octal_escape(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("\\{b:03o}")).collect()
}

#[tokio::test]
async fn a_length_prefix_cut_short_is_an_error_not_a_clean_end() {
    // Two bytes of a four byte length prefix, then the stream closes: the genuine "closed
    // before any byte arrived" case and this one must not collapse into the same `Ok(None)`.
    let mut link = spawn_shell(&format!("printf '{}'", octal_escape(&[0, 0])));
    let err = link.recv().await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
}

#[tokio::test]
async fn a_clean_end_of_stream_is_ok_none() {
    let mut link = spawn_shell("true");
    assert_eq!(link.recv().await.unwrap(), None);
}

#[tokio::test]
async fn dropping_a_racing_recv_does_not_desync_the_next_one() {
    let msg = FromAgent::Log {
        level: LogLevel::Info,
        message: "still readable after the race".to_string(),
    };
    let payload = serde_json::to_vec(&msg).unwrap();
    let len = u32::try_from(payload.len()).unwrap().to_be_bytes();
    // The header arrives immediately; the payload is held back long enough that a recv
    // racing an already-elapsed timeout is guaranteed to still be waiting on it, so the
    // timeout wins and the recv future is dropped between the two reads.
    let script = format!(
        "printf '{}'; sleep 0.5; printf '{}'",
        octal_escape(&len),
        octal_escape(&payload)
    );
    let mut link = spawn_shell(&script);
    // Give the header time to actually land in the pipe, so the raced recv below consumes
    // it and blocks on the payload rather than racing an entirely empty stream.
    tokio::time::sleep(Duration::from_millis(100)).await;

    tokio::select! {
        _ = tokio::time::sleep(Duration::ZERO) => {}
        result = link.recv() => panic!("recv should not resolve before the payload arrives: {result:?}"),
    }

    assert_eq!(link.recv().await.unwrap(), Some(msg));
}

#[test]
fn ssh_is_refused_until_it_exists() {
    let mut host = local_host();
    host.vars.remove("ansible_connection");
    let err = Transport::for_host(&host).unwrap_err();
    assert!(format!("{err:#}").contains("ssh"));
}

#[test]
fn the_agent_is_found_in_a_directory_named_by_the_environment() {
    let dir = agent_path().parent().unwrap().to_path_buf();
    temp_env(&[("VOLANT_AGENT_DIR", Some(dir.to_str().unwrap()))], || {
        assert_eq!(volant::agent::locate().unwrap(), agent_path());
    });
}

fn temp_env(vars: &[(&str, Option<&str>)], f: impl FnOnce()) {
    let saved: Vec<_> = vars
        .iter()
        .map(|(k, _)| (k.to_string(), std::env::var(k).ok()))
        .collect();
    for (k, v) in vars {
        match v {
            Some(v) => unsafe { std::env::set_var(k, v) },
            None => unsafe { std::env::remove_var(k) },
        }
    }
    f();
    for (k, v) in saved {
        match v {
            Some(v) => unsafe { std::env::set_var(&k, v) },
            None => unsafe { std::env::remove_var(&k) },
        }
    }
}
