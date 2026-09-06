// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(unix)]
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::json;
use volant::inventory::Host;
use volant::transport::Transport;
use volant_protocol::{BatchOutcome, FromAgent, Task, ToAgent};

/// The agent is built by the workspace into the same directory as the controller binary.
fn agent_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_volant")).with_file_name("volant-agent")
}

fn local_host() -> Host {
    let mut vars = BTreeMap::new();
    vars.insert("ansible_connection".to_string(), "local".to_string());
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
