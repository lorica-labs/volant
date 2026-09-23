// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(unix)]

use std::fs;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Map, Value, json};
use volant_protocol::encoding::{b64_decode, b64_encode};
use volant_protocol::frame::{read_frame, write_frame};
use volant_protocol::{FromAgent, LogLevel, PythonPayload, StagedFile, Task, TaskResult, ToAgent};

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
            .join(&hash);
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
                files: Vec::new(),
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

/// A staged file is handed to the module under the argument it names, and is gone from the
/// cache once the task has run.
///
/// What would make this red: the blob's own cache path handed to the module - `copy` moves
/// its source, so the cache entry would vanish under a controller that believes the link
/// still holds it; or the staged copy left behind, which keeps a rendered template, and the
/// secret in it, on the host after the run.
///
/// Three tasks in one batch: one that reads its file, one that stages the same blob again and
/// must find it consumed, and one that moves its file away as `copy` does, which the agent's
/// own removal has to take in its stride.
#[test]
fn a_staged_file_reaches_the_module_and_leaves_nothing_behind() {
    let scratch = Scratch::new("staged");
    let mut link = Link::open(&scratch.0);
    let union = link.put(&build_payload(
        "\n    import os\n    src = args[\"src\"]\n    content = open(src).read()\n    if args.get(\"move_to\"):\n        os.rename(src, args[\"move_to\"])\n    print(json.dumps({\"changed\": False, \"src\": src, \"content\": content}))\n",
    ));
    let read = link.put(b"the file the module reads\n");
    let moved = link.put(b"the file the module moves\n");
    let dest = scratch.0.join("dest");
    let mut moving = python_task(&union, &moved);
    moving.args.insert("move_to".into(), json!(dest));
    let results = link.run(vec![
        python_task(&union, &read),
        python_task(&union, &read),
        moving,
    ]);

    let cache = cache_dir(&scratch.0);
    assert_eq!(
        entries(&cache),
        vec![union.clone()],
        "nothing behind: the cache keeps the union and nothing a task staged"
    );
    let first = &results[0].0;
    assert_eq!(first["content"], "the file the module reads\n", "{first:?}");
    let src = first["src"].as_str().unwrap();
    assert!(
        Path::new(src).starts_with(&cache),
        "{src} is not under {}",
        cache.display()
    );
    assert!(
        results[1].failed(),
        "the second task ran on a file the first consumed: {:?}",
        results[1].0
    );
    assert_eq!(
        results[1].0["msg"],
        format!("staging file {read} for 'src': No such file or directory (os error 2)")
    );
    assert!(!results[2].failed(), "{:?}", results[2].0);
    assert_eq!(fs::read(&dest).unwrap(), b"the file the module moves\n");
}

/// A file that cannot be staged fails its task with the reason, and the module never starts:
/// the union named here is not on the host, so a module that did start would fail on that
/// instead.
///
/// What would make this red: a staging failure logged and the module run without its file, the
/// "accepted then ignored" shape; or a corrupted blob moved out of the cache, which leaves the
/// next `put_blob` nothing to replace and a staged file whose bytes nobody checked.
#[test]
fn a_file_that_cannot_be_staged_fails_the_task_before_the_module_runs() {
    let scratch = Scratch::new("unstaged");
    let mut link = Link::open(&scratch.0);
    let absent_union = "0".repeat(64);
    let corrupt = link.put(b"the bytes the controller sent");
    let planted = cache_dir(&scratch.0).join(&corrupt);
    fs::write(&planted, b"bytes somebody else wrote").unwrap();
    let missing = blake3::hash(b"never sent").to_hex().to_string();
    let results = link.run(vec![
        python_task(&absent_union, "../volant-agent-0.1.0/volant-agent"),
        python_task(&absent_union, &missing),
        python_task(&absent_union, &corrupt),
    ]);

    let msg = |i: usize| results[i].0["msg"].as_str().unwrap().to_string();
    assert!(
        msg(0).starts_with("staging file ../volant-agent-0.1.0/volant-agent for 'src': ")
            && msg(0).contains("is not a payload name"),
        "{}",
        msg(0)
    );
    assert_eq!(
        msg(1),
        format!("staging file {missing} for 'src': No such file or directory (os error 2)")
    );
    let actual = blake3::hash(b"bytes somebody else wrote")
        .to_hex()
        .to_string();
    assert!(
        msg(2).starts_with(&format!("staging file {corrupt} for 'src': "))
            && msg(2).contains(&actual),
        "{}",
        msg(2)
    );
    assert_eq!(
        fs::read(&planted).unwrap(),
        b"bytes somebody else wrote",
        "the corrupted blob stays for the next put_blob to replace"
    );
    assert!(results.iter().all(TaskResult::failed));
}

