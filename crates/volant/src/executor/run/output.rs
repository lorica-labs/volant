// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::Value;
use volant_protocol::shape::MAX_VALUES;
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

/// Whether a result carries the `_ansible_no_log` an action sets when it read what must not be
/// shown (`include_vars` of a vaulted file): every line that prints the result then prints the
/// censored body, a retry line included.
pub(in crate::executor) fn asks_to_be_hidden(result: &TaskResult) -> bool {
    result.0.get("_ansible_no_log") == Some(&Value::Bool(true))
}

/// The output a line of `stdout` or `stderr` stands for past [`MAX_VALUES`] lines. A command may
/// print millions of ordinary lines, which ansible-core splits whole; what the bound stops is a
/// frame of near-empty lines, two bytes each on the wire. At 32 bytes a line, the most a 64 MiB
/// frame may split into is about two million lines, the same order of memory as the frame's own
/// text.
const BYTES_PER_LINE: usize = 32;

/// The lines of `stdout` and `stderr` are values like any other: a line of a few bytes costs a
/// few hundred bytes of memory as one. Together they may hold [`MAX_VALUES`] lines, or one line
/// for every [`BYTES_PER_LINE`] bytes of output when that is more, and a host over that gets a
/// failed result in place of its own.
pub(super) fn add_output_lines(result: &mut TaskResult) {
    const SPLIT: [(&str, &str); 2] = [("stdout", "stdout_lines"), ("stderr", "stderr_lines")];
    let bytes: usize = SPLIT
        .iter()
        .filter(|(_, lines)| !result.0.contains_key(*lines))
        .filter_map(|(key, _)| result.0.get(*key)?.as_str())
        .map(str::len)
        .sum();
    let limit = MAX_VALUES.max(bytes / BYTES_PER_LINE);
    let mut room = limit;
    for (key, lines) in SPLIT {
        if result.0.contains_key(lines) {
            continue;
        }
        // `data.get('stdout') or ''` in the reference: a value Python reads as false splits as
        // an empty string. Any other value that is not a string is left alone.
        let split = match result.0.get(key) {
            Some(Value::String(text)) => {
                let Some(split) = splitlines(text, room) else {
                    *result = TaskResult::failed_with(format!(
                        "the output of the host is refused: {key} of {} bytes has more lines \
                         than the limit of {limit} for stdout and stderr together",
                        text.len()
                    ));
                    return;
                };
                room -= split.len();
                split.into_iter().map(Value::from).collect()
            }
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
    // The summary itself, for the error block `settle_failure` words: its message and its causes.
    if failed && let Some(summary) = sent.as_ref().filter(|s| brief_message(s).is_some()) {
        result.0.insert(CAUSE_KEY.into(), summary.clone());
    }
    if failed || sent.is_some_and(|e| !python_falsy(&e)) {
        result
            .0
            .insert("exception".into(), Value::from("(traceback unavailable)"));
    }
}

/// Where `summarise_exception` leaves a failed module's error summary for `settle_failure`.
pub(super) const CAUSE_KEY: &str = "_volant_cause";

/// `format_event_brief_message` of a serialised `ErrorSummary`: the event's message and those of
/// the causes it follows, each part ending where the next begins, as the reference joins them.
pub(super) fn brief_message(summary: &Value) -> Option<String> {
    let mut parts = Vec::new();
    let mut event = summary.get("event")?;
    loop {
        parts.push(event.get("msg")?.as_str()?.to_string());
        match followed(event) {
            Some(next) => event = next,
            None => break,
        }
    }
    Some(join_parts(parts))
}

/// The event `event` is chained to, when the chain is one the reference follows.
fn followed(event: &Value) -> Option<&Value> {
    let chain = event.get("chain")?;
    (chain.get("follow") == Some(&Value::Bool(true)))
        .then(|| chain.get("event"))
        .flatten()
}

/// Why the event is chained to its cause.
fn msg_reason(event: &Value) -> Option<&Value> {
    event.get("chain")?.get("msg_reason")
}

/// `deduplicate_message_parts`: the innermost part first, each outer part put before it unless
/// the outer one already ends with it.
fn join_parts(parts: Vec<String>) -> String {
    let mut parts = parts.into_iter().rev();
    let mut message = parts.next().unwrap_or_default();
    for part in parts {
        message = if part.ends_with(&message) {
            part
        } else {
            crate::render::concat_message(&part, &message)
        };
    }
    message
}

/// `format_event_verbose_message` of a serialised `ErrorSummary`, one text for each link the
/// reference prints behind `<<< caused by >>>`. A link whose event adds nothing is folded into
/// the one before it, as the reference does. `None` when the summary holds sub-events, which
/// the block does not show.
pub(super) fn verbose_segments(summary: &Value) -> Option<Vec<String>> {
    let text = |event: &Value, key: &str| {
        event
            .get(key)
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    let mut segments = Vec::new();
    let mut event = Some(summary.get("event")?);
    while let Some(current) = event {
        if current.get("events").is_some_and(|e| !python_falsy(e)) {
            return None;
        }
        let msg = |e: &Value| e.get("msg").and_then(Value::as_str).map(str::to_string);
        let mut parts = vec![msg(current)?];
        let mut last = current;
        let mut next = followed(current);
        while let Some(linked) = next {
            let details = |e: &Value| (text(e, "formatted_source_context"), text(e, "help_text"));
            let folds = linked.get("events").is_none_or(python_falsy)
                && (details(linked) == (None, None) || details(linked) == details(current))
                && (linked.get("chain").is_none() || msg_reason(linked) == msg_reason(last));
            if !folds {
                break;
            }
            parts.push(msg(linked)?);
            last = linked;
            next = followed(linked);
        }
        let (context, help) = (
            text(current, "formatted_source_context"),
            text(current, "help_text"),
        );
        let message = join_parts(parts);
        segments.push(match (context, help) {
            (None, None) => message,
            (None, Some(help)) if !message.contains('\n') && !help.contains('\n') => {
                format!("{message} {help}")
            }
            (context, help) => {
                let mut lines = vec![message];
                lines.extend(context);
                if let Some(help) = help {
                    lines.push(String::new());
                    lines.push(help);
                }
                lines.join("\n")
            }
        });
        event = next;
    }
    Some(segments)
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
/// empty line after a final boundary. `None` once there are more than `limit` lines.
fn splitlines(text: &str, limit: usize) -> Option<Vec<&str>> {
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
        if out.len() == limit {
            return None;
        }
        out.push(&rest[..at]);
        let width = if rest[at..].starts_with("\r\n") {
            2
        } else {
            rest[at..].chars().next().map_or(1, char::len_utf8)
        };
        rest = &rest[at + width..];
    }
    if !rest.is_empty() {
        if out.len() == limit {
            return None;
        }
        out.push(rest);
    }
    Some(out)
}

pub(in crate::executor) fn classify(
    result: &TaskResult,
    ignore_errors: bool,
    rescuable: bool,
) -> Outcome {
    classify_as(result, result.is_failed(), ignore_errors, rescuable)
}

/// [`classify`] for a result whose failure the caller judged: a loop's aggregate says `failed`
/// when an item did, and the task fails only when [`TaskResult::is_failed`] says so of an item.
pub(in crate::executor) fn classify_as(
    result: &TaskResult,
    failed: bool,
    ignore_errors: bool,
    rescuable: bool,
) -> Outcome {
    if failed {
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
