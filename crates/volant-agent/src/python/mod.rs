// SPDX-License-Identifier: GPL-3.0-or-later
//! One warm Python server per interpreter and payload, forking a child per module.
//!
//! The server is where this release's Python support earns its name. Measured on the reference's
//! `ping`: a module started from cold takes 340 ms, forking alone takes it to 266 ms, and a
//! parent that has already imported `module_utils` from the payload takes it to 12.8 ms. The
//! import is paid once per run, by a parent that never imports a module and never runs module
//! code, so nothing one module did reaches the next.
//!
//! The parent and this side speak the agent's own frames - four bytes of big-endian length, then
//! JSON - so there is one framing in the whole agent rather than two.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::io::{BufReader, Read};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use volant_protocol::frame::{read_frame, write_frame};
use volant_protocol::{PythonPayload, TaskResult};

use crate::modules::command::{CANCEL_POLL, kill_group};
use crate::modules::{Context, Run};

/// The parent, handed to the interpreter with `-c`. Embedded rather than written to the host:
/// the agent already refuses to leave anything executable behind, and a file on a `noexec`
/// `remote_tmp` could not be run anyway.
const SERVER: &str = include_str!("server.py");

/// How long a server gets to import `module_utils` and answer `ready`. Measured at 264 ms on the
/// reference's own payload; the margin is for a host under load, and the bound exists so an
/// interpreter that starts and then says nothing fails the task instead of hanging the batch.
const START_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the server gets to hand back a result once its child has been killed. The child is
/// gone by then, so this only covers the parent reaping it and writing one frame.
const REAP_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the agent waits for a dead server's stderr to reach its end before it reports what
/// has arrived. Long enough for a pipe that is already closed to be drained, short enough that a
/// grandchild holding it open costs the task nothing it can feel.
const LAST_WORDS_WAIT: Duration = Duration::from_millis(500);

/// What an operator is shown of a module's own output when it is not a result.
const EXCERPT: usize = 2048;

#[derive(Debug)]
pub struct Server {
    child: Child,
    stdin: ChildStdin,
    /// Frames from the parent, read by a thread so a task can be cancelled while one is awaited.
    frames: Receiver<io::Result<Vec<u8>>>,
    /// Everything the server has written to stderr so far, filled as it arrives rather than at
    /// the end: a grandchild the server left behind holds that pipe open for as long as it
    /// lives, and waiting for the end of it is waiting on a process this agent never started.
    stderr: Arc<Mutex<String>>,
    /// Sent once the server's stderr reaches its end, so the common case reads a complete
    /// message rather than racing the reader thread for the last few bytes.
    stderr_ended: Receiver<()>,
    interpreter: String,
}