/// The staged copy is removed when the module fails, here because the union it names is not on
/// the host. Cancellation takes the same path: the removal follows the module whatever `Run` it
/// returned.
///
/// What would make this red: the removal done only after a module that succeeded, which leaves
/// a failed `template` task's rendering on the host.
#[test]
fn a_staged_file_is_removed_when_the_module_fails() {
    let scratch = Scratch::new("failed");
    let mut link = Link::open(&scratch.0);
    let file = link.put(b"a rendered secret");
    let results = link.run(vec![python_task(&"0".repeat(64), &file)]);
    assert_eq!(
        results[0].0["msg"],
        format!("payload {} is not on this host", "0".repeat(64)),
        "the file was staged and the module path reached"
    );
    let left: Vec<String> = entries(&cache_dir(&scratch.0))
        .into_iter()
        .filter(|name| name.starts_with("stage-"))
        .collect();
    assert_eq!(
        left,
        Vec::<String>::new(),
        "a staged copy outlived its task"
    );
}

/// A staged copy the agent cannot remove fails its task, naming the path, even though the
/// module itself succeeded. The module replaces its file with a directory, which `remove_file`
/// refuses with something other than `NotFound`.
///
/// What would make this red: the removal error dropped, which reports green for a task whose
/// rendered secret is still on the host.
#[test]
fn a_staged_file_that_cannot_be_removed_fails_the_task() {
    let scratch = Scratch::new("unremovable");
    let mut link = Link::open(&scratch.0);
    let union = link.put(&build_payload(
        "\n    import os\n    src = args[\"src\"]\n    os.remove(src)\n    os.mkdir(src)\n    open(src + \"/x\", \"w\").close()\n    print(json.dumps({\"changed\": False}))\n",
    ));
    let file = link.put(b"a rendered secret");
    let results = link.run(vec![python_task(&union, &file)]);
    let msg = results[0]
        .0
        .get("msg")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        results[0].failed() && msg.starts_with("removing staged file "),
        "{:?}",
        results[0].0
    );
    assert!(msg.contains(&format!("stage-{file}-")), "{msg}");
}

/// A native module has nowhere to put a staged file, so a task asking for one fails by name and
/// the blob stays in the cache.
///
/// What would make this red: the files dropped and `command` run without them, which reports
/// green for a task that never saw its source.
#[test]
fn a_native_module_cannot_take_staged_files() {
    let scratch = Scratch::new("native");
    let mut link = Link::open(&scratch.0);
    let file = link.put(b"a file");
    let results = link.run(vec![Task {
        module: "command".into(),
        args: json!({"_raw_params": "true"}).as_object().unwrap().clone(),
        ignore_errors: true,
        timeout: None,
        environment: std::collections::BTreeMap::new(),
        payload: None,
        files: vec![StagedFile {
            arg: "src".into(),
            blob: file.clone(),
        }],
    }]);
    assert_eq!(
        results[0].0["msg"],
        "the module command cannot take staged files"
    );
    assert_eq!(entries(&cache_dir(&scratch.0)), vec![file]);
}

/// The interpreter the staged-file tests run their module under.
const PYTHON: &str = "/usr/bin/python3";

