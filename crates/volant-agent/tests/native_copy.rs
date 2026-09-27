// SPDX-License-Identifier: GPL-3.0-or-later
//! The native `copy` through the real agent binary: the file the controller staged is moved into
//! place, a case outside the subset reaches the payload with nothing changed, and a `validate`
//! that hangs ends at the task's `timeout` and at the controller's cancel.
//!
//! The answer itself is held to the reference's key by key in the module's own tests and by the
//! native golden.
#![cfg(target_os = "linux")]

use std::io::{BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use volant_protocol::frame::{read_frame, write_frame};
use volant_protocol::{
    BatchOutcome, BlobEncoding, ExecPath, FromAgent, PROTOCOL_VERSION, PythonPayload, Ran,
    StagedFile, Task, TaskResult, ToAgent,
};

/// A changed `copy` moves the staged file over `dest`: the content arrives, the staged file is
/// gone, and the task went the native path.
///
/// What would make this red: `copy` not enabled in this agent, or the native handing back on the
/// action plugin's own arguments.
#[test]
fn a_staged_file_is_moved_into_place() {
    let scratch = Scratch::new("moved");
    let mut agent = Agent::spawn(&scratch.0);
    let natives = agent.hello();
    assert!(natives.contains(&"copy".to_string()), "{natives:?}");
    let dest = scratch.0.join("dest.txt");
    let (result, ran) = agent.copy(1, b"moved\n", json!({"dest": dest}), None);
    assert_eq!(ran.path, ExecPath::Native, "{:?}", ran.reason);
    assert_eq!(result.0["changed"], true, "{result:?}");
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "moved\n");
    let src = result.0["src"].as_str().expect("src is in the answer");
    assert!(
        !Path::new(src).exists(),
        "the staged file outlived its task"
    );
}

/// A case outside the subset goes to the payload, and the destination is as it was.
///
/// What would make this red: `directory_mode` answered by the native.
#[test]
fn a_case_outside_the_subset_reaches_the_payload() {
    let scratch = Scratch::new("outside");
    let mut agent = Agent::spawn(&scratch.0);
    agent.hello();
    let dest = scratch.0.join("dest.txt");
    let (result, ran) = agent.copy(
        1,
        b"x\n",
        json!({"dest": dest, "directory_mode": "0755"}),
        None,
    );
    assert_eq!(ran.path, ExecPath::Fallback, "{ran:?}");
    assert_eq!(ran.reason.as_deref(), Some("directory_mode is set"));
    assert!(
        result.0["msg"]
            .as_str()
            .is_some_and(|msg| msg.starts_with(&format!("payload {}", "0".repeat(64)))),
        "not the Python path's answer: {result:?}"
    );
    assert!(!dest.exists());
}

/// A `validate` that hangs ends at the task's `timeout` with the Python path's answer for it.
///
/// What would make this red: `validate` run without the task's deadline, which waits out the
/// sleep past the test's own limit.
#[test]
fn a_hung_validate_ends_at_the_task_timeout() {
    let scratch = Scratch::new("timeout");
    let hang = scratch.hang();
    let mut agent = Agent::spawn(&scratch.0);
    agent.hello();
    let started = Instant::now();
    let (result, ran) = agent.copy(
        1,
        b"x\n",
        json!({"dest": scratch.0.join("dest.txt"), "validate": format!("{hang} %s")}),
        Some(2),
    );
    assert_eq!(ran.path, ExecPath::Native, "{:?}", ran.reason);
    assert_eq!(result, TaskResult::timed_out(2));
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the hung validate was waited for"
    );
}

/// The controller's cancel stops a `validate` that hangs.
///
/// What would make this red: the cancel not asked while `validate` runs.
#[test]
fn a_cancel_stops_a_hung_validate() {
    let scratch = Scratch::new("cancel");
    let hang = scratch.hang();
    let mut agent = Agent::spawn(&scratch.0);
    agent.hello();
    let hash = agent.put(b"x\n");
    let started = Instant::now();
    agent.send(&ToAgent::RunBatch {
        id: 7,
        tasks: vec![copy_task(
            json!({"dest": scratch.0.join("dest.txt"), "validate": format!("{hang} %s")}),
            &hash,
            None,
        )],
    });
    std::thread::sleep(Duration::from_millis(500));
    agent.send(&ToAgent::Cancel { id: 7 });
    let outcome = loop {
        match agent.recv() {
            FromAgent::BatchDone { outcome, .. } => break outcome,
            FromAgent::TaskResult { .. } | FromAgent::Log { .. } => {}
            other => panic!("expected the batch to end, got {other:?}"),
        }
    };
    assert_eq!(outcome, BatchOutcome::Cancelled { at: 0 });
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the hung validate was waited for"
    );
}

