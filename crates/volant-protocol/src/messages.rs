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
pub const PROTOCOL_VERSION: u32 = 5;

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
        /// A file one task stages rather than a payload the link reuses. The agent keeps it out
        /// of its shared cache, in a directory of this connection's own that goes when the
        /// connection does, and `HasBlob` never answers for it: two links to one host account
        /// share the cache, and one consuming a file the other was told was there failed the
        /// other's task.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        staged: bool,
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

    /// Which results changed, against ansible-core 2.19.12 on the development machine: a module
    /// printing each of these shapes, and the task's own line read - `changed:` for every
    /// `true` here, `ok:` for every `false`, and a recap of `changed=3`.
    ///
    /// What would make this red: `changed: "yes"` or `changed: 1` reported as `ok`, which the
    /// recap and every `notify` then miss.
    #[test]
    fn a_result_changed_by_the_reference_s_own_rule() {
        let cases = [
            (json!({"changed": "yes"}), true),
            (json!({"changed": "false"}), true),
            (json!({"changed": 1}), true),
            (json!({"changed": 0}), false),
            (json!({"changed": null}), false),
            (json!({"changed": ""}), false),
            (json!({"changed": []}), false),
            (json!({}), false),
        ];
        let wrong: Vec<String> = cases
            .into_iter()
            .filter_map(|(shape, changed)| {
                let Value::Object(map) = shape.clone() else {
                    unreachable!()
                };
                (TaskResult(map).changed() != changed).then(|| format!("{shape} changed={changed}"))
            })
            .collect();
        assert!(
            wrong.is_empty(),
            "read otherwise than the reference: {wrong:?}"
        );
    }

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

    /// Additive in both directions, like `Ready.interpreters`: a controller that does not know
    /// `ran` or `natives` reads today's frames, and an agent that does not know `force_python`
    /// runs the payload, which is what the flag asks for anyway.
    ///
    /// What would make this red: one of the three fields serialised when empty, which puts a
    /// key on every task and every result; or one of them required on the way in, which makes a
    /// frame from the other side's older build unreadable.
    #[test]
    fn the_native_fields_are_absent_on_the_wire_when_empty_and_default_on_the_way_in() {
        let old = r#"{"type":"task_result","batch":1,"index":0,"result":{"changed":false}}"#;
        let FromAgent::TaskResult { ran, .. } = serde_json::from_str(old).unwrap() else {
            panic!("a task result")
        };
        assert_eq!(ran, None);
        let old = r#"{"type":"ready","protocol":5,"version":"0.0.0","arch":"x86_64"}"#;
        let FromAgent::Ready { natives, .. } = serde_json::from_str(old).unwrap() else {
            panic!("a ready")
        };
        assert!(natives.is_empty());
        let task: Task = serde_json::from_str(r#"{"module":"stat"}"#).unwrap();
        assert!(!task.force_python);
        let task = Task {
            module: "stat".into(),
            ..Default::default()
        };
        assert!(
            !serde_json::to_string(&task)
                .unwrap()
                .contains("force_python")
        );
        let forced = Task {
            force_python: true,
            ..task
        };
        assert!(
            serde_json::to_string(&forced)
                .unwrap()
                .contains(r#""force_python":true"#)
        );
        let ran = Ran {
            path: ExecPath::Fallback,
            reason: Some("not implemented".into()),
            micros: 12,
            fork_micros: None,
            import_micros: None,
            module_micros: None,
        };
        let text = serde_json::to_string(&ran).unwrap();
        assert_eq!(
            text,
            r#"{"path":"fallback","reason":"not implemented","micros":12}"#
        );
        assert_eq!(serde_json::from_str::<Ran>(&text).unwrap(), ran);
        assert_eq!(
            PROTOCOL_VERSION, 5,
            "additive fields do not move the version"
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
            natives: Vec::new(),
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
                files: Vec::new(),
                force_python: false,
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
            staged: false,
        };
        let back: ToAgent = serde_json::from_slice(&serde_json::to_vec(&put).unwrap()).unwrap();
        assert_eq!(back, put);
        assert_eq!(
            serde_json::to_string(&put).unwrap(),
            r#"{"type":"put_blob","hash":"ff","zip_b64":"UEsDBA=="}"#,
            "a payload carries no staged field"
        );
        let file = ToAgent::PutBlob {
            hash: "ff".into(),
            zip_b64: "UEsDBA==".into(),
            staged: true,
        };
        let back: ToAgent = serde_json::from_slice(&serde_json::to_vec(&file).unwrap()).unwrap();
        assert_eq!(back, file);

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

    /// A task that stages a file names the blob and the argument, and a task that stages none
    /// carries no `files` field at all.
    ///
    /// What would make this red: `files` serialised on every task, which puts an empty list on
    /// the thousands of native tasks a playbook sends; or the bytes travelling inside `Task`,
    /// which would bypass the hash the agent checks before it lets a blob be used.
    #[test]
    fn a_task_names_the_files_it_stages_and_nothing_else() {
        let bare: Task = serde_json::from_str(r#"{"module":"command"}"#).unwrap();
        assert!(bare.files.is_empty());
        assert!(!serde_json::to_string(&bare).unwrap().contains("files"));
        let task: Task =
            serde_json::from_str(r#"{"module":"copy","files":[{"arg":"src","blob":"ab"}]}"#)
                .unwrap();
        assert_eq!(
            task.files,
            vec![StagedFile {
                arg: "src".into(),
                blob: "ab".into()
            }]
        );
    }

    /// `skipped` reads by truthiness, as `changed` and `failed` do since PR 181. Measured on
    /// ansible-core 2.19.12 when this was filed: `skipped: "yes"` shows `skipping:`.
    #[test]
    fn skipped_is_read_by_truthiness() {
        let yes: TaskResult = serde_json::from_str(r#"{"skipped":"yes"}"#).unwrap();
        assert!(yes.skipped());
        let zero: TaskResult = serde_json::from_str(r#"{"skipped":0}"#).unwrap();
        assert!(!zero.skipped());
    }

    /// The version moves with the shape. An agent that does not know `files` would run `copy`
    /// with no source; an agent that does not know `payload` would run a Python task as an
    /// unknown module; an agent that does not know `put_blob` would discard the frame and then
    /// fail every task of the batch.
    #[test]
    fn the_protocol_version_is_five() {
        assert_eq!(PROTOCOL_VERSION, 5);
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