/// A Python task under `union` whose `src` is the blob `file`, failures ignored so every task
/// of a batch reports.
fn python_task(union: &str, file: &str) -> Task {
    Task {
        module: "probe".into(),
        args: Map::new(),
        ignore_errors: true,
        timeout: None,
        environment: std::collections::BTreeMap::new(),
        payload: Some(PythonPayload {
            blob: union.into(),
            module_fqn: "ansible.modules.probe".into(),
            profile: "legacy".into(),
            rlimit_nofile: 0,
            extensions: Map::new(),
            interpreter: PYTHON.into(),
        }),
        files: vec![StagedFile {
            arg: "src".into(),
            blob: file.into(),
        }],
    }
}

/// A stub union whose module is `body`, built as a real zip by the host interpreter, as the
/// agent's own python tests build theirs.
fn build_payload(body: &str) -> Vec<u8> {
    const BUILD: &str = r##"
import base64, io, sys, zipfile

loader = 'import json, sys\n\n\ndef run_module(json_params, profile, module_fqn, modlib_path, extensions):\n    args = json.loads(json_params)["ANSIBLE_MODULE_ARGS"]' + sys.argv[1]
buf = io.BytesIO()
archive = zipfile.ZipFile(buf, "w")
for name in [
    "ansible/__init__.py",
    "ansible/module_utils/__init__.py",
    "ansible/module_utils/_internal/__init__.py",
    "ansible/module_utils/_internal/_ansiballz/__init__.py",
]:
    archive.writestr(name, "")
archive.writestr("ansible/module_utils/basic.py", "# stub\n")
archive.writestr("ansible/module_utils/_internal/_ansiballz/_loader.py", loader)
archive.close()
sys.stdout.write(base64.b64encode(buf.getvalue()).decode())
"##;
    assert!(
        Path::new(PYTHON).exists(),
        "this test runs a real module under {PYTHON}, which this host does not have"
    );
    let built = Command::new(PYTHON)
        .args(["-c", BUILD, body])
        .output()
        .expect("the host python builds the payload");
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    b64_decode(&String::from_utf8(built.stdout).unwrap()).unwrap()
}

/// The cache directory an agent run as this user keeps under `remote_tmp`.
fn cache_dir(remote_tmp: &Path) -> PathBuf {
    // SAFETY: `geteuid` reads the calling process's own credentials and cannot fail.
    let euid = unsafe { libc::geteuid() };
    remote_tmp.join(format!("volant-blobs-{}-{euid}", env!("CARGO_PKG_VERSION")))
}

/// The names in `dir`, sorted; none when it does not exist.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|list| {
            list.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// A directory of this test's own, removed when it ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("volant-agent-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// An agent whose cache lives under `remote_tmp`, and both ends of the link to it.
struct Link {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Link {
    fn open(remote_tmp: &Path) -> Link {
        let mut child = Command::new(env!("CARGO_BIN_EXE_volant-agent"))
            .env("VOLANT_REMOTE_TMP", remote_tmp)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("agent starts");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Link {
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

    /// Sends `bytes` as a blob, waits for it to land, and returns its name.
    fn put(&mut self, bytes: &[u8]) -> String {
        let hash = blake3::hash(bytes).to_hex().to_string();
        self.send(&ToAgent::PutBlob {
            hash: hash.clone(),
            zip_b64: b64_encode(bytes),
        });
        assert_eq!(
            self.recv(),
            FromAgent::BlobState {
                hash: hash.clone(),
                present: true
            }
        );
        hash
    }

    /// Runs one batch and returns every task's result, in order.
    fn run(&mut self, tasks: Vec<Task>) -> Vec<TaskResult> {
        let count = tasks.len();
        self.send(&ToAgent::RunBatch { id: 1, tasks });
        let mut results = Vec::new();
        loop {
            match self.recv() {
                FromAgent::TaskResult { result, .. } => results.push(result),
                FromAgent::BatchDone { .. } => break,
                FromAgent::Log { .. } => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(results.len(), count, "{results:?}");
        results
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
