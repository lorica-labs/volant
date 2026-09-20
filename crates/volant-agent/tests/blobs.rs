// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(unix)]

use std::fs;
use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use volant_protocol::frame::{read_frame, write_frame};
use volant_protocol::{FromAgent, LogLevel, Task, ToAgent};

/// The agent answers for a payload it does not hold, keeps one whose bytes match its name, and
/// refuses one whose bytes do not - each with a `BlobState` the controller can wait on.
///
/// What would make this red: a `put_blob` answered with nothing, which leaves the controller
/// waiting on a state that never comes; or a refused payload answered `present: true`, which
/// sends the batch at a blob that is not there and fails every task in it.
#[test]
fn the_agent_answers_for_a_blob_and_refuses_one_whose_bytes_do_not_match() {
    let dir = std::env::temp_dir().join(format!("volant-agent-blobs-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    // The agent reads this the way it reads the directory it was cached in, so the test owns
    // the cache it then looks at.
    let mut child = Command::new(env!("CARGO_BIN_EXE_volant-agent"))
        .env("VOLANT_REMOTE_TMP", &dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("agent starts");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    {
        let mut send = |msg: &ToAgent| {
            write_frame(&mut stdin, &serde_json::to_vec(msg).unwrap()).unwrap();
            stdin.flush().unwrap();
        };
        let mut recv = || -> FromAgent {
            let bytes = read_frame(&mut stdout).unwrap().expect("the agent answers");
            serde_json::from_slice(&bytes).unwrap()
        };

        let zip = b"PK\x03\x04";
        let hash = blake3::hash(zip).to_hex().to_string();

        send(&ToAgent::HasBlob { hash: hash.clone() });
        assert_eq!(
            recv(),
            FromAgent::BlobState {
                hash: hash.clone(),
                present: false,
            }
        );

        send(&ToAgent::PutBlob {
            hash: hash.clone(),
            zip_b64: "UEsDBA==".into(),
        });
        assert_eq!(
            recv(),
            FromAgent::BlobState {
                hash: hash.clone(),
                present: true,
            }
        );
        // SAFETY: `geteuid` reads the calling process's own credentials and cannot fail.
        let euid = unsafe { libc::geteuid() };
        let at: PathBuf = dir
            .join(format!("volant-blobs-{}-{euid}", env!("CARGO_PKG_VERSION")))
            .join(format!("{hash}.zip"));
        assert_eq!(fs::read(&at).unwrap(), zip, "the payload landed at {at:?}");

        send(&ToAgent::HasBlob { hash: hash.clone() });
        assert_eq!(
            recv(),
            FromAgent::BlobState {
                hash: hash.clone(),
                present: true,
            }
        );

        // The bytes under the name are what is answered for, never the name. A local user who
        // can write `remote_tmp` computes the hash of a payload offline from a released
        // ansible-core; if the agent answered from the name, the controller would never re-send
        // and task 3 would run the planted zip.
        fs::write(&at, b"a zip of the planter's choosing").unwrap();
        send(&ToAgent::HasBlob { hash: hash.clone() });
        assert_eq!(
            recv(),
            FromAgent::BlobState {
                hash: hash.clone(),
                present: false,
            },
            "a planted blob is not answered for"
        );
        send(&ToAgent::PutBlob {
            hash: hash.clone(),
            zip_b64: "UEsDBA==".into(),
        });
        assert_eq!(
            recv(),
            FromAgent::BlobState {
                hash: hash.clone(),
                present: true,
            }
        );
        assert_eq!(
            fs::read(&at).unwrap(),
            zip,
            "the planted bytes are replaced"
        );

        let wrong = "0".repeat(64);
        send(&ToAgent::PutBlob {
            hash: wrong.clone(),
            zip_b64: "UEsDBA==".into(),
        });
        match recv() {
            FromAgent::Log { level, message } => {
                assert_eq!(level, LogLevel::Error);
                assert!(
                    message.contains(&wrong) && message.contains(&hash),
                    "{message}"
                );
            }
            other => panic!("expected a log naming both hashes, got {other:?}"),
        }
        assert_eq!(
            recv(),
            FromAgent::BlobState {
                hash: wrong,
                present: false,
            }
        );
    }
    drop(stdin);
    assert!(
        child.wait().unwrap().success(),
        "the agent ends at end of stream"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// A `put_blob` that arrives while a batch is running is answered before the batch ends.
///
/// What would make this red: the message logged to the agent's stderr and discarded, which is
/// what happened until now - a controller that sends the payload for the next play while a batch
/// is still in flight would wait for a `BlobState` that never comes, and a wait with nothing
/// behind it is a hang rather than a failure.
#[test]
fn a_put_blob_that_arrives_during_a_batch_is_answered() {
    let dir = std::env::temp_dir().join(format!("volant-agent-batch-blob-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_volant-agent"))
        .env("VOLANT_REMOTE_TMP", &dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("agent starts");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let hash = blake3::hash(b"PK\x03\x04").to_hex().to_string();
    {
        let mut send = |msg: &ToAgent| {
            write_frame(&mut stdin, &serde_json::to_vec(msg).unwrap()).unwrap();
            stdin.flush().unwrap();
        };
        let mut recv = || -> FromAgent {
            let bytes = read_frame(&mut stdout).unwrap().expect("the agent answers");
            serde_json::from_slice(&bytes).unwrap()
        };

        // A task long enough for the next frame to arrive while it runs, and one the agent
        // checks the control channel during.
        send(&ToAgent::RunBatch {
            id: 1,
            tasks: vec![Task {
                module: "command".into(),
                args: serde_json::json!({"_raw_params": "sleep 1"})
                    .as_object()
                    .unwrap()
                    .clone(),
                ignore_errors: false,
                timeout: None,
                environment: std::collections::BTreeMap::new(),
                payload: None,
            }],
        });
        send(&ToAgent::PutBlob {
            hash: hash.clone(),
            zip_b64: "UEsDBA==".into(),
        });

        let mut answered = false;
        loop {
            match recv() {
                FromAgent::BlobState {
                    hash: which,
                    present,
                } => {
                    assert_eq!(which, hash);
                    assert!(present);
                    answered = true;
                }
                FromAgent::BatchDone { .. } => break,
                _ => {}
            }
        }
        assert!(
            answered,
            "the batch ended without the payload ever being answered for"
        );
    }
    drop(stdin);
    let _ = child.wait();
    let _ = fs::remove_dir_all(&dir);
}
