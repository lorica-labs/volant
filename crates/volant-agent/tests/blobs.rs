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
            staged: false,
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
            staged: false,
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
            staged: false,
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
            staged: false,
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
    let read = link.stage(b"the file the module reads\n");
    let moved = link.stage(b"the file the module moves\n");
    let dest = scratch.0.join("dest");
    let mut moving = python_task(&union, &moved);
    moving.args.insert("move_to".into(), json!(dest));
    let results = link.run(vec![
        python_task(&union, &read),
        python_task(&union, &read),
        moving,
    ]);

    let cache = cache_dir(&scratch.0);
    let stage = link.stage_dir(&scratch.0);
    let stage_name = stage.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(
        entries(&cache),
        vec![union.clone(), stage_name],
        "the cache keeps the union and the connection's own directory"
    );
    assert_eq!(
        entries(&stage),
        Vec::<String>::new(),
        "nothing behind: every staged file went with its task"
    );
    let first = &results[0].0;
    assert_eq!(first["content"], "the file the module reads\n", "{first:?}");
    let src = first["src"].as_str().unwrap();
    assert!(
        Path::new(src).starts_with(&stage),
        "{src} is not under {}",
        stage.display()
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
    let corrupt = link.stage(b"the bytes the controller sent");
    let planted = link.stage_dir(&scratch.0).join(&corrupt);
    fs::write(&planted, b"bytes somebody else wrote").unwrap();
    let missing = blake3::hash(b"never sent").to_hex().to_string();
    // In the shared cache, where a payload goes: a file is only ever taken from the
    // connection's own directory, so this one is as good as never sent.
    let shared = link.put(b"sent as a payload");
    let results = link.run(vec![
        python_task(&absent_union, "../volant-agent-0.1.0/volant-agent"),
        python_task(&absent_union, &missing),
        python_task(&absent_union, &corrupt),
        python_task(&absent_union, &shared),
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
    assert_eq!(
        msg(3),
        format!("staging file {shared} for 'src': No such file or directory (os error 2)")
    );
    assert!(
        cache_dir(&scratch.0).join(&shared).exists(),
        "the payload stays in the shared cache"
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
    let file = link.stage(b"a rendered secret");
    let results = link.run(vec![python_task(&"0".repeat(64), &file)]);
    assert_eq!(
        results[0].0["msg"],
        format!("payload {} is not on this host", "0".repeat(64)),
        "the file was staged and the module path reached"
    );
    assert_eq!(
        entries(&link.stage_dir(&scratch.0)),
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
    let file = link.stage(b"a rendered secret");
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
    let staged = link.stage_dir(&scratch.0).join(format!("{file}-"));
    assert!(msg.contains(&*staged.to_string_lossy()), "{msg}");
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
    let file = link.stage(b"a file");
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
    assert_eq!(entries(&link.stage_dir(&scratch.0)), vec![file]);
}

/// Two agents of one user on one cache - two inventory names for one machine, or two hosts
/// delegating to one - are each sent the same file, and each module reads it, in the order that
/// broke: both puts land before either task runs.
///
/// What would make this red: a staged file written to the shared cache. The second put finds
/// the first one's file there and answers for it, the first task consumes it, and the second
/// task fails `staging file <hash> for 'src': No such file or directory`.
#[test]
fn two_agents_of_one_user_each_stage_their_own_copy_of_one_file() {
    let scratch = Scratch::new("two-links");
    let mut first = Link::open(&scratch.0);
    let union = first.put(&build_payload(
        "\n    print(json.dumps({\"changed\": False, \"content\": open(args[\"src\"]).read()}))\n",
    ));
    let file = first.stage(b"one file, two links\n");
    // Started after the first one staged, so its sweep meets a live agent's directory.
    let mut second = Link::open(&scratch.0);
    assert_eq!(second.stage(b"one file, two links\n"), file);
    for link in [&mut first, &mut second] {
        let results = link.run(vec![python_task(&union, &file)]);
        assert_eq!(
            results[0].0.get("content").and_then(Value::as_str),
            Some("one file, two links\n"),
            "{:?}",
            results[0].0
        );
    }
}

/// A file put and never taken - a batch cancelled before its task, a link dropped between the
/// put and the batch - goes with the connection, and so does the connection's directory.
///
/// What would make this red: nothing removing the directory when the controller goes away,
/// which leaves a rendered template, and the secret in it, on the host with nothing that will
/// ever look for it.
#[test]
fn a_file_put_and_never_taken_is_gone_when_the_connection_ends() {
    let scratch = Scratch::new("untaken");
    let mut link = Link::open(&scratch.0);
    let file = link.stage(b"a rendered secret nobody took");
    let stage = link.stage_dir(&scratch.0);
    assert_eq!(entries(&stage), vec![file.clone()], "the file was put");
    assert!(
        !cache_dir(&scratch.0).join(&file).exists(),
        "a staged file never enters the shared cache"
    );
    link.close();
    assert!(
        !stage.exists(),
        "{} outlived its connection",
        stage.display()
    );
    assert_eq!(entries(&cache_dir(&scratch.0)), Vec::<String>::new());
}

/// `has_blob` answers for the shared cache only, never for a file this or any connection staged.
///
/// What would make this red: `holds` looking in the connection's directory too. The controller
/// would then skip the put for a file one task already consumed, or that another link staged.
#[test]
fn has_blob_never_answers_for_a_staged_file() {
    let scratch = Scratch::new("has-staged");
    let mut link = Link::open(&scratch.0);
    let file = link.stage(b"a file");
    link.send(&ToAgent::HasBlob { hash: file.clone() });
    assert_eq!(
        link.recv(),
        FromAgent::BlobState {
            hash: file,
            present: false
        }
    );
}

/// An agent starting up removes what a dead agent of this user left on this host, and leaves
/// alone a live one's directory, another host's, and the earlier layout that names no host.
///
/// What would make this red: the sweep removing a directory without asking whether its pid is
/// alive - two runs reaching one host run two agents of one user at once, and the second to
/// start would pull the file out from under the first one's module; or the sweep ignoring the
/// host in the name. A home directory mounted over NFS shares the cache between hosts, and a pid
/// that is dead here says nothing about the host that wrote it. The live pid is the test's own.
#[test]
fn the_next_agent_sweeps_what_a_dead_agent_staged_and_keeps_a_live_ones() {
    use std::os::unix::fs::DirBuilderExt;

    let scratch = Scratch::new("sweep");
    let cache = cache_dir(&scratch.0);
    fs::DirBuilder::new().mode(0o700).create(&cache).unwrap();
    let dead = dead_pid();
    let live = std::process::id();
    let host = this_host();
    let mine_dead = format!("stage-{host}-{dead}");
    let mine_live = format!("stage-{host}-{live}");
    let elsewhere = format!("stage-{}-{dead}", other_host(&host));
    for name in [&mine_dead, &mine_live, &elsewhere] {
        let dir = cache.join(name);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("0".repeat(64)), b"a staged secret").unwrap();
    }
    let old = format!("stage-{}-{dead}-0", "0".repeat(64));
    fs::write(cache.join(&old), b"a staged secret").unwrap();

    started(&scratch.0);

    let mut kept = vec![mine_live.clone(), elsewhere, old];
    kept.sort();
    assert_eq!(
        entries(&cache),
        kept,
        "only this host's dead agent's directory goes"
    );
    assert_eq!(entries(&cache.join(&mine_live)), vec!["0".repeat(64)]);
}

/// A cache this agent does not own outright is not swept at all, even of an entry that would be
/// swept in a private one.
///
/// What would make this red: the sweep acting before it checks the cache's owner and mode, which
/// lets anyone who can write the directory name what this agent deletes.
#[test]
fn a_cache_others_can_write_is_not_swept() {
    use std::os::unix::fs::DirBuilderExt;

    let scratch = Scratch::new("sweep-open");
    let cache = cache_dir(&scratch.0);
    fs::DirBuilder::new().mode(0o755).create(&cache).unwrap();
    let stale = format!("stage-{}-{}", this_host(), dead_pid());
    fs::create_dir(cache.join(&stale)).unwrap();

    started(&scratch.0);

    assert_eq!(entries(&cache), vec![stale]);
}

/// A staged file the agent refuses is named a file in the log, not a payload.
#[test]
fn a_refused_staged_file_is_logged_as_a_file() {
    let scratch = Scratch::new("refused-file");
    let mut link = Link::open(&scratch.0);
    let wrong = "0".repeat(64);
    link.send(&ToAgent::PutBlob {
        hash: wrong.clone(),
        zip_b64: b64_encode(b"a file"),
        staged: true,
    });
    match link.recv() {
        FromAgent::Log { message, .. } => {
            assert!(
                message.starts_with(&format!("storing file {wrong}: ")),
                "{message}"
            );
        }
        other => panic!("expected the refusal's log, got {other:?}"),
    }
}

/// Starts an agent on `remote_tmp` and waits until it has answered: the sweep runs before the
/// agent reads its first frame, so an answer means it is over.
fn started(remote_tmp: &Path) {
    let mut link = Link::open(remote_tmp);
    link.send(&ToAgent::Hello {
        protocol: volant_protocol::PROTOCOL_VERSION,
    });
    assert!(matches!(link.recv(), FromAgent::Ready { .. }));
}

/// The pid of a process that has exited and been reaped.
fn dead_pid() -> u32 {
    let mut gone = Command::new("true").spawn().expect("true starts");
    let pid = gone.id();
    gone.wait().unwrap();
    pid
}

/// This host's name as the agent puts it in a directory name: `[A-Za-z0-9._]` kept, anything else
/// `_`, at most 64 bytes. Written out again rather than shared, so a change on the agent's side
/// has to be made here too.
fn this_host() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is writable for its whole length, which is the length passed.
    assert_eq!(
        unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) },
        0
    );
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    buf[..len]
        .iter()
        .take(64)
        .map(|&b| {
            if b.is_ascii_alphanumeric() || b == b'.' || b == b'_' {
                char::from(b)
            } else {
                '_'
            }
        })
        .collect()
}

