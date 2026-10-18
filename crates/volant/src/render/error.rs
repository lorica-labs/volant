// SPDX-License-Identifier: GPL-3.0-or-later
//! The `[ERROR]:` and `fatal:` blocks: the error's cause chain, its message and the source excerpt.

use std::io::Write;

use volant_protocol::TaskResult;

use super::{CENSORED, FAILED, Renderer, UNREACHABLE};

use super::recap_json::{ansible_json, ansible_json_indented};

/// What the reference's callback prints in front of a failure's line: `[ERROR]: ` and the
/// failure's message, then, when the task's file is known, `Origin: <file>:<line>:<col>` and the
/// task's lines with a column marker (`_error_utils.SourceContext`, ansible-core 2.19.12).
///
/// A failure with causes prints them as the reference chains them: the message, a blank line,
/// `Task failed.` with the task's origin, then each cause behind `<<< caused by >>>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorBlock {
    pub message: String,
    pub origin: Option<crate::playbook::Origin>,
    /// What the reference shows under `Origin: <unknown>` for a task no file holds: its data,
    /// shortened (`textwrap.shorten(str(value), 120)`).
    pub unknown: Option<String>,
    pub causes: Vec<Cause>,
}

/// One link of a failure's chain: its own message, where the value it is about was written, and
/// the help the reference adds to some.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cause {
    pub msg: String,
    pub origin: Option<crate::playbook::Origin>,
    pub help: Option<String>,
}

/// The key the executor leaves a failure's causes under, as `[{"msg", "path", "line", "col",
/// "help"}]`. Taken off with [`ERROR_KEY`].
pub const CHAIN_KEY: &str = "_volant_chain";

/// One cause as [`CHAIN_KEY`] holds it.
pub fn cause(
    msg: &str,
    origin: Option<&crate::playbook::Origin>,
    help: Option<&str>,
) -> serde_json::Value {
    let mut c = serde_json::Map::new();
    c.insert("msg".into(), msg.into());
    if let Some(o) = origin {
        c.insert("path".into(), o.path.to_string().into());
        c.insert("line".into(), o.line.into());
        c.insert("col".into(), o.col.into());
    }
    if let Some(help) = help {
        c.insert("help".into(), help.into());
    }
    serde_json::Value::Object(c)
}

pub(super) fn causes_of(result: &TaskResult) -> Vec<Cause> {
    let Some(serde_json::Value::Array(list)) = result.0.get(CHAIN_KEY) else {
        return Vec::new();
    };
    let text = |c: &serde_json::Value, key: &str| {
        c.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let number = |c: &serde_json::Value, key: &str| {
        c.get(key)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
    };
    list.iter()
        .map(|c| Cause {
            msg: text(c, "msg").unwrap_or_default(),
            origin: match (text(c, "path"), number(c, "line"), number(c, "col")) {
                (Some(path), Some(line), Some(col)) => Some(crate::playbook::Origin {
                    path: path.into(),
                    line,
                    col,
                }),
                _ => None,
            },
            help: text(c, "help"),
        })
        .collect()
}

impl ErrorBlock {
    /// The block of a failed result: the message the executor settled on, or `Task failed: ` and
    /// the result's `msg` for a failure that never went through it.
    pub fn of(
        result: &TaskResult,
        origin: Option<&crate::playbook::Origin>,
        censored: bool,
    ) -> Self {
        let message = result
            .0
            .get(ERROR_KEY)
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| task_failed(&message_of(result)), str::to_string);
        Self {
            message: if censored { censor(&message) } else { message },
            origin: origin.cloned(),
            unknown: None,
            // A `no_log` task's causes would repeat what its censored message no longer says.
            causes: if censored {
                Vec::new()
            } else {
                causes_of(result)
            },
        }
    }

    /// The block of a host the task could not reach: its message is the reason, which the
    /// reference words `Task failed: ...` too.
    pub fn unreachable(
        msg: &str,
        origin: Option<&crate::playbook::Origin>,
        censored: bool,
    ) -> Self {
        let message = task_failed(msg);
        Self {
            message: if censored { censor(&message) } else { message },
            origin: origin.cloned(),
            unknown: None,
            causes: Vec::new(),
        }
    }
}

