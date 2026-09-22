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
pub const PROTOCOL_VERSION: u32 = 4;

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
    /// The payload itself, base64 exactly as `modify_module` produced it. The agent verifies the
    /// hash against the decoded bytes before the blob can be used.
    PutBlob {
        hash: String,
        zip_b64: String,
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
    },
    TaskResult {
        batch: u64,
        index: usize,
        result: TaskResult,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    pub fn flag(&self, key: &str) -> bool {
        matches!(self.0.get(key), Some(Value::Bool(true)))
    }

    pub fn changed(&self) -> bool {
        self.flag("changed")
    }

    pub fn skipped(&self) -> bool {
        self.flag("skipped")
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
        let mut timedout = Map::new();
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
mod tests {
    use super::*;
    use serde_json::json;

    /// Which results fail, against ansible-core 2.19.12 on the development machine: a module
    /// printing each of these shapes (with `changed: false`) under `ignore_errors`, and the
    /// task's own line read. `fatal` for every `true` here, `ok` for every `false`.
    ///
    /// What would make this red: a string `rc` of `"2"`, a `null` rc or a `failed: "yes"` read
    /// as success - a failure reported as `ok`.
    #[test]
    fn a_result_fails_by_the_reference_s_own_rule() {
        let cases = [
            (json!({"failed": "yes"}), true),
            (json!({"failed": "false"}), true),
            (json!({"failed": 0}), false),
            (json!({"failed": 1}), true),
            (json!({"failed": null}), false),
            (json!({"failed": ""}), false),
            (json!({"failed": []}), false),
            (json!({"rc": "2"}), true),
            (json!({"rc": "0"}), false),
            (json!({"rc": null}), true),
            (json!({"rc": 0.0}), false),
            (json!({"rc": false}), false),
            (json!({"rc": true}), true),
            (json!({"rc": ""}), true),
            (json!({"rc": 1}), true),
            (json!({}), false),
            (json!({"failed": false, "rc": 1}), false),
        ];
        let wrong: Vec<String> = cases
            .into_iter()
            .filter_map(|(shape, fails)| {
                let Value::Object(map) = shape.clone() else {
                    unreachable!()
                };
                (TaskResult(map).failed() != fails).then(|| format!("{shape} fails={fails}"))
            })
            .collect();
        assert!(
            wrong.is_empty(),
            "read otherwise than the reference: {wrong:?}"
        );
    }

    #[test]
    fn hello_has_a_snake_case_type_tag() {
        let text = serde_json::to_string(&ToAgent::Hello {
            protocol: PROTOCOL_VERSION,
        })
        .unwrap();
        assert_eq!(
            text,
            format!(r#"{{"type":"hello","protocol":{PROTOCOL_VERSION}}}"#)
        );
    }

    /// A host with no Python carries no `interpreters` key at all, and a host with one carries
    /// the list in the order the agent found them.
    ///
    /// What would make this red: the field serialised when empty, which puts an empty array in
    /// every handshake of every host including the ones that never run a Python module; or the
    /// list re-ordered on the way out, which hands the controller a different best interpreter
    /// from the one the agent chose.
    #[test]
    fn ready_carries_the_interpreters_only_when_the_host_has_some() {
        let ready = |interpreters: Vec<String>| FromAgent::Ready {
            protocol: PROTOCOL_VERSION,
            version: "0.0.0".into(),
            arch: "x86_64".into(),
            interpreters,
        };
        let empty = serde_json::to_string(&ready(Vec::new())).unwrap();
        assert!(!empty.contains("interpreters"));
        assert_eq!(
            serde_json::from_str::<FromAgent>(&empty).unwrap(),
            ready(Vec::new()),
            "a handshake from a host with no Python has to parse, which is the one the empty \
             list exists for"
        );
        let text = serde_json::to_string(&ready(vec![
            "/usr/bin/python3.12".into(),
            "/usr/local/bin/python3.9".into(),
        ]))
        .unwrap();
        assert!(
            text.contains(r#""interpreters":["/usr/bin/python3.12","/usr/local/bin/python3.9"]"#),
            "{text}"
        );
        let back: FromAgent = serde_json::from_str(&text).unwrap();
        assert_eq!(
            back,
            ready(vec![
                "/usr/bin/python3.12".into(),
                "/usr/local/bin/python3.9".into()
            ])
        );
    }

    #[test]
    fn timeout_is_optional_and_absent_when_unset() {
        let task: Task = serde_json::from_str(r#"{"module":"raw"}"#).unwrap();
        assert_eq!(task.timeout, None);
        assert!(!serde_json::to_string(&task).unwrap().contains("timeout"));
        let task: Task = serde_json::from_str(r#"{"module":"raw","timeout":5}"#).unwrap();
        assert_eq!(task.timeout, Some(5));
    }

    #[test]
    fn run_batch_round_trips() {
        let mut environment = BTreeMap::new();
        environment.insert("PATH".into(), "/opt/bin".into());
        let msg = ToAgent::RunBatch {
            id: 7,
            tasks: vec![Task {
                module: "ansible.builtin.command".into(),
                args: json!({"_raw_params": "echo hi"})
                    .as_object()
                    .unwrap()
                    .clone(),
                ignore_errors: true,
                timeout: None,
                environment,
                payload: None,
            }],
        };
        let back: ToAgent = serde_json::from_slice(&serde_json::to_vec(&msg).unwrap()).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn task_args_and_ignore_errors_default() {
        let task: Task = serde_json::from_str(r#"{"module":"raw"}"#).unwrap();
        assert!(task.args.is_empty());
        assert!(!task.ignore_errors);
    }

    /// A task with nothing to add to the environment does not carry the field at all, and one
    /// that does carries it as a map of strings.
    ///
    /// What would make this red: `environment` serialised when it is empty, which would put a
    /// key on every task of every batch; or the field dropped from the shape, which is how the
    /// controller would send an environment the agent never sets.
    #[test]
    fn the_environment_is_absent_when_empty_and_round_trips_when_set() {
        let task: Task = serde_json::from_str(r#"{"module":"raw"}"#).unwrap();
        assert!(task.environment.is_empty());
        assert!(
            !serde_json::to_string(&task)
                .unwrap()
                .contains("environment")
        );
        let task: Task =
            serde_json::from_str(r#"{"module":"raw","environment":{"A":"1"}}"#).unwrap();
        assert_eq!(task.environment["A"], "1");
        assert!(serde_json::to_string(&task).unwrap().contains(r#""A":"1""#));
    }

    /// A Python task carries the reference of its payload and nothing of the payload itself, and
    /// a native task carries no payload field at all.
    ///
    /// What would make this red: `payload` serialised on every task, which puts a null on each of
    /// the thousands of native tasks a real playbook sends; or the zip travelling inside `Task`,
    /// which would re-send 631 KB once per task instead of once per run.
    #[test]
    fn a_python_task_carries_a_payload_reference_and_a_native_one_carries_none() {
        let native: Task = serde_json::from_str(r#"{"module":"command"}"#).unwrap();
        assert_eq!(native.payload, None);
        assert!(!serde_json::to_string(&native).unwrap().contains("payload"));

        let task: Task = serde_json::from_str(
            r#"{"module":"ping","payload":{"blob":"abc123","module_fqn":"ansible.modules.ping",
            "profile":"legacy","rlimit_nofile":0,"interpreter":"/usr/bin/python3.12"}}"#,
        )
        .unwrap();
        let payload = task.payload.expect("a python task carries one");
        assert_eq!(payload.blob, "abc123");
        assert_eq!(payload.module_fqn, "ansible.modules.ping");
        assert_eq!(payload.profile, "legacy");
        assert_eq!(payload.rlimit_nofile, 0);
        assert!(payload.extensions.is_empty());
        assert_eq!(payload.interpreter, "/usr/bin/python3.12");
    }

    /// The three blob messages round-trip under the snake_case tag every other message uses.
    #[test]
    fn the_blob_messages_round_trip() {
        let put = ToAgent::PutBlob {
            hash: "ff".into(),
            zip_b64: "UEsDBA==".into(),
        };
        let back: ToAgent = serde_json::from_slice(&serde_json::to_vec(&put).unwrap()).unwrap();
        assert_eq!(back, put);
        assert!(
            serde_json::to_string(&put)
                .unwrap()
                .starts_with(r#"{"type":"put_blob""#)
        );

        let has = ToAgent::HasBlob { hash: "ff".into() };
        let back: ToAgent = serde_json::from_slice(&serde_json::to_vec(&has).unwrap()).unwrap();
        assert_eq!(back, has);

        let state = FromAgent::BlobState {
            hash: "ff".into(),
            present: true,
        };
        let back: FromAgent = serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(back, state);
    }

    /// The version moves with the shape. An agent that does not know `payload` would run a Python
    /// task as an unknown module and report it failed, which is the honest failure; an agent that
    /// does not know `put_blob` would discard the frame and then fail every task of the batch.
    #[test]
    fn the_protocol_version_is_four() {
        assert_eq!(PROTOCOL_VERSION, 4);
    }

    #[test]
    fn a_non_zero_rc_counts_as_failed() {
        let ok = TaskResult(
            json!({"rc": 0, "changed": true})
                .as_object()
                .unwrap()
                .clone(),
        );
        let bad = TaskResult(json!({"rc": 2}).as_object().unwrap().clone());
        let flagged = TaskResult(json!({"failed": true}).as_object().unwrap().clone());
        assert!(!ok.failed() && ok.changed());
        assert!(bad.failed() && !bad.changed());
        assert!(flagged.failed());
    }

    #[test]
    fn an_explicit_failed_false_rescues_a_non_zero_rc() {
        let rescued = TaskResult(
            json!({"failed": false, "rc": 2})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert!(!rescued.failed());
    }

    #[test]
    fn batch_outcome_carries_the_index() {
        let text = serde_json::to_string(&FromAgent::BatchDone {
            batch: 1,
            outcome: BatchOutcome::Failed { at: 3 },
        })
        .unwrap();
        assert_eq!(
            text,
            r#"{"type":"batch_done","batch":1,"outcome":{"kind":"failed","at":3}}"#
        );
    }

    #[test]
    fn failed_with_builds_the_ansible_shape() {
        let r = TaskResult::failed_with("boom");
        assert!(r.failed());
        assert_eq!(r.0["msg"], "boom");
    }
}
