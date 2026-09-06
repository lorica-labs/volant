// SPDX-License-Identifier: GPL-3.0-or-later
//! `raw`: a command line handed to the shell with nothing around it. Ansible uses it on hosts
//! that have no Python; the result carries only rc, stdout and stderr.

use std::time::Duration;

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;
use volant_protocol::modules::RAW;

use super::{Module, Run, command};

pub const MODULE: Module = Module {
    spec: &RAW,
    run: run_raw,
};

fn run_raw(
    args: &Map<String, Value>,
    timeout: Option<Duration>,
    cancelled: &dyn Fn() -> bool,
) -> Run {
    let line = args
        .get("_raw_params")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let executable = args
        .get("executable")
        .and_then(Value::as_str)
        .unwrap_or("sh");
    let mut inner = Map::new();
    inner.insert("argv".into(), json!([executable, "-c", line]));
    // Ansible's `raw` returns the output untouched, where `command` trims trailing newlines.
    inner.insert("strip_empty_ends".into(), json!(false));
    if line.is_empty() {
        return Run::Done(TaskResult::failed_with("no command given"));
    }
    match command::execute(&inner, false, timeout, cancelled) {
        Run::Done(TaskResult(mut map)) => {
            for key in ["cmd", "start", "end", "delta", "msg"] {
                map.remove(key);
            }
            Run::Done(TaskResult(map))
        }
        Run::Cancelled => Run::Cancelled,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn raw_reports_only_what_ansible_reports() {
        let args: Map<String, Value> = json!({"_raw_params": "echo $((2 + 2))"})
            .as_object()
            .unwrap()
            .clone();
        let Run::Done(r) = run_raw(&args, None, &|| false) else {
            panic!("cancelled")
        };
        // Ansible keeps the trailing newline for `raw` where `command` strips it.
        assert_eq!(r.0["stdout"], "4\n");
        assert_eq!(r.0["stdout_lines"], json!(["4"]));
        assert_eq!(r.0["rc"], 0);
        assert!(r.changed());
        for absent in ["cmd", "start", "end", "delta", "msg"] {
            assert!(
                !r.0.contains_key(absent),
                "{absent} must not be reported by raw"
            );
        }
    }

    #[test]
    fn raw_failure_is_carried_by_rc_alone() {
        let args: Map<String, Value> = json!({"_raw_params": "exit 3"})
            .as_object()
            .unwrap()
            .clone();
        let Run::Done(r) = run_raw(&args, None, &|| false) else {
            panic!("cancelled")
        };
        assert_eq!(r.0["rc"], 3);
        assert!(r.failed());
    }

    #[test]
    fn executable_selects_the_shell() {
        let args: Map<String, Value> = json!({"_raw_params": "echo $0", "executable": "/bin/sh"})
            .as_object()
            .unwrap()
            .clone();
        let Run::Done(r) = run_raw(&args, None, &|| false) else {
            panic!("cancelled")
        };
        assert_eq!(r.0["stdout"], "/bin/sh\n");
    }
}