/// What the reference writes in place of a `no_log` task's error detail, measured on an
/// unreachable host: `Failed to connect to the host via ssh: <error censored due to no log>`.
pub const CENSORED_ERROR: &str = "<error censored due to no log>";

/// A `no_log` task's error message with everything but its fixed prefixes replaced by
/// `CENSORED_ERROR`. The reference censors only an unreachable host's detail and prints a
/// failed task's message as it is, the secret a `fail: msg=` carries included (ansible-core
/// 2.19.12); Volant censors both.
pub(super) fn censor(message: &str) -> String {
    const PREFIXES: [&str; 4] = [
        "Task failed: ",
        "Module failed: ",
        "Action failed: ",
        "Failed to connect to the host via ssh: ",
    ];
    let mut kept = String::new();
    let mut rest = message;
    while let Some(prefix) = PREFIXES.iter().find(|p| rest.starts_with(**p)) {
        kept.push_str(prefix);
        rest = &rest[prefix.len()..];
    }
    format!("{kept}{CENSORED_ERROR}")
}

/// The key the executor leaves the error block's message under on a failed result. Taken off
/// before the result is shown, registered or handed to a `rescue`.
pub const ERROR_KEY: &str = "_volant_error";

pub(super) fn message_of(result: &TaskResult) -> String {
    match result.0.get("msg") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => s.clone(),
        Some(serde_json::Value::String(_)) | None => "Unknown error.".to_string(),
        Some(other) => ansible_json(other),
    }
}

/// `Task failed: <msg>`, unless `msg` already says it: `_text_utils.concat_message`.
pub fn task_failed(msg: &str) -> String {
    if msg.starts_with("Task failed: ") {
        msg.to_string()
    } else {
        concat_message("Task failed.", msg)
    }
}

/// `left` without its trailing `.` and spaces, `: `, then `right`: `_text_utils.concat_message`.
pub fn concat_message(left: &str, right: &str) -> String {
    format!("{}: {right}", left.trim_end_matches(['.', ' ']))
}

/// Python's `textwrap.shorten(text, width)`: whitespace collapsed, then as many words as fit with
/// ` [...]` behind them when the whole does not.
// Words split on whitespace only, where `TextWrapper` also breaks after a hyphen;
// split those too if a quoted value with hyphens ever needs it.
pub fn shorten(text: &str, width: usize) -> String {
    const PLACEHOLDER: &str = " [...]";
    let words: Vec<&str> = text.split_whitespace().collect();
    let whole = words.join(" ");
    if whole.chars().count() <= width {
        return whole;
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut len = 0;
    for word in &words {
        let add = word.chars().count() + usize::from(!kept.is_empty());
        if len + add > width {
            break;
        }
        kept.push(word);
        len += add;
    }
    while let Some(last) = kept.last() {
        if len + PLACEHOLDER.len() <= width {
            return format!("{}{PLACEHOLDER}", kept.join(" "));
        }
        len -= last.chars().count() + usize::from(kept.len() > 1);
        kept.pop();
    }
    PLACEHOLDER.trim_start().to_string()
}

/// `Origin: ...`, a blank line, then the task's line and the two before it, numbered, with a
/// marker under the column: `SourceContext.from_origin` and its `__str__`.
pub(crate) fn source_context(origin: &crate::playbook::Origin) -> String {
    let mut out = format!("Origin: {}:{}:{}", origin.path, origin.line, origin.col);
    let annotated = annotated_lines(origin);
    if !annotated.is_empty() {
        out.push_str("\n\n");
        out.push_str(&annotated.join("\n"));
    }
    out
}

pub(super) fn annotated_lines(origin: &crate::playbook::Origin) -> Vec<String> {
    const CONTEXT: usize = 2;
    const WIDTH: usize = 120;
    const MARK: &str = "...";
    let target = origin.line;
    if target < 1 {
        return Vec::new();
    }
    let start = (target - 1).saturating_sub(CONTEXT);
    // The name of what Python's `open()` and read raise, which the reference prints.
    let text = match std::fs::read_to_string(&*origin.path) {
        Ok(text) => text,
        Err(e) => {
            let name = match e.kind() {
                std::io::ErrorKind::NotFound => "FileNotFoundError",
                std::io::ErrorKind::PermissionDenied => "PermissionError",
                std::io::ErrorKind::InvalidData => "UnicodeDecodeError",
                std::io::ErrorKind::IsADirectory => "IsADirectoryError",
                _ => "OSError",
            };
            return vec![format!("(source not shown: {name})")];
        }
    };
    // Read in text mode there: `\r\n` and a lone `\r` both end a line.
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = text
        .split_inclusive('\n')
        .skip(start)
        .take(target - start)
        .collect();
    if lines.len() != target - start {
        return vec!["(source not shown: file truncated)".to_string()];
    }
    let label = target.to_string().len();
    let max_src = WIDTH - label - 1;
    let mut usable = max_src;
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let mut line = line.trim_end_matches('\n').replace('\t', " ");
        if line.chars().count() > max_src {
            line = line.chars().take(max_src - MARK.len()).collect::<String>() + MARK;
            usable = max_src - MARK.len();
        }
        let gap = if line.is_empty() { "" } else { " " };
        out.push(format!("{:>label$}{gap}{line}", start + i + 1));
    }
    let col = origin.col;
    if col >= 1 && col <= usable {
        let marker = format!("column {col}");
        let idx = col - 1;
        let marker = if idx + 2 + marker.len() > max_src {
            format!(
                "{}{marker} ^",
                " ".repeat(idx.saturating_sub(marker.len() + 1))
            )
        } else {
            format!("{}^ {marker}", " ".repeat(idx))
        };
        out.push(format!("{} {marker}", " ".repeat(label)));
    }
    out
}

