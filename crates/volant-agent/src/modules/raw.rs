// SPDX-License-Identifier: GPL-3.0-or-later
//! `raw`: a command line handed to the shell with nothing around it. Ansible uses it on hosts
//! that have no Python; the result carries only rc, stdout and stderr.

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;
use volant_protocol::modules::RAW;

use super::{Context, Module, Run, command};

pub const MODULE: Module = Module {
    spec: &RAW,
    run: run_raw,
};

fn run_raw(args: &Map<String, Value>, ctx: &Context, cancelled: &dyn Fn() -> bool) -> Run {
    // The reference's `raw` action skips in check mode without reaching the host.
    if ctx.check_mode {
        let mut skipped = Map::new();
        skipped.insert("changed".into(), json!(false));
        skipped.insert("skipped".into(), json!(true));
        return Run::Done(TaskResult(skipped));
    }
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
    // Run under a terminal when the controller says the connection would have given one.
    if let Some(tty) = args.get("_volant_tty") {
        inner.insert("_volant_tty".into(), tty.clone());
    }
    if line.is_empty() {
        return Run::Done(TaskResult::failed_with("no command given"));
    }
    match command::execute(&inner, false, ctx, cancelled) {
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
mod tests;
