// SPDX-License-Identifier: GPL-3.0-or-later
//! One warm Python server per interpreter and payload, forking a child per module.
//!
//! The server is where this release's Python support earns its name. Measured on the reference's
//! `ping`: a module started from cold takes 340 ms, forking alone takes it to 266 ms, and a
//! parent that has already imported `module_utils` from the payload takes it to 12.8 ms. The
//! import is paid once per run, by a parent that never runs a module's own code, so nothing one
//! module did reaches the next. What a module imports from the payload and the standard library
//! is imported in the parent too, once its first result is sent, so its later tasks skip that
//! part.
//!
//! The parent and this side speak the agent's own frames - four bytes of big-endian length, then
//! JSON - so there is one framing in the whole agent rather than two.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
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

/// How long a task waits for the server to finish importing ahead for the previous task's module
/// before it gives up on that server and runs under a fresh one. Measured on `target`, the
/// heaviest of the proof roles' modules (`systemd_service`) takes about 200 ms; the margin is for
/// a loaded host, and the bound is what keeps an import that blocks from reaching the task.
const PRELOAD_WAIT: Duration = Duration::from_secs(3);

/// How long the agent waits for a dead server's stderr to reach its end before it reports what
/// has arrived. Long enough for a pipe that is already closed to be drained, short enough that a
/// grandchild holding it open costs the task nothing it can feel.
const LAST_WORDS_WAIT: Duration = Duration::from_millis(500);

/// What an operator is shown of a module's own output when it is not a result.
const EXCERPT: usize = 2048;

/// Where a Python task's time went, as the server measured it: the fork, the module's import up
/// to the moment it builds its `AnsibleModule`, and the rest of its run. Each is `None` when the
/// server did not report it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub fork: Option<u64>,
    pub import: Option<u64>,
    pub module: Option<u64>,
}

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
    /// What the server was started with, for the replacement a stale server asks for.
    blob: PathBuf,
    environment: BTreeMap<String, String>,
    /// What the last result frame said about the task's time.
    timing: Timing,
    /// The module the server is importing ahead for, once its result frame said it would.
    preloading: Option<String>,
    /// Modules a server gave up on importing ahead for in time; its replacements are asked not
    /// to import them ahead again.
    abandoned: HashSet<String>,
    /// `REAP_TIMEOUT` and `PRELOAD_WAIT`; fields so a test can shorten them.
    reap_timeout: Duration,
    preload_wait: Duration,
}

/// Where the server stands before a task is sent to it.
enum Settled {
    Ready,
    Cancelled,
    /// Still importing ahead for this module once `PRELOAD_WAIT` has passed.
    Stuck(String),
}

