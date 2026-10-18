// SPDX-License-Identifier: GPL-3.0-or-later
//! Messages exchanged over frames. Field names are part of the protocol.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Bumped when a message changes shape. Controller and agent refuse to talk across versions.
///
/// 3 added `Task.environment`: a task now carries the variables its module runs with, and an
/// agent that does not know the field would run the command without them - which is the "accepted
/// then ignored" shape this project refuses.
///
/// 4 added the blob messages and `Task.payload`: a Python module travels as a content-addressed
/// zip sent once per run, and a task names the payload it needs rather than carrying it. An
/// agent that did not know the field would treat a Python task as an unknown module.
///
/// 5 added `Task.files`: a file travels as a content-addressed blob and the agent hands the
/// module a private copy it consumes; an agent that did not know the field would run `copy` with
/// no source.
///
/// `PutBlob.staged` did not move it again: no published release speaks 5 (`v0.1.0-alpha.7`
/// speaks 4), so no agent that speaks 5 without knowing the field exists anywhere.
///
/// 6 sends a blob as its own binary frame after `PutBlob`, deflated when that is shorter, rather
/// than as base64 inside the JSON. An agent that speaks 5 would read the blob's frame as a
/// malformed message, drop it, and answer a `put_blob` with no `zip_b64`; the handshake refuses
/// it first, with the same sentence as every other mismatch.
///
/// 7 added `Task.job`: a task the controller wants started in the background. An agent that did
/// not know the field would run the task to the end in front of the controller and answer a
/// result with no job id, which is the "accepted then ignored" shape again.
///
/// 8 added `PutFile`: a staged blob moved to a path of the host. An agent that does not know the
/// message would drop it as malformed and leave the controller waiting for an answer.
pub const PROTOCOL_VERSION: u32 = 8;

/// How the frame after a `PutBlob` carries the blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobEncoding {
    /// The bytes themselves.
    Raw,
    /// A zlib stream, see [`crate::encoding::deflate`].
    Deflate,
}

/// Controller to agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToAgent {
    Hello {
        protocol: u32,
    },
    /// Run these tasks in order. One batch at a time: the controller waits for `BatchDone`.
    RunBatch {
        id: u64,
        tasks: Vec<Task>,
    },
    Cancel {
        id: u64,
    },
    /// Is this payload already in the agent's cache? Answered by [`FromAgent::BlobState`].
    HasBlob {
        hash: String,
    },
    /// A blob follows: the next frame on the wire is the blob itself, encoded as `encoding` says,
    /// and never a message. The agent verifies the hash against the decoded bytes before the
    /// blob can be used.
    PutBlob {
        hash: String,
        /// The decoded length. The agent refuses a frame longer than this, reading it through
        /// without holding it, and stops inflating once the output would pass it.
        len: u64,
        encoding: BlobEncoding,
        /// A file one task stages rather than a payload the link reuses. The agent keeps it out
        /// of its shared cache, in a directory of this connection's own that goes when the
        /// connection does, and `HasBlob` never answers for it: two links to one host account
        /// share the cache, and one consuming a file the other was told was there failed the
        /// other's task.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        staged: bool,
    },
    /// Moves the staged blob `hash` to `path`, an absolute path, atomically, with `mode` (0600
    /// when absent). Answered by [`FromAgent::PutFileDone`].
    PutFile {
        hash: String,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
    },
}