impl Server {
    /// Starts one server under `interpreter`, preloading module_utils from `blob`.
    ///
    /// An interpreter that cannot start is an error carrying its own words: it is the one party
    /// that knows why, and an operator reading "the module failed" would go looking at the
    /// module.
    ///
    /// The wait for the ready frame asks `cancelled` every [`CANCEL_POLL`], as the run loop
    /// does: importing `module_utils` is the slow part of a start, and a cancel that arrives
    /// during it is answered then rather than at [`START_TIMEOUT`]. A cancelled start is an
    /// [`io::ErrorKind::Interrupted`] error, and the server is killed with it.
    pub fn start(
        interpreter: &str,
        blob: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<Server> {
        let mut child = Command::new(interpreter)
            .arg("-c")
            .arg(SERVER)
            .arg(blob)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| io::Error::new(err.kind(), format!("starting {interpreter}: {err}")))?;
        let stdin = child.stdin.take().expect("stdin was piped");
        let mut pipe = child.stderr.take().expect("stderr was piped");
        let stderr = Arc::new(Mutex::new(String::new()));
        let (ended, stderr_ended) = mpsc::channel();
        let sink = Arc::clone(&stderr);
        thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if let Ok(mut text) = sink.lock() {
                            text.push_str(&String::from_utf8_lossy(&buffer[..read]));
                        }
                    }
                }
            }
            let _ = ended.send(());
        });
        let mut out = BufReader::new(child.stdout.take().expect("stdout was piped"));
        let (tx, frames) = mpsc::channel();
        thread::spawn(move || {
            loop {
                match read_frame(&mut out) {
                    Ok(Some(bytes)) => {
                        if tx.send(Ok(bytes)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        let _ = tx.send(Err(err));
                        break;
                    }
                }
            }
        });
        let mut server = Server {
            child,
            stdin,
            frames,
            stderr,
            stderr_ended,
            interpreter: interpreter.to_string(),
        };
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            match server.frame(deadline.min(Instant::now() + CANCEL_POLL)) {
                Ok(bytes) if ready(&bytes) => return Ok(server),
                Err(err) if err.kind() == io::ErrorKind::TimedOut && Instant::now() < deadline => {
                    if cancelled() {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "cancelled while the python server was starting",
                        ));
                    }
                }
                _ => return Err(server.died()),
            }
        }
    }

    /// Runs one module in a fresh fork. `args` is the task's `args` map; the server wraps it as
    /// `{"ANSIBLE_MODULE_ARGS": args}` exactly as the reference does.
    pub fn run(
        &mut self,
        payload: &PythonPayload,
        args: &Map<String, Value>,
        context: &Context,
        cancelled: &dyn Fn() -> bool,
    ) -> Run {
        match self.attempt(payload, args, context, cancelled) {
            Ok(run) => run,
            // The server is the one thing a task cannot work around, so its death is the task's
            // failure rather than a silent retry; the table above restarts it for the next one.
            // Its own last words go with it: a fork refused against `RLIMIT_NPROC`, a
            // `MemoryError`, any uncaught exception in the parent, all leave a traceback on the
            // pipe that is otherwise read and thrown away.
            Err(err) => {
                let said = self.last_words();
                Run::Done(fail(
                    format!(
                        "the python server under {} stopped: {err}",
                        self.interpreter
                    ),
                    said,
                ))
            }
        }
    }

    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn attempt(
        &mut self,
        payload: &PythonPayload,
        args: &Map<String, Value>,
        context: &Context,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<Run> {
        let request = json!({
            "args": args,
            "profile": payload.profile,
            "module_fqn": payload.module_fqn,
            "extensions": payload.extensions,
            "environment": context.environment,
            "rlimit_nofile": payload.rlimit_nofile,
        });
        write_frame(&mut self.stdin, &serde_json::to_vec(&request)?)?;

        // The fork happens as soon as the request is read, so this frame is immediate. It is
        // waited for even when the task is already cancelled: without the pid there is nothing
        // to kill, and the result frame behind it would desynchronise the next task.
        let pid = started(&self.frame(Instant::now() + REAP_TIMEOUT)?)?;

        let deadline = context.timeout.map(|t| Instant::now() + t);
        loop {
            if let Some(end) = deadline
                && Instant::now() >= end
            {
                kill_group(pid);
                self.reap();
                let seconds = context.timeout.unwrap_or_default().as_secs();
                return Ok(Run::Done(TaskResult::timed_out(seconds)));
            }
            match self.frames.recv_timeout(CANCEL_POLL) {
                Ok(frame) => {
                    return Ok(Run::Done(result(&serde_json::from_slice(&frame?)?)));
                }
                Err(RecvTimeoutError::Timeout) => {
                    if cancelled() {
                        kill_group(pid);
                        self.reap();
                        return Ok(Run::Cancelled);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "the python server ended while a module was running",
                    ));
                }
            }
        }
    }

    /// Takes the result frame of a child that has just been killed, and gives up on the server
    /// when it does not come.
    ///
    /// Never an error the caller sees: the answer at this point is the operator's - timed out, or
    /// cancelled - and turning a slow reap into "the server stopped" would report a deliberate
    /// cancellation as an engine failure and a five second timeout ten seconds late. A server
    /// that cannot finish draining, because a grandchild that called `setsid` still holds the
    /// child's stdout, is killed instead: that is what keeps its orphan frame out of the next
    /// task.
    fn reap(&mut self) {
        if self.frame(Instant::now() + REAP_TIMEOUT).is_err() {
            let _ = self.child.kill();
        }
    }

    /// The next frame, or an error once `deadline` is past. A server that stops answering is an
    /// error and not a wait without end: the batch behind it would never finish.
    fn frame(&mut self, deadline: Instant) -> io::Result<Vec<u8>> {
        let left = deadline.saturating_duration_since(Instant::now());
        match self.frames.recv_timeout(left) {
            Ok(frame) => frame,
            Err(RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the python server did not answer",
            )),
            Err(RecvTimeoutError::Disconnected) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the python server ended",
            )),
        }
    }

    /// Everything the server wrote to its own stderr, once it has ended.
    ///
    /// The child is killed first, so the pipe closes and what it wrote can be read whole.
    ///
    /// The wait for that end is bounded, and taking what has arrived is the answer when it
    /// expires: anything the server started - a module's daemon, a `Popen` nobody waited on -
    /// inherits that pipe and holds it open for as long as it lives, and a task already failing
    /// must not also wait on a process this agent never started. Measured on a fake server whose
    /// child outlives it by two minutes: the whole of that, before this bound existed.
    fn last_words(&mut self) -> Option<String> {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = self.stderr_ended.recv_timeout(LAST_WORDS_WAIT);
        let said = self
            .stderr
            .lock()
            .map(|text| excerpt(text.trim()))
            .unwrap_or_default();
        if said.is_empty() { None } else { Some(said) }
    }

    /// The error for a server that never got going, carrying what the interpreter said about it.
    fn died(&mut self) -> io::Error {
        let said = self
            .last_words()
            .unwrap_or_else(|| "it said nothing".to_string());
        io::Error::other(format!(
            "{} could not start a python server: {said}",
            self.interpreter
        ))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs one task that carries a payload, starting a server for it if this agent has none yet.
///
/// A blob is trusted because its bytes hash to its name, and the task that uses one is where that
/// has to hold. `Servers::verify` decides when the hash has to be read again.
pub fn run(
    payload: &PythonPayload,
    args: &Map<String, Value>,
    context: &Context,
    cancelled: &dyn Fn() -> bool,
) -> Run {
    let remote_tmp = crate::blobs::remote_tmp();
    let blob = match crate::blobs::path(&remote_tmp, &payload.blob) {
        Ok(blob) => blob,
        Err(err) => return Run::Done(fail(format!("payload {}: {err}", payload.blob), None)),
    };
    SERVERS.with(|table| {
        table
            .borrow_mut()
            .run(payload, &remote_tmp, &blob, args, context, cancelled)
    })
}

/// What `Servers::verify` compares to decide that a blob is still the file it hashed: device,
/// inode, length and modification time. `store` renames a new file over an old one, which
/// changes the inode whatever the length and the time say.
type Fingerprint = (u64, u64, u64, std::time::SystemTime);

fn fingerprint(at: &Path) -> Option<Fingerprint> {
    let meta = std::fs::metadata(at).ok()?;
    #[cfg(unix)]
    let (dev, ino) = {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    };
    #[cfg(not(unix))]
    let (dev, ino) = (0, 0);
    Some((dev, ino, meta.len(), meta.modified().ok()?))
}

/// Called at the start of every batch. The allowance for restarting a server that dies is per
/// batch, and a batch that refused every task because one start failed has to be able to succeed
/// the next time the controller asks.
pub fn batch_started() {
    SERVERS.with(|table| {
        let mut table = table.borrow_mut();
        table.starts.clear();
        table.refused.clear();
    });
}

thread_local! {
    /// One table per thread, which is one per agent: batches run one after another on the thread
    /// that reads the control channel, and a server is a child process of this one.
    static SERVERS: RefCell<Servers> = RefCell::new(Servers::default());
}

/// A server is keyed by the interpreter and the payload it preloaded: those two decide what a
/// module will import, so a task naming either differently needs a server of its own.
type Key = (String, String);

#[derive(Default)]
struct Servers {
    live: HashMap<Key, Server>,
    /// Starts spent on each key in this batch. The second is the one restart a batch gets; a
    /// server that dies again in the same batch is refused rather than restarted per task, which
    /// would spend a quarter of a second on every one of them.
    starts: HashMap<Key, u32>,
    /// Keys whose server this batch has given up on, with the sentence every remaining task of
    /// the batch is answered with.
    refused: HashMap<Key, String>,
    /// Each blob's fingerprint when its hash last matched its name.
    verified: HashMap<String, Fingerprint>,
}

impl Servers {
    fn run(
        &mut self,
        payload: &PythonPayload,
        remote_tmp: &str,
        blob: &Path,
        args: &Map<String, Value>,
        context: &Context,
        cancelled: &dyn Fn() -> bool,
    ) -> Run {
        let key = (payload.interpreter.clone(), payload.blob.clone());
        if self
            .live
            .get_mut(&key)
            .is_some_and(|server| !server.alive())
        {
            self.live.remove(&key);
        }
        if let Err(msg) = self.verify(payload, remote_tmp, blob, &key) {
            return Run::Done(fail(msg, None));
        }
        if let Some(refused) = self.refused.get(&key) {
            return Run::Done(fail(refused.clone(), None));
        }
        if !self.live.contains_key(&key) {
            let spent = self.starts.entry(key.clone()).or_default();
            *spent += 1;
            if *spent > 2 {
                let msg = format!(
                    "the python server under {} stopped twice in one batch; the rest of the batch is refused",
                    payload.interpreter
                );
                self.refused.insert(key, msg.clone());
                return Run::Done(fail(msg, None));
            }
            match Server::start(&payload.interpreter, blob, cancelled) {
                Ok(server) => {
                    self.live.insert(key.clone(), server);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => return Run::Cancelled,
                Err(err) => {
                    let msg = format!("{err}");
                    self.refused.insert(key, msg.clone());
                    return Run::Done(fail(msg, None));
                }
            }
        }
        let server = self.live.get_mut(&key).expect("just started");
        let run = server.run(payload, args, context, cancelled);
        if !server.alive() {
            self.live.remove(&key);
        }
        run
    }

    /// Hashes the blob unless a live server already runs on it and the file is still the one
    /// whose hash matched: once per server start, and again whenever the fingerprint moves.
    /// Measured on the real union, hashing it costs 1.18 ms, 4 to 9 % of a warm task. The
    /// fingerprint is read before the hash, so a file that changes during the hash is hashed again
    /// next time.
    fn verify(
        &mut self,
        payload: &PythonPayload,
        remote_tmp: &str,
        blob: &Path,
        key: &Key,
    ) -> Result<(), String> {
        let now = fingerprint(blob);
        if self.live.contains_key(key)
            && now.is_some()
            && self.verified.get(&payload.blob) == now.as_ref()
        {
            return Ok(());
        }
        self.verified.remove(&payload.blob);
        match crate::blobs::holds(remote_tmp, &payload.blob) {
            Ok(true) => {
                if let Some(now) = now {
                    self.verified.insert(payload.blob.clone(), now);
                }
                Ok(())
            }
            Ok(false) => Err(format!("payload {} is not on this host", payload.blob)),
            Err(err) => Err(format!("payload {}: {err}", payload.blob)),
        }
    }
}

/// Whether a frame is the parent saying it has imported the module_utils and is waiting.
fn ready(frame: &[u8]) -> bool {
    serde_json::from_slice::<Value>(frame)
        .is_ok_and(|value| value.get("ready").and_then(Value::as_bool) == Some(true))
}

/// The pid of the child the parent has just forked.
fn started(frame: &[u8]) -> io::Result<u32> {
    serde_json::from_slice::<Value>(frame)
        .ok()
        .and_then(|value| value.get("started").and_then(Value::as_u64))
        .and_then(|pid| u32::try_from(pid).ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "the python server did not say which process it started",
            )
        })
}

