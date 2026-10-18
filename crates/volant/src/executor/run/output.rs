// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::Value;
use volant_protocol::{Task, TaskResult};

use crate::stats::Outcome;

/// The argument that asks the agent to run a command on a terminal. Internal: the controller sets
/// it, a playbook cannot, and the agent never reports it.
pub(super) const TTY_KEY: &str = "_volant_tty";

/// Settles which tasks of a batch run on a terminal. A task marks itself with `true` when the
/// reference would run it through `ssh -tt`; the marker becomes the line that ends its stderr when
/// this link gives commands a terminal, and is dropped on any other link. Any other value is not
/// the controller's, and is dropped.
pub(super) fn give_terminals(tasks: &mut [Task], close: Option<String>) {
    for task in tasks {
        if let Some(marked) = task.args.remove(TTY_KEY)
            && marked == Value::Bool(true)
            && let Some(close) = &close
        {
            task.args
                .insert(TTY_KEY.into(), Value::String(close.clone()));
        }
    }
}

/// The key the Python helper sets on a result it built from `AnsibleActionSkip`.
pub(crate) const RAISED_SKIP: &str = "_volant_raised_skip";

/// `stdout_lines` and `stderr_lines`, for a result that carries `stdout` or `stderr` and not the
/// list beside it. The reference's `_execute_module` adds both to every
/// module result that lacks them, so a Python module's result has them there; here the agent
/// writes them for `command` and `raw` alone, and every result an agent sends passes through
/// [`super::batch::run_agent_batch`], plugin sub-tasks included.
/// A result a host sent, without the keys only the controller may set: every `_volant_` key,
/// which the renderer reads (an error chain names files on the controller), and every
/// `_ansible_` key but `_ansible_parsed`, as the reference's `remove_internal_keys` drops them
/// from a module's return. Measured on ansible-core 2.19.12: a module returning
/// `_ansible_verbose_always` gets a warning and its result printed on one line.
pub(crate) fn drop_internal_keys(result: &mut TaskResult) {
    result.0.retain(|key, _| {
        !key.starts_with("_volant_") && (!key.starts_with("_ansible_") || key == "_ansible_parsed")
    });
}

pub(super) fn add_output_lines(result: &mut TaskResult) {
    for (key, lines) in [("stdout", "stdout_lines"), ("stderr", "stderr_lines")] {
        if result.0.contains_key(lines) {
            continue;
        }
        // `data.get('stdout') or ''` in the reference: a value Python reads as false splits as
        // an empty string. Any other value that is not a string is left alone.
        let split = match result.0.get(key) {
            Some(Value::String(text)) => splitlines(text).into_iter().map(Value::from).collect(),
            Some(value) if python_falsy(value) => Vec::new(),
            _ => continue,
        };
        result.0.insert(lines.to_string(), Value::Array(split));
    }
}

/// The `exception` of a module's result, as the reference's controller leaves it. Its
/// `_parse_returned_data` turns whatever a module sent - an error summary, a traceback string or
/// nothing - into an error summary whenever the result failed, and drops a falsy one otherwise;
/// `error_summary` renders that summary as `(traceback unavailable)` while tracebacks are off,
/// which is the default. Measured on ansible-core 2.19.12: a failed `copy`, `lineinfile` and
/// `command` each register that string, and a `stat` that succeeds registers no `exception`. So
/// does a failed `raw`, which never goes through `_parse_returned_data`: `TaskExecutor._execute`
/// normalises every task's final result the same way (`maybe_raise_on_result`).
pub(super) fn summarise_exception(result: &mut TaskResult) {
    let sent = result.0.remove("exception");
    let failed = result.0.get("failed").is_some_and(|v| !python_falsy(v));
    // What the summary says, causes and all, for the error block `settle_failure` words.
    if failed && let Some(brief) = sent.as_ref().and_then(brief_message) {
        result.0.insert(CAUSE_KEY.into(), Value::from(brief));
    }
    if failed || sent.is_some_and(|e| !python_falsy(&e)) {
        result
            .0
            .insert("exception".into(), Value::from("(traceback unavailable)"));
    }
}

/// Where `summarise_exception` leaves a failed module's own brief message for `settle_failure`.
pub(super) const CAUSE_KEY: &str = "_volant_cause";

/// `format_event_brief_message` of a serialised `ErrorSummary`: the event's message and those of
/// the causes it follows, each part ending where the next begins, as the reference joins them.
fn brief_message(summary: &Value) -> Option<String> {
    let mut parts = Vec::new();
    let mut event = summary.get("event")?;
    loop {
        parts.push(event.get("msg")?.as_str()?.to_string());
        match event.get("chain") {
            Some(chain) if chain.get("follow") == Some(&Value::Bool(true)) => {
                event = chain.get("event")?;
            }
            _ => break,
        }
    }
    let mut parts = parts.into_iter().rev();
    let mut message = parts.next()?;
    for part in parts {
        message = if part.ends_with(&message) {
            part
        } else {
            crate::render::concat_message(&part, &message)
        };
    }
    Some(message)
}

/// Whether Python's `bool()` is false for the value a module sent: `None`, `False`, a zero, or
/// an empty string, list or dict.
pub(super) fn python_falsy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => true,
        Value::Number(n) => matches!(n.to_string().as_str(), "0" | "0.0" | "-0.0"),
        Value::String(s) => s.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

/// Python's `str.splitlines()`: every line boundary Python knows, `\r\n` counted as one, and no
/// empty line after a final boundary.
fn splitlines(text: &str) -> Vec<&str> {
    let boundary = |c: char| {
        matches!(
            c,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        )
    };
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(boundary) {
        out.push(&rest[..at]);
        let width = if rest[at..].starts_with("\r\n") {
            2
        } else {
            rest[at..].chars().next().map_or(1, char::len_utf8)
        };
        rest = &rest[at + width..];
    }
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

pub(in crate::executor) fn classify(
    result: &TaskResult,
    ignore_errors: bool,
    rescuable: bool,
) -> Outcome {
    if result.failed() {
        if ignore_errors {
            Outcome::Ignored
        } else if rescuable {
            Outcome::Rescued
        } else {
            Outcome::Failed
        }
    } else if result.skipped() {
        Outcome::Skipped
    } else if result.changed() {
        Outcome::Changed
    } else {
        Outcome::Ok
    }
}