/// Agent to controller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromAgent {
    Ready {
        protocol: u32,
        version: String,
        arch: String,
        /// Absolute paths of the Python interpreters this host has, best first, in the
        /// reference's own fallback order. Empty when the host has none.
        ///
        /// Additive, so it did not move `PROTOCOL_VERSION`: absent on the wire when empty,
        /// defaulted on the way in, and `FromAgent` denies no unknown field, so either side
        /// parses the other's `Ready` whichever of the two is older. What makes a mismatch
        /// unreachable rather than merely survivable is that the controller uploads the agent
        /// binary it shipped with. An empty list therefore means "this agent reported none",
        /// which is not quite "this host has none" - a refusal has to be worded as the former.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        interpreters: Vec<String>,
        /// The native modules this agent runs in place of their Python payload, by the name the
        /// reference gives them. Additive like `interpreters`: an older agent reports none, and
        /// the controller then expects every Python task to run as Python.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        natives: Vec<String>,
    },
    TaskResult {
        batch: u64,
        index: usize,
        result: TaskResult,
        /// How the agent ran the task and how long it took. Absent from an older agent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ran: Option<Ran>,
    },
    BatchDone {
        batch: u64,
        outcome: BatchOutcome,
    },
    Log {
        level: LogLevel,
        message: String,
    },
    /// Whether the agent holds this payload, after a `has_blob` asked or a `put_blob` landed.
    /// `present: false` after a `put_blob` means the payload was refused; the `log` before it
    /// says why.
    BlobState {
        hash: String,
        present: bool,
    },
    /// The end of a `put_file`: `error` says why the file is not at its path, `changed` whether
    /// its content differs from what the path held.
    PutFileDone {
        hash: String,
        changed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BatchOutcome {
    Completed,
    /// The task at `at` failed without `ignore_errors`; later tasks did not run.
    Failed {
        at: usize,
    },
    /// Cancelled while the task at `at` was running or about to run.
    Cancelled {
        at: usize,
    },
}

/// One task, fully resolved by the controller: no templates left.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Task {
    /// Module name as written in the playbook, short or fully qualified.
    pub module: String,
    #[serde(default)]
    pub args: Map<String, Value>,
    #[serde(default)]
    pub ignore_errors: bool,
    /// Seconds allowed for the module to run; the agent kills it past that (Ansible's `timeout`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// Variables the module's process runs with, added to the ones the agent already has
    /// (Ansible's `environment`). Resolved by the controller: the play's layer, the block's and
    /// the task's are merged and rendered there, so the agent only sets what it is handed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
    /// Present when this task is a Python module rather than a native one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<PythonPayload>,
    /// Files the agent stages before running the module. Each is consumed by this task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<StagedFile>,
    /// Run the Python payload even when the agent has an enabled native module of this name.
    /// The controller sets it for `[volant] native_modules = false` and for a `setup` whose
    /// facts it could not prove the play reads only from the native collector's keys.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force_python: bool,
    /// Start the task as a background job and answer at once with its id (Ansible's `async`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<AsyncJob>,
}

/// How a task runs in the background: what ansible-core hands its `async_wrapper`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AsyncJob {
    /// Seconds the job may run before its process group is killed (`async`).
    pub limit: u64,
    /// Where the job's status file goes, `~` not yet expanded (`ansible_async_dir`).
    pub dir: String,
}

/// Which code ran a task that carries a Python payload. A task without one (`command`, `shell`,
/// `raw`) always runs in the agent and reports `Native`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecPath {
    /// The agent's own implementation of the module.
    Native,
    /// The Python payload, because no enabled native exists or the task forced Python.
    Python,
    /// A native was tried, handed the task back before touching the host, and the Python
    /// payload ran instead.
    Fallback,
}

/// What the agent reports about how it ran one task, next to the task's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ran {
    pub path: ExecPath,
    /// Why a native handed the task back, for `Fallback`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Wall time of the task in the agent.
    pub micros: u64,
    /// Python only: fork, import of the module, and the module's own run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module_micros: Option<u64>,
}

/// A file the agent takes out of its cache and hands to the module under `args[arg]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedFile {
    /// The module argument that receives the staged path: `src` for `copy` and `unarchive`.
    pub arg: String,
    /// Lowercase hex blake3 of the file's bytes, as for the union blob.
    pub blob: String,
}

/// A module payload already on the host, named by the blake3 hash of the zip it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PythonPayload {
    /// Lowercase hex blake3 of the decoded zip. Names the blob and is verified before use.
    pub blob: String,
    /// `ansible.modules.ping`, as `modify_module` reports it.
    pub module_fqn: String,
    /// `legacy` on 2.19.12; the serialisation profile the module was built for.
    pub profile: String,
    /// `rlimit_nofile` from the wrapper; 0 means leave the limit alone.
    pub rlimit_nofile: u64,
    /// The wrapper's `extensions`; `{}` on 2.19.12.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extensions: Map<String, Value>,
    /// Absolute path of the interpreter the agent must run this under.
    pub interpreter: String,
}