impl Server {
    /// [`Server::start_in`] with the agent's own environment alone.
    #[cfg(all(test, unix))]
    pub fn start(
        interpreter: &str,
        blob: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<Server> {
        Self::start_in(interpreter, blob, &BTreeMap::new(), cancelled)
    }

    /// Starts one server under `interpreter`, with `environment` added to what it inherits,
    /// preloading module_utils from `blob`.
    ///
    /// An interpreter that cannot start is an error carrying its own words: it is the one party
    /// that knows why, and an operator reading "the module failed" would go looking at the
    /// module.
    ///
    /// The wait for the ready frame asks `cancelled` every [`CANCEL_POLL`], as the run loop
    /// does: importing `module_utils` is the slow part of a start, and a cancel that arrives
    /// during it is answered then rather than at [`START_TIMEOUT`]. A cancelled start is an
    /// [`io::ErrorKind::Interrupted`] error, and the server is killed with it.
    fn start_in(
        interpreter: &str,
        blob: &Path,
        environment: &BTreeMap<String, String>,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<Server> {
        let mut child = Command::new(interpreter)
            .envs(environment)
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
            blob: blob.to_path_buf(),
            environment: environment.clone(),
            timing: Timing::default(),
            preloading: None,
            abandoned: HashSet::new(),
            reap_timeout: REAP_TIMEOUT,
            preload_wait: PRELOAD_WAIT,
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
        let outcome = match self.settle(cancelled) {
            Ok(Settled::Ready) => self.attempt(payload, args, context, cancelled),
            Ok(Settled::Cancelled) => return Run::Cancelled,
            // Given up on rather than waited for: the task runs under a fresh server, which is
            // asked not to import that module ahead again.
            Ok(Settled::Stuck(module)) => {
                self.abandoned.insert(module);
                if let Err(run) = self.replace(cancelled) {
                    return run;
                }
                self.attempt(payload, args, context, cancelled)
            }
            Err(err) => Err(err),
        };
        let outcome = match outcome {
            // The server had imported from a directory that has changed since, and ended rather
            // than fork a child from what it imported then: a fresh one runs the task.
            Ok(None) => {
                if let Err(run) = self.replace(cancelled) {
                    return run;
                }
                self.attempt(payload, args, context, cancelled)
            }
            other => other,
        };
        match outcome {
            Ok(Some(run)) => run,
            Ok(None) => Run::Done(fail(
                format!(
                    "the python server under {} asked to be replaced as soon as it started",
                    self.interpreter
                ),
                None,
            )),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => Run::Cancelled,
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

    /// Puts a fresh server in this one's place, with the same interpreter, payload and
    /// environment, and the modules this one gave up importing ahead. A replacement that cannot
    /// start fails the task with its own words: the old server's stderr belongs to a process that
    /// had nothing wrong with it.
    fn replace(&mut self, cancelled: &dyn Fn() -> bool) -> Result<(), Run> {
        match Server::start_in(&self.interpreter, &self.blob, &self.environment, cancelled) {
            Ok(mut fresh) => {
                fresh.abandoned = std::mem::take(&mut self.abandoned);
                *self = fresh;
                Ok(())
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => Err(Run::Cancelled),
            Err(err) => Err(Run::Done(fail(format!("{err}"), None))),
        }
    }

    /// Waits, before a task is sent, for the server to finish importing ahead for the previous
    /// task's module: at most `PRELOAD_WAIT`, and answering `cancelled` every [`CANCEL_POLL`].
    /// Nothing has been sent yet, so a cancel or a server given up on leaves no frame behind.
    fn settle(&mut self, cancelled: &dyn Fn() -> bool) -> io::Result<Settled> {
        let Some(module) = self.preloading.take() else {
            return Ok(Settled::Ready);
        };
        let deadline = Instant::now() + self.preload_wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.frames.recv_timeout(left.min(CANCEL_POLL)) {
                Ok(frame) => {
                    let frame: Value = serde_json::from_slice(&frame?)?;
                    if frame.get("preloaded").and_then(Value::as_bool) == Some(true) {
                        return Ok(Settled::Ready);
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the python server did not say its imports were done",
                    ));
                }
                Err(RecvTimeoutError::Timeout) => {
                    if cancelled() {
                        self.preloading = Some(module);
                        return Ok(Settled::Cancelled);
                    }
                    if Instant::now() >= deadline {
                        return Ok(Settled::Stuck(module));
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "the python server ended",
                    ));
                }
            }
        }
    }

    /// Notes, from a result frame, that the server now imports ahead for `module`.
    fn note_preload(&mut self, frame: &Value, module: &str) {
        if frame.get("preloading").and_then(Value::as_bool) == Some(true) {
            self.preloading = Some(module.to_string());
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
    ) -> io::Result<Option<Run>> {
        self.timing = Timing::default();
        let request = json!({
            "args": args,
            "profile": payload.profile,
            "module_fqn": payload.module_fqn,
            "extensions": payload.extensions,
            "environment": context.environment,
            "rlimit_nofile": payload.rlimit_nofile,
            "preload": !self.abandoned.contains(&payload.module_fqn),
        });
        write_frame(&mut self.stdin, &serde_json::to_vec(&request)?)?;

        // The fork happens as soon as the request is read, which is once the server has done
        // the imports it takes on after a module's first result (the child of that module
        // already did the same imports in about the same time), so this frame is prompt. It is
        // waited for even when the task is already cancelled: without the pid there is nothing
        // to kill, and the result frame behind it would desynchronise the next task.
        let first = self.frame(Instant::now() + self.reap_timeout)?;
        if stale(&first) {
            return Ok(None);
        }
        let pid = started(&first)?;

        let deadline = context.timeout.map(|t| Instant::now() + t);
        loop {
            if let Some(end) = deadline
                && Instant::now() >= end
            {
                kill_group(pid);
                self.reap(&payload.module_fqn);
                let seconds = context.timeout.unwrap_or_default().as_secs();
                return Ok(Some(Run::Done(TaskResult::timed_out(seconds))));
            }
            match self.frames.recv_timeout(CANCEL_POLL) {
                Ok(frame) => {
                    let frame = serde_json::from_slice(&frame?)?;
                    self.timing = timing(&frame);
                    self.note_preload(&frame, &payload.module_fqn);
                    return Ok(Some(Run::Done(result(&frame))));
                }
                Err(RecvTimeoutError::Timeout) => {
                    if cancelled() {
                        kill_group(pid);
                        self.reap(&payload.module_fqn);
                        return Ok(Some(Run::Cancelled));
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
    fn reap(&mut self, module: &str) {
        match self.frame(Instant::now() + self.reap_timeout) {
            Ok(frame) => {
                if let Ok(frame) = serde_json::from_slice::<Value>(&frame) {
                    self.note_preload(&frame, module);
                }
            }
            Err(_) => {
                let _ = self.child.kill();
            }
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

/// Runs one task that carries a payload, starting a server for it if this agent has none yet,
/// and says where its time went.
///
/// A blob is trusted because its bytes hash to its name, and the task that uses one is where that
/// has to hold. `Servers::verify` decides when the hash has to be read again.
pub fn run(
    payload: &PythonPayload,
    args: &Map<String, Value>,
    context: &Context,
    cancelled: &dyn Fn() -> bool,
) -> (Run, Timing) {
    // Set by the dispatcher from `blobs::remote_tmp`, and by a test to its own directory.
    let remote_tmp = &context.remote_tmp;
    let blob = match crate::blobs::path(remote_tmp, &payload.blob) {
        Ok(blob) => blob,
        Err(err) => {
            return (
                Run::Done(fail(format!("payload {}: {err}", payload.blob), None)),
                Timing::default(),
            );
        }
    };
    SERVERS.with(|table| {
        table
            .borrow_mut()
            .run(payload, remote_tmp, &blob, args, context, cancelled)
    })
}

/// What `Servers::verify` compares to decide that a blob is still the file it hashed: device,
/// inode, length, modification time and change time. `store` renames a new file over an old one,
/// which changes the inode whatever the length and the time say; a write in place that puts the
/// old modification time back (`touch -r`, `cp --preserve=timestamps`) still moves the change
/// time, which no unprivileged call can set.
type Fingerprint = (u64, u64, u64, std::time::SystemTime, i64, i64);

fn fingerprint(at: &Path) -> Option<Fingerprint> {
    let meta = std::fs::metadata(at).ok()?;
    #[cfg(unix)]
    let (dev, ino, ctime, ctime_nsec) = {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec())
    };
    #[cfg(not(unix))]
    let (dev, ino, ctime, ctime_nsec) = (0, 0, 0, 0);
    Some((
        dev,
        ino,
        meta.len(),
        meta.modified().ok()?,
        ctime,
        ctime_nsec,
    ))
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

/// A server is keyed by the interpreter, the payload it preloaded and the task's environment, and
/// is started with that environment: the three decide what a module imports and what an import
/// reads (`PYTHONPATH`, `PYTHONHOME` and the rest are read by an interpreter as it starts, and a
/// library can read a variable once, as it loads). A task naming any of them differently needs a
/// server of its own; a play-level `environment:` keeps one warm server for all its tasks.
type Key = (String, String, BTreeMap<String, String>);

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
    ) -> (Run, Timing) {
        let key = (
            payload.interpreter.clone(),
            payload.blob.clone(),
            context.environment.clone(),
        );
        if self
            .live
            .get_mut(&key)
            .is_some_and(|server| !server.alive())
        {
            self.live.remove(&key);
        }
        if let Err(msg) = self.verify(payload, remote_tmp, blob, &key) {
            return (Run::Done(fail(msg, None)), Timing::default());
        }
        if let Some(refused) = self.refused.get(&key) {
            return (Run::Done(fail(refused.clone(), None)), Timing::default());
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
                return (Run::Done(fail(msg, None)), Timing::default());
            }
            match Server::start_in(&payload.interpreter, blob, &context.environment, cancelled) {
                Ok(server) => {
                    // One server per environment would grow without bound under an environment
                    // that changes per task (a loop setting a variable from `item`): a server with
                    // an environment replaces any other of the same interpreter and payload that
                    // has one, and costs a start when two of them alternate.
                    if !key.2.is_empty() {
                        let evicted: Vec<Key> = self
                            .live
                            .keys()
                            .filter(|other| {
                                other.0 == key.0 && other.1 == key.1 && !other.2.is_empty()
                            })
                            .cloned()
                            .collect();
                        // Evicted, not stopped: the start it cost is not one of the batch's
                        // allowance for servers that die.
                        for other in evicted {
                            self.live.remove(&other);
                            self.starts.remove(&other);
                        }
                    }
                    self.live.insert(key.clone(), server);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                    return (Run::Cancelled, Timing::default());
                }
                Err(err) => {
                    let msg = format!("{err}");
                    self.refused.insert(key, msg.clone());
                    return (Run::Done(fail(msg, None)), Timing::default());
                }
            }
        }
        let server = self.live.get_mut(&key).expect("just started");
        let run = server.run(payload, args, context, cancelled);
        let timing = server.timing;
        if !server.alive() {
            self.live.remove(&key);
        }
        (run, timing)
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

/// Whether the parent is saying that a package has moved under it since it started, and that it
/// has ended rather than run the request.
fn stale(frame: &[u8]) -> bool {
    serde_json::from_slice::<Value>(frame)
        .is_ok_and(|value| value.get("stale").and_then(Value::as_bool) == Some(true))
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

/// The server's own measure of the task, in microseconds. A frame from a server that measured
/// nothing, or a value that is not a count, reads as unmeasured rather than as zero.
fn timing(frame: &Value) -> Timing {
    let field = |name: &str| {
        frame
            .get("timing")
            .and_then(|timing| timing.get(name))
            .and_then(Value::as_u64)
    };
    Timing {
        fork: field("fork_us"),
        import: field("import_us"),
        module: field("module_us"),
    }
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
        Ok(Value::Object(result)) => crate::modules::module_result(result),
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
mod tests;
