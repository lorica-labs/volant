// SPDX-License-Identifier: GPL-3.0-or-later
//! `command`, `shell` and `raw`: run a program and report rc, stdout and stderr like Ansible.

use std::path::Path;

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;
use volant_protocol::modules::{COMMAND, arg_bool};

use super::{Context, Module, Run};
#[cfg(unix)]
use crate::natives::common::{clean_args, popen_error};

pub const MODULE: Module = Module {
    spec: &COMMAND,
    run: run_command,
};

fn run_command(args: &Map<String, Value>, ctx: &Context, cancelled: &dyn Fn() -> bool) -> Run {
    execute(args, false, ctx, cancelled)
}

mod execute;
mod expand;
mod spawn;

pub(crate) use execute::execute;
pub(crate) use spawn::{CANCEL_POLL, kill_group};

fn lines(text: &str) -> Value {
    if text.is_empty() {
        return json!([]);
    }
    json!(text.lines().collect::<Vec<_>>())
}

/// A guard that stopped the command. The reference leaves the task `ok`, never `skipped`, and
/// `start`, `end` and `delta` null because nothing ran.
fn not_run(cmd: Value, msg: String, stdout: String) -> TaskResult {
    let mut result = Map::new();
    result.insert("cmd".into(), cmd);
    for key in ["start", "end", "delta"] {
        result.insert(key.into(), Value::Null);
    }
    result.insert("rc".into(), json!(0));
    result.insert("stdout".into(), json!(stdout));
    result.insert("stderr".into(), json!(""));
    result.insert("stdout_lines".into(), lines(&stdout));
    result.insert("stderr_lines".into(), json!([]));
    result.insert("changed".into(), json!(false));
    result.insert("msg".into(), json!(msg));
    TaskResult(result)
}

fn spawn_failure(cmd: Value, chdir: Option<&str>, err: &std::io::Error) -> TaskResult {
    let (rc, msg) = match err.kind() {
        std::io::ErrorKind::NotFound => {
            let missing = match chdir {
                Some(dir) if !Path::new(dir).exists() => format!("b'{dir}'"),
                _ => program_name(&cmd),
            };
            (2, format!("[Errno 2] No such file or directory: {missing}"))
        }
        _ => (err.raw_os_error().unwrap_or(1), err.to_string()),
    };
    let mut result = Map::new();
    result.insert("cmd".into(), cmd);
    result.insert("rc".into(), json!(rc));
    result.insert("msg".into(), json!(msg));
    result.insert("failed".into(), json!(true));
    TaskResult(result)
}

/// A boolean argument as the reference reads it: unset (or null) is `default`, and a value it
/// cannot convert fails the task with its own sentence. The list of valid spellings is a Python
/// set, so the reference prints it in an order that changes from run to run; this one is fixed.
fn bool_arg(args: &Map<String, Value>, name: &str, default: bool) -> Result<bool, TaskResult> {
    let value = match args.get(name) {
        None | Some(Value::Null) => return Ok(default),
        Some(value) => value,
    };
    if let Some(flag) = arg_bool(value) {
        return Ok(flag);
    }
    let (kind, why) = match value {
        Value::String(text) => ("str", text.clone()),
        Value::Number(n) => (if n.is_f64() { "float" } else { "int" }, n.to_string()),
        Value::Array(_) => ("list", String::new()),
        _ => ("dict", String::new()),
    };
    let why = if matches!(value, Value::Array(_) | Value::Object(_)) {
        format!("<class '{kind}'> cannot be converted to a bool")
    } else {
        format!(
            "The value '{why}' is not a valid boolean. Valid booleans include: 'n', 1, '0', 0, 'true', 't', 'off', 'y', 'on', 'f', 'no', 'false', 'yes', '1'"
        )
    };
    let mut result = TaskResult::failed_with(format!(
        "argument '{name}' is of type {kind} and we were unable to convert to bool: {why}"
    ));
    result.0.insert("changed".into(), json!(false));
    Err(result)
}

/// A `chdir` that cannot be entered, as the reference reports it (measured on 2.19.12): the words as
/// written, `rc` and the clock null, nothing run, and the errno in the `exception`.
#[cfg(unix)]
fn chdir_failure(cmd: Value, dir: &str, err: &std::io::Error) -> TaskResult {
    let errno = err.raw_os_error().unwrap_or(libc::EIO);
    let msg = "Unable to change directory before execution.";
    let mut exception = popen_error(errno, dir);
    exception["event"]["msg"] = json!(msg);
    let mut result = Map::new();
    result.insert("cmd".into(), cmd);
    for key in ["rc", "start", "end", "delta"] {
        result.insert(key.into(), Value::Null);
    }
    for key in ["stdout", "stderr"] {
        result.insert(key.into(), json!(""));
        result.insert(format!("{key}_lines"), json!([]));
    }
    result.insert("changed".into(), json!(false));
    result.insert("msg".into(), json!(msg));
    result.insert("failed".into(), json!(true));
    result.insert("exception".into(), exception);
    TaskResult(result)
}

#[cfg(not(unix))]
fn chdir_failure(cmd: Value, dir: &str, err: &std::io::Error) -> TaskResult {
    spawn_failure(cmd, Some(dir), err)
}

/// A program that would not start, as the reference reports `Popen` raising: `rc` is the errno,
/// `cmd` is the words after expansion, quoted and joined into one string, and the cause is in the
/// `exception`. A shell line is the `executable -c line` that was started.
#[cfg(unix)]
fn exec_failure(argv: &[String], err: &std::io::Error) -> TaskResult {
    let errno = err.raw_os_error().unwrap_or(libc::EIO);
    let words: Vec<&str> = argv.iter().map(String::as_str).collect();
    let mut result = Map::new();
    result.insert("cmd".into(), json!(clean_args(&words)));
    result.insert("rc".into(), json!(errno));
    result.insert("msg".into(), json!("Error executing command."));
    for key in ["stdout", "stderr"] {
        result.insert(key.into(), json!(""));
        result.insert(format!("{key}_lines"), json!([]));
    }
    result.insert("changed".into(), json!(false));
    result.insert("failed".into(), json!(true));
    result.insert("exception".into(), popen_error(errno, &argv[0]));
    TaskResult(result)
}

fn program_name(cmd: &Value) -> String {
    match cmd {
        Value::Array(list) => list
            .first()
            .and_then(Value::as_str)
            .map(|s| format!("b'{s}'"))
            .unwrap_or_default(),
        Value::String(s) => format!("b'{s}'"),
        _ => String::new(),
    }
}

#[cfg(all(test, unix))]
mod tests;