/// A module result, in the free-form shape Ansible modules return.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskResult(pub Map<String, Value>);

impl TaskResult {
    /// ansible-core 2.19.12's rule, measured: `changed` decides by Python truthiness, so
    /// `"yes"`, `"false"` and `1` report `changed:` and `0`, `null`, `""` and `[]` do not. The
    /// value is registered as the module wrote it (`r.changed` reads `yes`), so nothing here
    /// rewrites it; `changed_when` does, with a boolean.
    pub fn changed(&self) -> bool {
        self.0.get("changed").is_some_and(truthy)
    }

    /// By Python truthiness, as `changed` and `failed` are: measured on ansible-core 2.19.12, a
    /// module printing `skipped: "yes"` shows `skipping:`, `r is skipped` is `true` and the recap
    /// counts `skipped=1`.
    pub fn skipped(&self) -> bool {
        self.0.get("skipped").is_some_and(truthy)
    }

    /// ansible-core 2.19.12's rule, measured: a present `failed` decides by Python truthiness
    /// (`"yes"` and `"false"` fail, `0`, `null`, `""` and `[]` do not), and only when it is
    /// absent does `rc` decide, failing on anything present but `0` or `"0"` - `null`, `"2"`,
    /// `""` and `true` fail, `0.0` and `false` (both equal to `0` in Python) do not.
    /// `failed_when: false` can rescue a non-zero `rc`.
    pub fn failed(&self) -> bool {
        match self.0.get("failed") {
            Some(value) => truthy(value),
            None => match self.0.get("rc") {
                None => false,
                Some(Value::Number(n)) => n.as_f64() != Some(0.0),
                Some(Value::Bool(b)) => *b,
                Some(Value::String(s)) => s != "0",
                Some(_) => true,
            },
        }
    }

    /// Makes a result that counts as failed say `failed: true`, which is what the reference
    /// registers for one: measured, `failed: "yes"` and `failed: 1` both register `True`, while a
    /// falsy `failed` (`0`, `null`) is kept as the module wrote it.
    pub fn settle_failed(&mut self) {
        let failed = self.failed();
        if failed || !self.0.contains_key("failed") {
            self.0.insert("failed".into(), Value::Bool(failed));
        }
    }

    pub fn failed_with(msg: impl Into<String>) -> Self {
        let mut map = Map::new();
        map.insert("failed".into(), Value::Bool(true));
        map.insert("msg".into(), Value::String(msg.into()));
        Self(map)
    }

    /// A task that ran out of its `timeout:` keyword, on the agent or on the controller.
    /// Measured on ansible-core 2.19.12, for `command` and for `pause` alike: no `cmd`, no
    /// `rc`/`stdout`/`stderr` (the reference drops those too, even when the command had already
    /// produced output), and no `timedout.frame` (an Ansible-internal traceback hint there is
    /// nothing here to reproduce).
    pub fn timed_out(seconds: u64) -> Self {
        let mut map = Map::new();
        map.insert("changed".into(), Value::Bool(false));
        map.insert("failed".into(), Value::Bool(true));
        map.insert(
            "msg".into(),
            Value::String(format!("Task failed: Timed out after {seconds} second(s).")),
        );
        // `frame` is the reference's own, measured on a module and a controller-side action
        // alike: there is no Python frame to point at here either.
        let mut timedout = Map::new();
        timedout.insert(
            "frame".into(),
            Value::String(
                "Configure `DISPLAY_TRACEBACK` to see a traceback on timeout errors.".into(),
            ),
        );
        timedout.insert("period".into(), Value::from(seconds));
        map.insert("timedout".into(), Value::Object(timedout));
        Self(map)
    }
}

/// Python's truthiness of a JSON value, which is how the reference reads a result's `failed`.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests;
