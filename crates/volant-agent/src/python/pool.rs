// SPDX-License-Identifier: GPL-3.0-or-later
//! The servers this agent keeps, one per interpreter, payload and environment.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::Path;

use serde_json::{Map, Value};
use volant_protocol::PythonPayload;

use super::Timing;
use super::frame::fail;
use super::server::Server;
use crate::modules::{Context, Run};
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