/// What the module did, as the task's result.
///
/// The module's own JSON is the truth whenever `stdout` is one object and nothing else, exit
/// status included: a module that says `failed: true` and exits non-zero is reporting its own
/// failure, which `TaskResult::failed` already reads. Everything else is a failure of this
/// engine's making and says so in words an operator can act on.
fn result(frame: &Value) -> TaskResult {
    let stdout = frame.get("stdout").and_then(Value::as_str).unwrap_or("");
    let stderr = frame.get("stderr").and_then(Value::as_str).unwrap_or("");
    let said = if stderr.trim().is_empty() {
        None
    } else {
        Some(excerpt(stderr.trim()))
    };
    // A signal is never a result, whatever the module had printed by then: it did not finish, and
    // the commonest reason - the out-of-memory killer - leaves a perfectly parseable half.
    if let Some(signal) = frame.get("signal").and_then(Value::as_i64) {
        return fail(format!("the module was killed by signal {signal}"), said);
    }
    let code = frame.get("exit").and_then(Value::as_i64).unwrap_or(-1);
    // A result too large to carry is the module's own failure, said in the module's terms. The
    // parent read both pipes to the end either way, so the child is not left blocked on a full
    // one; what it did not keep is what would have made a frame the agent refuses, and a refused
    // frame reads as a dead server and poisons the rest of the batch.
    // The bound is the server's `STREAM_LIMIT`, sent in the frame by the side that enforces it.
    if frame.get("truncated").and_then(Value::as_bool) == Some(true) {
        let limit = frame.get("limit").and_then(Value::as_u64).map_or_else(
            || "the server's limit".to_string(),
            |limit| format!("{limit} bytes"),
        );
        return fail(
            format!(
                "the module wrote more than {limit}, which is more than a result can carry (exit status {code})"
            ),
            said,
        );
    }
    if stdout.trim().is_empty() {
        return fail(
            format!("the module wrote no result (exit status {code})"),
            said,
        );
    }
    match serde_json::from_str::<Value>(stdout) {
        // ansible-core's `TaskExecutor._execute` fills in a missing `changed` and nothing more.
        Ok(Value::Object(mut result)) => {
            result.entry("changed").or_insert(Value::Bool(false));
            TaskResult(result)
        }
        _ => fail(
            format!(
                "the module wrote something that is not a result (exit status {code}): {}",
                excerpt(stdout.trim())
            ),
            said,
        ),
    }
}

/// A failed task in Ansible's shape, with what the module wrote to stderr behind the sentence
/// when it wrote anything: a traceback is the one thing an operator needs and the one thing a
/// result object cannot carry.
fn fail(msg: impl Into<String>, stderr: Option<String>) -> TaskResult {
    let msg = match stderr {
        Some(said) => format!("{}: {said}", msg.into()),
        None => msg.into(),
    };
    TaskResult::failed_with(msg)
}