/// A host name that is not `host`.
fn other_host(host: &str) -> String {
    format!("{host}.elsewhere")
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
    /// `None` once [`Link::close`] has ended the conversation.
    stdin: Option<ChildStdin>,
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
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Link {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, msg: &ToAgent) {
        let stdin = self.stdin.as_mut().expect("the link is open");
        write_frame(&mut *stdin, &serde_json::to_vec(msg).unwrap()).unwrap();
        stdin.flush().unwrap();
    }

    fn recv(&mut self) -> FromAgent {
        let bytes = read_frame(&mut self.stdout)
            .unwrap()
            .expect("the agent answers");
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Sends `bytes` as a payload for the shared cache, waits for it to land, and returns its
    /// name.
    fn put(&mut self, bytes: &[u8]) -> String {
        self.put_as(bytes, false)
    }

    /// Sends `bytes` as a file one task stages, as the controller sends every file.
    fn stage(&mut self, bytes: &[u8]) -> String {
        self.put_as(bytes, true)
    }

    fn put_as(&mut self, bytes: &[u8], staged: bool) -> String {
        let hash = blake3::hash(bytes).to_hex().to_string();
        self.send(&ToAgent::PutBlob {
            hash: hash.clone(),
            zip_b64: b64_encode(bytes),
            staged,
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

    /// Where this link's agent keeps the files it was sent to stage.
    fn stage_dir(&self, remote_tmp: &Path) -> PathBuf {
        cache_dir(remote_tmp).join(format!("stage-{}-{}", this_host(), self.child.id()))
    }

    /// Ends the conversation as a controller that went away does, and waits for the agent.
    fn close(&mut self) {
        self.stdin = None;
        assert!(
            self.child.wait().unwrap().success(),
            "the agent ends at end of stream"
        );
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
