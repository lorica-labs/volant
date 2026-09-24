// SPDX-License-Identifier: GPL-3.0-or-later
//! The dispatcher between a native module and its Python payload, through the real agent binary.
//!
//! These run against `volant_echo`, a native built only with the `test-natives` feature, which
//! this package's own dev-dependency on itself turns on for its tests. Every payload here names
//! an interpreter that does not exist and a blob the agent does not hold, so reaching the Python
//! path shows as that path's own failure rather than as a module's answer.
#![cfg(unix)]

use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Map, Value, json};
use volant_protocol::encoding::b64_encode;
use volant_protocol::frame::{read_frame, write_frame};
use volant_protocol::{
    ExecPath, FromAgent, PROTOCOL_VERSION, PythonPayload, Ran, StagedFile, Task, TaskResult,
    ToAgent,
};

/// Through the real agent binary: a task naming a module with an enabled native runs it and
/// says so; a native that hands the task back lets the payload run; `force_python` skips the
/// native; a disabled native, or a module of the same name from anywhere but ansible-core's own
/// modules, never reaches one.
///
/// What would make this red: `force_python` ignored (`Native` where `Python` is asked for); a
/// hand-back taken as the answer (a result with no Python failure in it); a stub native listed
/// in `Ready.natives` or consulted at all; or the native looked up by the name the playbook
/// wrote, which answers for another collection's module.
#[test]
fn a_native_runs_hands_back_or_is_skipped_as_the_task_asks() {
    let scratch = Scratch::new("dispatch");
    let mut agent = Agent::spawn(&scratch.0);
    assert_eq!(agent.hello(), vec!["volant_echo".to_string()]);

    let (result, ran) = agent.run_one(1, task("ansible.modules.volant_echo", json!({"x": 1})));
    assert_eq!(ran.path, ExecPath::Native, "{ran:?}");
    assert_eq!(ran.reason, None);
    assert_eq!(ran.fork_micros, None, "a native forks nothing");
    assert_eq!(
        Value::Object(result.0),
        json!({"changed": false, "echo": {"x": 1}})
    );

    let (result, ran) = agent.run_one(
        2,
        task(
            "ansible.modules.volant_echo",
            json!({"fallback": "outside the subset"}),
        ),
    );
    assert_eq!(ran.path, ExecPath::Fallback, "{ran:?}");
    assert_eq!(ran.reason.as_deref(), Some("outside the subset"));
    assert_python_ran(&result);

    let mut forced = task("ansible.modules.volant_echo", json!({"x": 1}));
    forced.force_python = true;
    let (result, ran) = agent.run_one(3, forced);
    assert_eq!(ran.path, ExecPath::Python, "{ran:?}");
    assert_python_ran(&result);

    let (result, ran) = agent.run_one(4, task("ansible.modules.stat", json!({"path": "/"})));
    assert_eq!(
        ran.path,
        ExecPath::Python,
        "a disabled native is never consulted"
    );
    assert_python_ran(&result);

    let (result, ran) = agent.run_one(
        5,
        task(
            "ansible_collections.acme.tools.plugins.modules.volant_echo",
            json!({"x": 1}),
        ),
    );
    assert_eq!(ran.path, ExecPath::Python, "another collection's module");
    assert_python_ran(&result);
}

/// The native sees the task's staged file where the Python module would, under the argument it
/// was staged for, and the file is gone once the task is over.
///
/// What would make this red: the native run before the files are staged or after they are
/// removed, which leaves `src` unset or pointing at nothing.
#[test]
fn a_native_reads_the_files_staged_for_its_task() {
    let scratch = Scratch::new("staged");
    let mut agent = Agent::spawn(&scratch.0);
    agent.hello();
    let bytes = b"what copy would copy";
    let hash = blake3::hash(bytes).to_hex().to_string();
    agent.send(&ToAgent::PutBlob {
        hash: hash.clone(),
        zip_b64: b64_encode(bytes),
        staged: true,
    });
    assert_eq!(
        agent.recv(),
        FromAgent::BlobState {
            hash: hash.clone(),
            present: true
        }
    );
    let mut staged = task("ansible.modules.volant_echo", json!({"read_src": true}));
    staged.files = vec![StagedFile {
        arg: "src".into(),
        blob: hash,
    }];
    let (result, ran) = agent.run_one(1, staged);
    assert_eq!(ran.path, ExecPath::Native, "{ran:?}");
    assert_eq!(
        result.0["src_content"], "what copy would copy",
        "{result:?}"
    );
    let src = PathBuf::from(result.0["echo"]["src"].as_str().expect("src is set"));
    assert!(!src.exists(), "the staged file outlived its task");
}

/// A native that panics hands the task back instead of taking the agent down: the payload runs,
/// the reason says what happened, and the agent answers the next batch.
///
/// What would make this red: the call to the native no longer guarded, which ends the agent
/// and leaves this test reading a closed pipe.
#[test]
fn a_native_that_panics_hands_the_task_back() {
    let scratch = Scratch::new("panic");
    let mut agent = Agent::spawn(&scratch.0);
    agent.hello();
    let (result, ran) = agent.run_one(1, task("ansible.modules.volant_echo", json!({"panic": 1})));
    assert_eq!(ran.path, ExecPath::Fallback, "{ran:?}");
    assert_eq!(ran.reason.as_deref(), Some("native module panicked"));
    assert_python_ran(&result);
    let (_, ran) = agent.run_one(2, task("ansible.modules.volant_echo", json!({})));
    assert_eq!(ran.path, ExecPath::Native);
}

/// The Python path's own answer for a payload that is not on the host.
fn assert_python_ran(result: &TaskResult) {
    assert!(result.failed(), "{result:?}");
    let msg = result.0["msg"].as_str().unwrap_or_default();
    assert!(
        msg.starts_with(&format!("payload {}", "0".repeat(64))),
        "not the Python path's answer: {msg}"
    );
}

fn task(module_fqn: &str, args: Value) -> Task {
    Task {
        module: module_fqn.rsplit('.').next().unwrap().into(),
        args: args.as_object().unwrap().clone(),
        ignore_errors: true,
        payload: Some(PythonPayload {
            blob: "0".repeat(64),
            module_fqn: module_fqn.into(),
            profile: "legacy".into(),
            rlimit_nofile: 0,
            extensions: Map::new(),
            interpreter: "/nonexistent/python3".into(),
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
    /// An agent whose `remote_tmp` is `dir`, so its cache and staging are this test's own.
    fn spawn(dir: &Path) -> Agent {
        let mut child = Command::new(env!("CARGO_BIN_EXE_volant-agent"))
            .env("VOLANT_REMOTE_TMP", dir)
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

    /// Runs a batch of one task and returns its result and what the agent says about the run.
    fn run_one(&mut self, id: u64, task: Task) -> (TaskResult, Ran) {
        self.send(&ToAgent::RunBatch {
            id,
            tasks: vec![task],
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
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A directory of this test's own under the system's temporary directory, removed at the end.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "volant-agent-natives-{name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