impl Renderer {
    /// The block a failure's line is preceded by. Censored lines have none: a censored result
    /// carries no `exception` for the reference's callback to show.
    ///
    /// A block already written is not written again, for the whole run: `Display.error` keeps
    /// every one in `_errors`, as `_warning` does. Measured on ansible-core 2.19.12: two hosts, or
    /// two items, failing one task with one message print one block and two `fatal:` lines.
    pub fn error(&mut self, block: &ErrorBlock) {
        let mut text = format!("[ERROR]: {}", block.message);
        if block.causes.is_empty() {
            if let Some(origin) = &block.origin {
                text.push('\n');
                text.push_str(&source_context(origin));
            } else if let Some(data) = &block.unknown {
                text.push_str("\nOrigin: <unknown>\n\n");
                text.push_str(data);
            }
        } else {
            // `format_event_verbose_message`: the brief message, then one segment per link.
            let mut first = "Task failed.".to_string();
            if let Some(origin) = &block.origin {
                first.push('\n');
                first.push_str(&source_context(origin));
            }
            let mut segments = vec![first];
            for cause in &block.causes {
                let mut segment = cause.msg.clone();
                if let Some(origin) = &cause.origin {
                    segment.push('\n');
                    segment.push_str(&source_context(origin));
                }
                if let Some(help) = &cause.help {
                    segment.push_str("\n\n");
                    segment.push_str(help);
                }
                segments.push(segment);
            }
            text.push_str("\n\n");
            text.push_str(&segments.join("\n\n<<< caused by >>>\n\n"));
        }
        if !self.errored.insert(text.clone()) {
            return;
        }
        // `format_event`: a message of several lines ends with a blank one.
        if text.contains('\n') {
            text.push('\n');
        }
        let _ = writeln!(self.out, "{}", self.paint(FAILED, &text));
    }

    /// `censored` is the `no_log` of the task whose batch could not be sent. Measured on
    /// ansible-core 2.19.12: the reference censors this line too, so the reason the host could
    /// not be reached is hidden along with everything else the task would have printed.
    pub fn unreachable(&mut self, host: &str, msg: &str, censored: bool, delegate: Option<&str>) {
        let body = if censored {
            serde_json::json!({"censored": CENSORED, "changed": false})
        } else {
            serde_json::json!({"changed": false, "msg": msg, "unreachable": true})
        };
        let who = match delegate {
            Some(d) => format!("{host} -> {d}"),
            None => host.to_string(),
        };
        let json = if self.verbosity > 2 {
            ansible_json_indented(&body, 0)
        } else {
            ansible_json(&body)
        };
        let line = format!("fatal: [{who}]: UNREACHABLE! => {json}");
        let _ = writeln!(self.out, "{}", self.paint(UNREACHABLE, &line));
    }
}