/// The first [`EXCERPT`] bytes of `text`, cut on a character boundary.
fn excerpt(text: &str) -> String {
    if text.len() <= EXCERPT {
        return text.to_string();
    }
    let mut end = EXCERPT;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use serde_json::json;

    use crate::modules::{Context, Run};

    /// A server whose interpreter cannot even start is a task failure carrying the interpreter's
    /// own words, not a silent `ok`.
    ///
    /// What would make this red: the start error swallowed and the task reported ok, which is
    /// the accepted-then-ignored shape this project refuses; or the interpreter's stderr
    /// dropped, which sends an operator looking in the wrong place.
    #[test]
    fn an_interpreter_that_cannot_start_fails_the_task_with_its_own_message() {
        let dir = tempdir();
        let fake = program(
            dir.path(),
            "python3",
            "#!/bin/sh\necho 'boom: no python here' >&2\nexit 3\n",
        );
        let err = Server::start(
            fake.to_str().unwrap(),
            Path::new("/nonexistent.zip"),
            &|| false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("boom: no python here"), "{err}");
        assert!(err.to_string().contains(fake.to_str().unwrap()), "{err}");
    }

    /// An interpreter that is not there at all names itself and the system error, so an operator
    /// is not left guessing which of the two ends is missing.
    #[test]
    fn an_interpreter_that_is_not_there_names_itself() {
        let err = Server::start(
            "/nonexistent/python3",
            Path::new("/nonexistent.zip"),
            &|| false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("/nonexistent/python3"), "{err}");
    }

    /// The module_utils come from the blob, not from the interpreter. This is the proof: the
    /// dev host's `/usr/bin/python3` has no ansible-core at all, and the module still runs.
    ///
    /// What would make this red: the server importing ansible from the interpreter's own
    /// site-packages, which works on a developer's machine and fails on every managed host.
    #[test]
    fn the_module_utils_come_from_the_blob_and_not_from_the_interpreter() {
        assert_ne!(
            Command::new(HOST_PYTHON)
                .args(["-c", "import ansible"])
                .status()
                .expect("the dev host has a python3")
                .code(),
            Some(0),
            "this test is only meaningful on an interpreter without ansible-core"
        );
        let blob = stub_blob(
            r#"
    print(json.dumps({"probe": "ok", "got": args, "fqn": module_fqn, "profile": profile}))
"#,
        );
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let run = server.run(
            &payload("ansible.modules.probe"),
            &args(json!({"x": 1})),
            &Context::default(),
            &|| false,
        );
        let Run::Done(result) = run else {
            panic!("not cancelled")
        };
        assert_eq!(result.0["probe"], "ok", "{:?}", result.0);
        assert_eq!(result.0["got"]["x"], 1);
        assert_eq!(result.0["fqn"], "ansible.modules.probe");
        assert_eq!(result.0["profile"], "legacy");
        assert!(!result.failed());
    }

    /// One server runs task after task, and what one module did to its own process does not
    /// reach the next: each module runs in a fork of a parent that never imports a module.
    ///
    /// What would make this red: running the module in the server process itself, which is the
    /// whole reason the fork is there - measured, the second module then sees the first one's
    /// environment, working directory, `sys.path` and umask.
    #[test]
    fn one_module_does_not_reach_the_next() {
        let blob = stub_blob(
            r#"
    import os
    marker = os.environ.get("VOLANT_PROBE_MARKER", "clean")
    os.environ["VOLANT_PROBE_MARKER"] = "dirty"
    os.chdir("/")
    print(json.dumps({"marker": marker, "cwd": os.getcwd()}))
"#,
        );
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let first = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        let second = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert_eq!(first.0["marker"], "clean");
        assert_eq!(
            second.0["marker"], "clean",
            "the second module saw what the first one did to its own process"
        );
        assert!(server.alive(), "the server outlives a task");
    }

    /// The task's `environment` reaches the module's process, and only that task's.
    #[test]
    fn the_task_environment_reaches_the_module_and_stops_there() {
        let blob = stub_blob(
            r#"
    import os
    print(json.dumps({"seen": os.environ.get("VOLANT_TASK_VAR", "unset")}))
"#,
        );
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let mut context = Context::default();
        context
            .environment
            .insert("VOLANT_TASK_VAR".into(), "here".into());
        let with = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &context,
            &|| false,
        ));
        let without = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert_eq!(with.0["seen"], "here");
        assert_eq!(without.0["seen"], "unset", "it leaked into the next task");
    }

    /// A module that writes nothing fails the task and says so.
    ///
    /// What would make this red: an empty stdout read as an empty result, which reports a task
    /// that did nothing as green. Measured: `os._exit` discards the stdout buffer, so a module
    /// that printed its result and exited that way emits nothing at all - this is the shape that
    /// bug takes on the wire.
    #[test]
    fn a_module_that_writes_no_result_fails_the_task() {
        let blob = stub_blob("\n    pass\n");
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let result = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert!(result.failed());
        assert!(
            result.0["msg"]
                .as_str()
                .unwrap()
                .contains("the module wrote no result"),
            "{:?}",
            result.0
        );
    }

    /// A module that writes something that is not a result fails the task carrying what it wrote,
    /// so the operator reads the traceback instead of guessing.
    #[test]
    fn a_module_that_writes_noise_fails_the_task_carrying_it() {
        let blob = stub_blob("\n    print('Traceback: everything is on fire')\n");
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let result = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert!(result.failed());
        assert!(
            result.0["msg"]
                .as_str()
                .unwrap()
                .contains("everything is on fire"),
            "{:?}",
            result.0
        );
    }

    /// A result with anything after it is not a result.
    ///
    /// What would make this red: parsing the first JSON value and ignoring the rest, which would
    /// report a module green while the bytes behind its result say it printed a warning, a
    /// traceback, or a second result.
    #[test]
    fn a_result_followed_by_noise_is_not_taken_as_the_result() {
        let blob =
            stub_blob("\n    print(json.dumps({\"changed\": True}))\n    print('and then this')\n");
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let result = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert!(result.failed());
        assert!(
            result.0["msg"].as_str().unwrap().contains("and then this"),
            "{:?}",
            result.0
        );
    }

    /// A module killed by a signal fails the task naming the signal, and never reports an empty
    /// `ok`: the module did not finish, whatever it had printed by then.
    #[test]
    fn a_module_killed_by_a_signal_fails_the_task() {
        let blob = stub_blob("\n    import os, signal\n    os.kill(os.getpid(), signal.SIGKILL)\n");
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let result = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert!(result.failed());
        assert!(
            result.0["msg"]
                .as_str()
                .unwrap()
                .contains("the module was killed by signal 9"),
            "{:?}",
            result.0
        );
        assert!(server.alive(), "the server survives its child being killed");
    }

    /// The signal wins over a result the module had already printed: the out-of-memory killer
    /// leaves a perfectly parseable half behind, and a module that did not finish did not succeed.
    ///
    /// What would make this red: `result()` reading `stdout` before it looks at `signal`, which
    /// reports this task `ok` with the module's own words.
    #[test]
    fn a_result_printed_before_a_signal_is_not_taken_as_the_result() {
        let blob = stub_blob(
            "\n    import os, signal\n    print(json.dumps({\"changed\": False, \"msg\": \"all done\"}))\n    sys.stdout.flush()\n    os.kill(os.getpid(), signal.SIGKILL)\n",
        );
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let result = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert!(result.failed(), "{:?}", result.0);
        assert!(
            result.0["msg"]
                .as_str()
                .unwrap()
                .contains("the module was killed by signal 9"),
            "{:?}",
            result.0
        );
    }

    /// The oversized-result sentence quotes the bound the server applied, which it sends in the
    /// frame: the server is the side that enforces it, and a figure kept on this side as well
    /// would go on naming the old bound the day the server's changes.
    ///
    /// What would make this red: the sentence built from a constant of this crate again.
    #[test]
    fn the_oversized_result_sentence_quotes_the_server_s_own_limit() {
        let result = result(&json!({
            "exit": 0,
            "signal": null,
            "stdout": "",
            "stderr": "",
            "truncated": true,
            "limit": 1234,
        }));
        assert!(
            result.0["msg"]
                .as_str()
                .unwrap()
                .starts_with("the module wrote more than 1234 bytes"),
            "{:?}",
            result.0
        );
    }

    /// A cancel that arrives while the server is still importing `module_utils` is answered as a
    /// cancel, without waiting for the server to be ready. The payload's own loader sleeps as it
    /// is imported, which is where a slow `module_utils` import spends its time.
    ///
    /// What would make this red: `Server::start` waiting for the ready frame without asking the
    /// predicate. The task then ends a minute later, at `START_TIMEOUT`, as a refused server
    /// instead of a cancel.
    #[test]
    fn a_cancel_while_the_server_starts_is_a_cancel() {
        let root = tempdir();
        // SAFETY: nextest runs each test in its own process, so this reaches no other test.
        unsafe { std::env::set_var("VOLANT_REMOTE_TMP", root.path()) };
        let payload = cached_payload(root.path(), "\n\nimport time\ntime.sleep(120)\n");
        batch_started();
        let run = run(&payload, &args(json!({})), &Context::default(), &|| true);
        assert!(
            matches!(run, Run::Cancelled),
            "the start was not cancelled: {:?}",
            match run {
                Run::Done(result) => Value::Object(result.0),
                Run::Cancelled => Value::Null,
            }
        );
    }

    /// A payload is hashed once for as long as its server lives, not once per task: measured on
    /// the real union, the hash costs 1.18 ms per task, 4 to 9 % of a warm one.
    ///
    /// What would make this red: `holds` called for every task again.
    #[test]
    fn a_payload_is_hashed_once_while_its_server_lives() {
        let root = tempdir();
        // SAFETY: nextest runs each test in its own process, so this reaches no other test.
        unsafe { std::env::set_var("VOLANT_REMOTE_TMP", root.path()) };
        let payload = cached_payload(root.path(), "\n    print(json.dumps({}))\n");
        batch_started();
        let before = crate::blobs::HASHED.load(Ordering::Relaxed);
        for _ in 0..3 {
            let result = done(run(
                &payload,
                &args(json!({})),
                &Context::default(),
                &|| false,
            ));
            assert!(!result.failed(), "{:?}", result.0);
        }
        assert_eq!(crate::blobs::HASHED.load(Ordering::Relaxed) - before, 1);
    }

    /// A payload replaced under a live server is verified again before the next task uses it.
    /// The replacement is the shape `store` itself writes, a new file renamed over the old one,
    /// with the old length and modification time: only the inode tells the two apart.
    ///
    /// What would make this red: the fingerprint left out of the decision to skip the hash, or
    /// the inode left out of the fingerprint. The second task then runs against bytes nobody
    /// checked.
    #[test]
    fn a_payload_replaced_under_a_live_server_is_verified_again() {
        let root = tempdir();
        // SAFETY: nextest runs each test in its own process, so this reaches no other test.
        unsafe { std::env::set_var("VOLANT_REMOTE_TMP", root.path()) };
        let payload = cached_payload(root.path(), "\n    print(json.dumps({}))\n");
        batch_started();
        let first = done(run(
            &payload,
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert!(!first.failed(), "{:?}", first.0);

        let at = crate::blobs::path(root.path().to_str().unwrap(), &payload.blob).unwrap();
        let old = std::fs::metadata(&at).unwrap();
        let mut bytes = std::fs::read(&at).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        let fresh = at.with_extension("fresh");
        std::fs::write(&fresh, &bytes).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&fresh)
            .unwrap()
            .set_modified(old.modified().unwrap())
            .unwrap();
        std::fs::rename(&fresh, &at).unwrap();

        let second = done(run(
            &payload,
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert_eq!(
            second.0["msg"],
            format!("payload {} is not on this host", payload.blob),
            "{:?}",
            second.0
        );
    }

    /// A module that fails the way Ansible modules fail - a result saying `failed` - is reported
    /// as that result, not as an agent error. The module's own words are the truth.
    #[test]
    fn a_module_that_reports_its_own_failure_is_reported_as_it_wrote_it() {
        let blob = stub_blob(
            "\n    print(json.dumps({\"failed\": True, \"msg\": \"nothing to do here\"}))\n    raise SystemExit(1)\n",
        );
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let result = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert!(result.failed());
        assert_eq!(result.0["msg"], "nothing to do here");
    }

    /// A module that says nothing about `changed` reports `changed: false`, the way
    /// ansible-core's `TaskExecutor._execute` fills it in: `ping` prints `{"ping": "pong"}` and
    /// the reference registers `{"changed": false, "failed": false, "ping": "pong"}` (measured on
    /// 2.19.12; `failed` is a separate normalisation).
    ///
    /// What would make this red: the module's JSON handed back as printed, so `when: r.changed`
    /// on a registered `ping` is undefined here and false under the reference.
    #[test]
    fn a_module_silent_about_changed_reports_it_false() {
        let blob = stub_blob("\n    print(json.dumps({\"ping\": \"pong\"}))\n");
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let result = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert_eq!(
            Value::Object(result.0),
            json!({"changed": false, "ping": "pong"})
        );
    }

    /// The fill-in is only for an absent `changed`: a module that changed something says so.
    ///
    /// What would make this red: `changed: false` written over what the module printed.
    #[test]
    fn a_module_that_reports_a_change_keeps_it() {
        let blob = stub_blob("\n    print(json.dumps({\"changed\": True}))\n");
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let result = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert!(result.changed());
    }

    /// A cancelled task kills the module's process group and answers `Cancelled`, and the server
    /// is still there for the next batch.
    ///
    /// What would make this red: the agent waiting for a module that will never end, which is
    /// the hang a `Cancel` exists to stop; or killing the server along with the child, which
    /// would make every later task pay a restart.
    #[test]
    fn a_cancelled_module_is_killed_and_the_server_survives() {
        let blob = stub_blob(SLEEP_IF_ASKED);
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let run = server.run(
            &payload("ansible.modules.probe"),
            &args(json!({"sleep": 120})),
            &Context::default(),
            &|| true,
        );
        // Not an assertion about elapsed time: the cancelled call only answers `Cancelled` once
        // the server has handed back the child's result, so a kill that missed would have ended
        // this call as a failed task instead.
        assert!(matches!(run, Run::Cancelled), "the task was not cancelled");
        assert!(server.alive());
        // The server answers again, which it could not do if the cancelled task had left a
        // frame of its own unread in the pipe.
        let after = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert_eq!(after.0["slept"], false, "{:?}", after.0);
    }

    /// A module that outlives its `timeout` is killed and reported as timed out, in the same
    /// words a native module uses.
    #[test]
    fn a_module_that_outlives_its_timeout_is_killed() {
        let blob = stub_blob(SLEEP_IF_ASKED);
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let context = Context {
            timeout: Some(Duration::from_secs(1)),
            ..Context::default()
        };
        let result = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({"sleep": 120})),
            &context,
            &|| false,
        ));
        assert!(result.failed());
        assert!(
            result.0["msg"].as_str().unwrap().contains("Timed out"),
            "{:?}",
            result.0
        );
        assert!(server.alive());
    }

    /// A task naming a payload this host does not hold fails saying which one, rather than
    /// starting a server against a blob that is not there.
    ///
    /// What would make this red: the cache trusted on the name, which task 1 already refuses, or
    /// a missing payload reported as a module error - an operator would go looking at the module.
    #[test]
    fn a_task_whose_payload_is_not_cached_fails_naming_it() {
        let hash = "0".repeat(64);
        let result = done(run(
            &PythonPayload {
                blob: hash.clone(),
                module_fqn: "ansible.modules.ping".into(),
                profile: "legacy".into(),
                rlimit_nofile: 0,
                extensions: Map::new(),
                interpreter: HOST_PYTHON.into(),
            },
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert!(result.failed());
        assert_eq!(
            result.0["msg"],
            format!("payload {hash} is not on this host")
        );
    }

    /// The interpreter the task names is the one the module runs under, not whatever the agent
    /// would have picked for itself.
    #[test]
    fn the_payload_names_the_interpreter_the_module_runs_under() {
        let blob = stub_blob(
            r#"
    import sys
    print(json.dumps({"executable": sys.executable}))
"#,
        );
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let result = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        // What that interpreter calls itself, asked of the interpreter rather than assumed: on
        // macOS `/usr/bin/python3` is a shim that hands over to the one inside the command line
        // tools, so the path the module reports is not the path the agent spawned.
        let itself = Command::new(HOST_PYTHON)
            .args(["-c", "import sys; sys.stdout.write(sys.executable)"])
            .output()
            .expect("the host python says where it is");
        assert_eq!(
            result.0["executable"],
            String::from_utf8(itself.stdout).unwrap(),
            "the module ran under an interpreter the payload did not name"
        );
    }

    /// A server that has died fails the task that needed it, saying so, instead of waiting on a
    /// pipe nobody is answering.
    ///
    /// What would make this red: the write or the read blocking for ever on a dead parent, which
    /// is a hung batch rather than a failed task; or the death reported as the module's failure,
    /// which sends an operator to read a module that never ran.
    #[test]
    fn a_server_that_died_fails_the_task_it_was_needed_for() {
        let blob = stub_blob(SLEEP_IF_ASKED);
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        server.child.kill().unwrap();
        server.child.wait().unwrap();

        let result = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert!(result.failed());
        assert!(
            result.0["msg"].as_str().unwrap().contains("stopped"),
            "{:?}",
            result.0
        );
        assert!(!server.alive());
    }

    /// A payload that no server can be started against refuses the rest of the batch with one
    /// sentence, rather than paying a failed start - a quarter of a second, measured - for every
    /// task behind it.
    ///
    /// The blob here is cached and verified but is not a payload at all, so the interpreter
    /// cannot import `module_utils` from it: the same shape as a payload built by a controller
    /// this agent does not understand.
    #[test]
    fn a_payload_no_server_can_start_against_refuses_the_rest_of_the_batch() {
        let root = tempdir();
        // SAFETY: nextest runs each test in its own process, so this reaches no other test.
        unsafe { std::env::set_var("VOLANT_REMOTE_TMP", root.path()) };
        let zip = b"not a payload at all";
        let hash = blake3::hash(zip).to_hex().to_string();
        crate::blobs::store(
            root.path().to_str().unwrap(),
            &hash,
            "bm90IGEgcGF5bG9hZCBhdCBhbGw=",
            false,
        )
        .unwrap();

        let payload = PythonPayload {
            blob: hash,
            module_fqn: "ansible.modules.ping".into(),
            profile: "legacy".into(),
            rlimit_nofile: 0,
            extensions: Map::new(),
            interpreter: HOST_PYTHON.into(),
        };
        batch_started();
        let first = done(run(
            &payload,
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        let second = done(run(
            &payload,
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));

        assert!(first.failed() && second.failed());
        assert!(
            first.0["msg"].as_str().unwrap().contains(HOST_PYTHON),
            "{:?}",
            first.0
        );
        assert_eq!(
            first.0["msg"], second.0["msg"],
            "every task behind the first one is answered with the same sentence"
        );
    }

    /// One server serves every task of a batch: the second module's parent is the first's.
    ///
    /// What would make this red: `Servers::run` starting a server per task, which every other
    /// test here lets through - each result is still right and every failure path still fires.
    /// What is lost is the only thing this task exists for: 12.8 ms against 340 ms, measured.
    ///
    /// The same parent pid then makes the restart allowance observable. A server killed under the
    /// table is replaced once, and the batch is refused when it dies a second time, rather than
    /// paying a quarter of a second per task for a server that will not stay up.
    #[test]
    fn the_server_is_reused_across_the_tasks_of_a_batch() {
        let root = tempdir();
        // SAFETY: nextest runs each test in its own process, so this reaches no other test.
        unsafe { std::env::set_var("VOLANT_REMOTE_TMP", root.path()) };
        // The module reports the server it ran under, and can take that server down with it -
        // which is how the death is made to happen at a moment the test knows, rather than
        // racing a signal against the agent's next look at the table.
        let payload = cached_payload(
            root.path(),
            "\n    import os, signal\n    if args.get(\"kill_parent\"):\n        os.kill(os.getppid(), signal.SIGKILL)\n    print(json.dumps({\"parent\": os.getppid()}))\n",
        );
        let task = |args_: Value| done(run(&payload, &args(args_), &Context::default(), &|| false));
        batch_started();

        let first = task(json!({}));
        assert!(!first.failed(), "{:?}", first.0);
        let second = task(json!({}));
        assert_eq!(
            second.0.get("parent"),
            first.0.get("parent"),
            "a second server was started for the second task: {:?}",
            second.0
        );

        // The task that kills its own server fails, saying so; the one behind it gets the batch's
        // one restart, under a server that is not the first.
        let died = task(json!({"kill_parent": true}));
        assert!(died.failed(), "{:?}", died.0);
        let restarted = task(json!({}));
        assert!(
            !restarted.failed(),
            "the dead server was not replaced: {:?}",
            restarted.0
        );
        assert_ne!(
            restarted.0.get("parent"),
            first.0.get("parent"),
            "the task ran under a server that is gone"
        );

        // A second death in the same batch is refused rather than restarted, so a server that
        // will not stay up costs one start and not one per task.
        assert!(task(json!({"kill_parent": true})).failed());
        let refused = task(json!({}));
        assert!(refused.failed(), "{:?}", refused.0);
        assert!(
            refused.0["msg"]
                .as_str()
                .unwrap()
                .contains("stopped twice in one batch"),
            "{:?}",
            refused.0
        );
    }

    /// A module that fills one pipe while writing its result on the other is read whole.
    ///
    /// What would make this red: the parent reading one pipe to the end before touching the
    /// other. The child fills the 64 KiB stderr buffer long before it prints its result, the
    /// parent is waiting on stdout, and neither ever moves again: the task hangs until the
    /// deadline, or for ever without one.
    #[test]
    fn a_module_that_fills_one_pipe_while_writing_the_other_is_read_whole() {
        let blob = stub_blob(
            "\n    import sys\n    sys.stderr.write(\"x\" * (1024 * 1024))\n    sys.stderr.flush()\n    print(json.dumps({\"noisy\": True}))\n",
        );
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let result = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert_eq!(result.0["noisy"], true, "{:?}", result.0);
    }

    /// A module writing more than a frame can carry fails as an oversized result, and the server
    /// lives.
    ///
    /// What would make this red: the parent putting everything in one frame. The agent refuses a
    /// frame over 64 MiB, so a `slurp` of a large file would be reported as a dead server, the
    /// server would be restarted, and a second one in the batch would refuse every task behind
    /// it - blaming the engine for a module that worked.
    #[test]
    fn a_result_too_large_to_carry_fails_the_task_and_not_the_server() {
        let blob = stub_blob("\n    import sys\n    sys.stdout.write(\"y\" * (5 * 1024 * 1024))\n");
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let result = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert!(result.failed());
        assert!(
            result.0["msg"]
                .as_str()
                .unwrap()
                .contains("more than 4194304 bytes, which is more than a result can carry"),
            "the sentence does not quote the server's own limit: {:?}",
            result.0
        );
        assert!(
            server.alive(),
            "an oversized result is not the server's fault"
        );
    }

    /// A module's standard input is empty, not the agent's own request pipe.
    ///
    /// What would make this red: leaving fd 0 as the child inherited it. A module that reads
    /// stdin - or anything it starts, git asking for a passphrase, apt reaching debconf - waits
    /// on a pipe the agent only writes to between tasks, so it waits for ever: with `timeout:`
    /// the task is a late timeout, without one the batch hangs until a `Cancel`.
    #[test]
    fn a_module_reads_end_of_file_from_its_standard_input() {
        let blob =
            stub_blob("\n    import sys\n    print(json.dumps({\"stdin\": sys.stdin.read()}))\n");
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let result = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert_eq!(result.0["stdin"], "", "{:?}", result.0);
    }

    /// The server's own imports do not come from the agent's working directory.
    ///
    /// What would make this red: leaving the `''` that `python -c` puts at the head of
    /// `sys.path`. It resolves to the agent's working directory at every import, and under
    /// `become` that is the login user's home: a `selectors.py` an ordinary user dropped there
    /// would run as root before the server ever read the payload.
    #[test]
    fn the_agents_working_directory_is_not_on_the_servers_path() {
        let root = tempdir();
        std::fs::write(
            root.path().join("selectors.py"),
            "raise SystemExit('a file in the working directory was imported')\n",
        )
        .unwrap();
        // nextest runs each test in its own process, so this reaches no other test.
        std::env::set_current_dir(root.path()).unwrap();

        let blob = stub_blob("\n    print(json.dumps({\"clean\": True}))\n");
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let result = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert_eq!(result.0["clean"], true, "{:?}", result.0);
    }

    /// `rlimit_nofile` on the payload reaches the module's own process.
    ///
    /// What would make this red: the field carried by the protocol and applied by nobody, which
    /// is how "the reference raises the limit and we do not" is discovered months later by a
    /// module that runs out of descriptors.
    #[test]
    fn the_payload_raises_the_open_file_limit_of_the_module() {
        // Every figure here comes from this process rather than from the host's defaults. A
        // runner whose soft limit already equals its hard limit leaves nothing to raise, so the
        // test lowers its own soft limit first and then asks for the value it started with,
        // which is reachable wherever it runs. Asserting a fixed number would assert the runner.
        let (soft, hard) = open_files();
        assert!(soft > 1, "this host grants too few open files to raise one");
        let floor = if soft > 128 { soft / 2 } else { soft - 1 };
        set_open_files(floor, hard);

        let blob = stub_blob(
            "\n    import resource\n    print(json.dumps({\"soft\": resource.getrlimit(resource.RLIMIT_NOFILE)[0]}))\n",
        );
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let inherited = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert_eq!(
            inherited.0["soft"], floor,
            "the server did not inherit the limit this process set: {:?}",
            inherited.0
        );

        let mut wants = payload("ansible.modules.probe");
        wants.rlimit_nofile = soft;
        let raised = done(server.run(&wants, &args(json!({})), &Context::default(), &|| false));
        assert_eq!(
            raised.0["soft"], soft,
            "the payload's rlimit_nofile never reached the module: {:?}",
            raised.0
        );

        // And only for the task that asked: the next module is back where the server is, because
        // the limit was raised in a process that has since exited.
        let next = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert_eq!(next.0["soft"], floor, "{:?}", next.0);

        set_open_files(soft, hard);
    }

    /// A module that exits with a message keeps the message and fails.
    ///
    /// What would make this red: reading `SystemExit("msg")` as status 0, which drops the one
    /// sentence the module chose to die with.
    #[test]
    fn a_module_that_exits_with_a_message_keeps_it() {
        let blob = stub_blob("\n    raise SystemExit('nothing to work with here')\n");
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let result = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert!(result.failed());
        assert!(
            result.0["msg"]
                .as_str()
                .unwrap()
                .contains("nothing to work with here"),
            "{:?}",
            result.0
        );
    }

    /// A cancel whose child leaves something holding its output is still a cancel.
    ///
    /// Slow on purpose: the module leaves a grandchild in a session of its own holding the pipe
    /// the server drains, so the server cannot finish the task and the agent waits out its reap
    /// allowance before giving up on it.
    ///
    /// What would make this red: propagating that expiry. The task becomes `the python server
    /// stopped` and `run_batch` reports the batch `Failed` - a deliberate cancellation recorded
    /// as an engine failure - and a task with `timeout:` gets the same wrong sentence instead of
    /// the timeout the operator asked for.
    #[test]
    fn a_cancel_is_a_cancel_even_when_the_server_cannot_finish_reaping() {
        let blob = stub_blob(
            "\n    import os, subprocess\n    subprocess.Popen(['sleep', '120'], preexec_fn=os.setsid)\n    import time\n    time.sleep(120)\n",
        );
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let run = server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| true,
        );
        assert!(
            matches!(run, Run::Cancelled),
            "a cancel became something else"
        );
    }

    /// An interpreter that starts, says nothing and ends is a start failure naming itself and
    /// saying it said nothing, rather than an empty message an operator cannot act on.
    ///
    /// What would make this red: reading the absence of a ready frame as a server that is ready,
    /// which sends the first task into a pipe nobody is reading.
    #[test]
    fn an_interpreter_that_says_nothing_at_all_says_that() {
        let dir = tempdir();
        let fake = program(dir.path(), "python3", "#!/bin/sh\nexit 0\n");
        let err = Server::start(
            fake.to_str().unwrap(),
            Path::new("/nonexistent.zip"),
            &|| false,
        )
        .unwrap_err();
        assert!(err.to_string().contains(fake.to_str().unwrap()), "{err}");
        assert!(err.to_string().contains("it said nothing"), "{err}");
    }

    /// A server that answers a request with anything but the pid of the child it forked fails the
    /// task, and the table starts a fresh one for the next.
    ///
    /// What would make this red: taking whatever frame arrives as the child's pid. The agent
    /// would then kill a process it was never told about on the next deadline - a pid it did not
    /// choose, on a host it does not own - and read the real result as a `started` frame for the
    /// task behind it, one task out of step for the rest of the batch.
    #[test]
    fn a_server_that_does_not_name_the_process_it_started_fails_the_task() {
        let dir = tempdir();
        let fake = program(
            dir.path(),
            "python3",
            "#!/bin/sh\nprintf '\\0\\0\\0\\016{\"ready\":true}'\nprintf '\\0\\0\\0\\012{\"oops\":1}'\nsleep 30\n",
        );
        let mut server =
            Server::start(fake.to_str().unwrap(), Path::new("/blob"), &|| false).unwrap();
        let result = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert!(result.failed());
        assert!(
            result.0["msg"]
                .as_str()
                .unwrap()
                .contains("did not say which process it started"),
            "{:?}",
            result.0
        );
        assert!(!server.alive(), "a server that lost the thread is not kept");
    }

    /// A server that ends in the middle of a task fails that task and says it was the server.
    ///
    /// What would make this red: the read at the end of a closed pipe taken for an empty result,
    /// which reports a module that never ran as a task that did nothing.
    #[test]
    fn a_server_that_ends_in_the_middle_of_a_task_fails_it() {
        let dir = tempdir();
        let fake = program(
            dir.path(),
            "python3",
            "#!/bin/sh\nprintf '\\0\\0\\0\\016{\"ready\":true}'\nexit 0\n",
        );
        let mut server =
            Server::start(fake.to_str().unwrap(), Path::new("/blob"), &|| false).unwrap();
        let result = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert!(result.failed());
        assert!(
            result.0["msg"].as_str().unwrap().contains("stopped"),
            "{:?}",
            result.0
        );
    }

    /// A server that takes a request and never answers fails the task rather than holding the
    /// batch behind it.
    ///
    /// Slow on purpose: the wait is the agent's reap allowance, and the point of the test is that
    /// the allowance ends. What would make this red is removing the bound - the task, the batch
    /// and the play would then wait for a frame that never comes, and only a `Cancel` would end
    /// it.
    #[test]
    fn a_server_that_never_answers_fails_the_task_instead_of_waiting_for_ever() {
        let asked = Instant::now();
        let dir = tempdir();
        let fake = program(
            dir.path(),
            "python3",
            "#!/bin/sh\nprintf '\\0\\0\\0\\016{\"ready\":true}'\nsleep 120\n",
        );
        let mut server =
            Server::start(fake.to_str().unwrap(), Path::new("/blob"), &|| false).unwrap();
        let result = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert!(result.failed());
        assert!(
            result.0["msg"].as_str().unwrap().contains("did not answer"),
            "{:?}",
            result.0
        );
        assert!(
            !server.alive(),
            "a server that stopped answering is not kept"
        );
        // Not a measurement of how fast anything is: the allowance is ten seconds and the fake
        // sleeps for two minutes, so a ceiling between the two says only that the agent stopped
        // waiting on its own terms rather than on the fake's.
        assert!(
            asked.elapsed() < Duration::from_secs(60),
            "the agent waited on the server rather than on its own allowance"
        );
    }

    /// A module cut off in the middle of its result fails the task carrying what arrived.
    ///
    /// What would make this red: a parser that takes the first complete value and ignores the
    /// rest, or one that repairs what it is given. Half an object is not a result, and a module
    /// killed by the out-of-memory killer mid-print leaves exactly this.
    #[test]
    fn a_module_that_writes_half_a_result_fails_the_task_carrying_it() {
        let blob = stub_blob("\n    import sys\n    sys.stdout.write('{\"changed\": tr')\n");
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let result = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert!(result.failed());
        assert!(
            result.0["msg"]
                .as_str()
                .unwrap()
                .contains("{\"changed\": tr"),
            "{:?}",
            result.0
        );
    }

    /// A module that sleeps only when the task asks it to, so one blob serves both the task that
    /// has to be interrupted and the task that proves the server still answers afterwards.
    const SLEEP_IF_ASKED: &str = r#"
    if args.get("sleep"):
        import time
        time.sleep(args["sleep"])
    print(json.dumps({"slept": bool(args.get("sleep"))}))
"#;

    /// The interpreter every test here runs against. It has no ansible-core, which is what makes
    /// the blob the only place the module_utils can come from.
    const HOST_PYTHON: &str = "/usr/bin/python3";

    fn done(run: Run) -> TaskResult {
        match run {
            Run::Done(result) => result,
            Run::Cancelled => panic!("the task was cancelled"),
        }
    }

    fn payload(fqn: &str) -> PythonPayload {
        PythonPayload {
            blob: "0".repeat(64),
            module_fqn: fqn.into(),
            profile: "legacy".into(),
            rlimit_nofile: 0,
            extensions: Map::new(),
            interpreter: HOST_PYTHON.into(),
        }
    }

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    /// A payload directory holding a stub `_loader` whose `run_module` runs `body`.
    ///
    /// A directory rather than a zip: `sys.path` takes either, the Rust side hands over a path
    /// either way, and writing a zip by hand here would buy nothing the golden suite does not
    /// already cover with the real thing.
    fn stub_blob(body: &str) -> TempDir {
        let dir = tempdir();
        let package = dir.path().join("ansible/module_utils/_internal/_ansiballz");
        std::fs::create_dir_all(&package).unwrap();
        for level in [
            "ansible",
            "ansible/module_utils",
            "ansible/module_utils/_internal",
            "ansible/module_utils/_internal/_ansiballz",
        ] {
            std::fs::write(dir.path().join(level).join("__init__.py"), "").unwrap();
        }
        // The server preloads this one too, for the reason its own comment gives.
        std::fs::write(dir.path().join("ansible/module_utils/basic.py"), "# stub\n").unwrap();
        std::fs::write(
            package.join("_loader.py"),
            format!(
                "import json, sys\n\n\ndef run_module(json_params, profile, module_fqn, modlib_path, extensions):\n    args = json.loads(json_params)[\"ANSIBLE_MODULE_ARGS\"]{body}"
            ),
        )
        .unwrap();
        dir
    }

    /// A payload in this agent's own cache, built as a real zip by the host interpreter.
    ///
    /// The shape the controller sends, rather than the directory the tests above hand straight to
    /// a server: `python::run` verifies the blob by hashing the file it holds, so it needs one.
    /// The archive has no `.zip` in its name and does not need one - `zipimport` recognises an
    /// archive by its central directory.
    fn cached_payload(remote_tmp: &Path, body: &str) -> PythonPayload {
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
        let built = Command::new(HOST_PYTHON)
            .args(["-c", BUILD, body])
            .output()
            .expect("the host python builds the payload");
        assert!(
            built.status.success(),
            "{}",
            String::from_utf8_lossy(&built.stderr)
        );
        let zip_b64 = String::from_utf8(built.stdout).unwrap();
        let zip = volant_protocol::encoding::b64_decode(&zip_b64).unwrap();
        let hash = blake3::hash(&zip).to_hex().to_string();
        crate::blobs::store(remote_tmp.to_str().unwrap(), &hash, &zip_b64, false).unwrap();
        PythonPayload {
            blob: hash,
            module_fqn: "ansible.modules.probe".into(),
            profile: "legacy".into(),
            rlimit_nofile: 0,
            extensions: Map::new(),
            interpreter: HOST_PYTHON.into(),
        }
    }

    /// This process's own soft and hard limits on open files.
    #[cfg(unix)]
    fn open_files() -> (u64, u64) {
        let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
        // SAFETY: `getrlimit` fills a structure this call owns and cannot fail for a resource
        // every process has.
        let limit = unsafe {
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()), 0);
            limit.assume_init()
        };
        (limit.rlim_cur as u64, limit.rlim_max as u64)
    }

    /// Sets this process's soft limit on open files, which its children inherit.
    ///
    /// nextest runs each test in its own process, so this reaches no other test; lowering a soft
    /// limit needs no privilege, and raising one back to where it started is allowed for the same
    /// reason the payload's own raise is.
    #[cfg(unix)]
    fn set_open_files(soft: u64, hard: u64) {
        let limit = libc::rlimit {
            rlim_cur: soft as libc::rlim_t,
            rlim_max: hard as libc::rlim_t,
        };
        // SAFETY: `limit` is a fully initialised structure that outlives the call.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
    }

    fn program(dir: &Path, name: &str, source: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        std::fs::create_dir_all(dir).unwrap();
        let at = dir.join(name);
        std::fs::write(&at, source).unwrap();
        std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755)).unwrap();
        at
    }

    /// A directory of this process's own, removed when the test ends. The agent carries no
    /// temporary-directory dependency, for the reason `blobs.rs` gives.
    struct TempDir(PathBuf);

    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tempdir() -> TempDir {
        static COUNT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "volant-python-test-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("the test directory is created");
        TempDir(dir)
    }
}