/// Root replacing a file another account owns leaves it that account's: `atomic_move` gives the
/// staged file the replaced file's owner and group before the rename. The agent runs under
/// `sudo -n`, as under `become`, over a file of the account running the test.
///
/// What would make this red: the `chown` to the replaced file's owner dropped, which leaves the
/// file root's, on disk and in the answer's `uid`.
#[test]
#[ignore = "needs passwordless `sudo -n`; runs under `just ssh-test`"]
fn ssh_root_replacing_a_file_keeps_its_owner() {
    use std::os::unix::fs::MetadataExt;
    assert!(
        Command::new("sudo")
            .args(["-n", "true"])
            .status()
            .is_ok_and(|status| status.success()),
        "this test needs passwordless `sudo -n`"
    );
    let scratch = Scratch::new("owner");
    let dest = scratch.0.join("dest.txt");
    std::fs::write(&dest, "old\n").unwrap();
    let mine = std::fs::metadata(&dest).unwrap();
    assert_ne!(
        mine.uid(),
        0,
        "run as an ordinary account, whose file root replaces"
    );
    let mut agent = Agent::spawn_as_root(&scratch.0);
    agent.hello();
    let (result, ran) = agent.copy(1, b"new\n", json!({"dest": dest}), None);
    drop(agent);
    let left = std::fs::metadata(&dest).unwrap();
    let content = std::fs::read_to_string(&dest).unwrap();
    // The agent's cache and staging directories are root's.
    let _ = Command::new("sudo")
        .args(["-n", "rm", "-rf"])
        .arg(&scratch.0)
        .status();
    assert_eq!(ran.path, ExecPath::Native, "{:?}", ran.reason);
    assert_eq!(content, "new\n");
    assert_eq!((left.uid(), left.gid()), (mine.uid(), mine.gid()));
    assert_eq!(result.0["uid"], mine.uid(), "{result:?}");
    assert_eq!(result.0["gid"], mine.gid(), "{result:?}");
}

/// A `copy` task as the action plugin sends it, with `src` staged from `hash`, and a payload the
/// agent does not hold: the Python path would show as that path's own failure.
fn copy_task(args: Value, hash: &str, timeout: Option<u64>) -> Task {
    let mut args = args.as_object().unwrap().clone();
    args.insert("_original_basename".into(), "file.txt".into());
    args.insert("follow".into(), false.into());
    Task {
        module: "copy".into(),
        args,
        ignore_errors: true,
        timeout,
        files: vec![StagedFile {
            arg: "src".into(),
            blob: hash.into(),
        }],
        payload: Some(PythonPayload {
            blob: "0".repeat(64),
            module_fqn: "ansible.modules.copy".into(),
            profile: "legacy".into(),
            rlimit_nofile: 0,
            extensions: Map::new(),
            interpreter: "/usr/bin/python3".into(),
        }),
        ..Task::default()
    }
}

struct Agent {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Agent {
    /// An agent whose `remote_tmp` is `dir`, so its staging is this test's own and on the same
    /// file system as the destination.
    fn spawn(dir: &Path) -> Agent {
        let mut command = Command::new(env!("CARGO_BIN_EXE_volant-agent"));
        command.env("VOLANT_REMOTE_TMP", dir);
        Agent::start(command)
    }

    /// `spawn`, the agent running as root through `sudo -n`, as under `become`.
    fn spawn_as_root(dir: &Path) -> Agent {
        let mut command = Command::new("sudo");
        command
            .args(["-n", "env"])
            .arg(format!("VOLANT_REMOTE_TMP={}", dir.display()))
            .arg(env!("CARGO_BIN_EXE_volant-agent"));
        Agent::start(command)
    }

    fn start(mut command: Command) -> Agent {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("agent starts");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Agent {
            child,
            stdin,
            stdout,
        }
    }

    /// Stages `bytes` and returns their name.
    fn put(&mut self, bytes: &[u8]) -> String {
        let hash = blake3::hash(bytes).to_hex().to_string();
        self.send(&ToAgent::PutBlob {
            hash: hash.clone(),
            len: bytes.len() as u64,
            encoding: BlobEncoding::Raw,
            staged: true,
        });
        write_frame(&mut self.stdin, bytes).unwrap();
        self.stdin.flush().unwrap();
        assert_eq!(
            self.recv(),
            FromAgent::BlobState {
                hash: hash.clone(),
                present: true
            }
        );
        hash
    }

    /// Stages `bytes` and runs a `copy` of them with `args`.
    fn copy(
        &mut self,
        id: u64,
        bytes: &[u8],
        args: Value,
        timeout: Option<u64>,
    ) -> (TaskResult, Ran) {
        let hash = self.put(bytes);
        self.send(&ToAgent::RunBatch {
            id,
            tasks: vec![copy_task(args, &hash, timeout)],
        });
        let answer = loop {
            match self.recv() {
                FromAgent::TaskResult { result, ran, .. } => {
                    break (result, ran.expect("the agent says how it ran the task"));
                }
                FromAgent::Log { .. } => {}
                other => panic!("expected a task result, got {other:?}"),
            }
        };
        assert!(matches!(self.recv(), FromAgent::BatchDone { batch, .. } if batch == id));
        answer
    }

    fn send(&mut self, msg: &ToAgent) {
        write_frame(&mut self.stdin, &serde_json::to_vec(msg).unwrap()).unwrap();
        self.stdin.flush().unwrap();
    }

    fn recv(&mut self) -> FromAgent {
        let bytes = read_frame(&mut self.stdout)
            .unwrap()
            .expect("the agent answers");
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Says hello and returns the natives the agent reports.
    fn hello(&mut self) -> Vec<String> {
        self.send(&ToAgent::Hello {
            protocol: PROTOCOL_VERSION,
        });
        match self.recv() {
            FromAgent::Ready { natives, .. } => natives,
            other => panic!("expected Ready, got {other:?}"),
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A directory of this test's own under `/var/tmp`, removed at the end.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = PathBuf::from(format!(
            "/var/tmp/volant-native-copy-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    /// A program that hangs, for `validate`.
    fn hang(&self) -> String {
        let hang = self.0.join("hang");
        std::fs::write(&hang, "#!/bin/sh\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&hang, std::fs::Permissions::from_mode(0o755)).unwrap();
        hang.to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
