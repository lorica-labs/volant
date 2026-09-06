// SPDX-License-Identifier: GPL-3.0-or-later
//! Messages exchanged over frames. Field names are part of the protocol.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Bumped when a message changes shape. Controller and agent refuse to talk across versions.
pub const PROTOCOL_VERSION: u32 = 2;

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
}

/// Agent to controller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromAgent {
    Ready {
        protocol: u32,
        version: String,
        arch: String,
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

    /// Ansible's rule: `failed` set, or an `rc` present and different from zero.
    pub fn failed(&self) -> bool {
        self.flag("failed")
            || matches!(self.0.get("rc"), Some(Value::Number(n)) if n.as_i64() != Some(0))
    }

    pub fn failed_with(msg: impl Into<String>) -> Self {
        let mut map = Map::new();
        map.insert("failed".into(), Value::Bool(true));
        map.insert("msg".into(), Value::String(msg.into()));
        Self(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
