// SPDX-License-Identifier: GPL-3.0-or-later
//! Console output in the shape of ansible-playbook's default callback.

use std::io::Write;

use anstream::{AutoStream, ColorChoice};
use anstyle::{AnsiColor, Style};
use volant_protocol::TaskResult;

use crate::stats::{Outcome, Stats};

/// How a task's result lines are cleaned before they are shown. Whether a body goes on the line
/// at verbosity 0 is the result's own [`VERBOSE_ALWAYS`], not the task's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dump {
    /// Shown as it is.
    No,
    /// A `debug`'s: cleaned the way the reference's callback cleans one (`_clean_results`).
    Debug,
}

/// The key a `debug` or a non-`quiet` `assert` puts on its own result, as the reference's action
/// plugins do: its body is shown at verbosity 0, indented. A failure the executor raised instead
/// (a keyword that does not convert, a timeout out of range) carries none, so it prints on one
/// line whatever the module. Never shown, never registered: every `_ansible_` key is internal.
pub const VERBOSE_ALWAYS: &str = "_ansible_verbose_always";

/// What the reference's callback shows of a `no_log` result: [`CENSORED`] and the keys its
/// `_PRESERVE` keeps (`attempts`, `changed`, `retries`), `changed` always present here.
fn censored_body(result: &TaskResult) -> serde_json::Map<String, serde_json::Value> {
    let mut body = serde_json::Map::new();
    body.insert("censored".into(), serde_json::json!(CENSORED));
    body.insert("changed".into(), serde_json::json!(result.changed()));
    for key in ["attempts", "retries"] {
        if let Some(value) = result.0.get(key) {
            body.insert(key.into(), value.clone());
        }
    }
    body
}

/// Whether the reference shows this result's body at every verbosity: it carries
/// [`VERBOSE_ALWAYS`] and is not censored, which drops the key.
fn verbose_always(result: &TaskResult, censored: bool) -> bool {
    !censored && result.0.get(VERBOSE_ALWAYS) == Some(&serde_json::Value::Bool(true))
}

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

fn causes_of(result: &TaskResult) -> Vec<Cause> {
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
fn censor(message: &str) -> String {
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

fn message_of(result: &TaskResult) -> String {
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
// ponytail: words split on whitespace only, where `TextWrapper` also breaks after a hyphen;
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
fn source_context(origin: &crate::playbook::Origin) -> String {
    let mut out = format!("Origin: {}:{}:{}", origin.path, origin.line, origin.col);
    let annotated = annotated_lines(origin);
    if !annotated.is_empty() {
        out.push_str("\n\n");
        out.push_str(&annotated.join("\n"));
    }
    out
}

fn annotated_lines(origin: &crate::playbook::Origin) -> Vec<String> {
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

pub struct Renderer {
    out: Box<dyn Write>,
    color: bool,
    width: usize,
    verbosity: u8,
    /// Every `[WARNING]:` line written so far, which is never written twice.
    warned: std::collections::HashSet<String>,
    /// Every `[ERROR]:` block written so far, which is never written twice either.
    errored: std::collections::HashSet<String>,
}

const HOST_COLUMN: usize = 26;

/// What a `no_log` result shows instead of itself, measured on ansible-core 2.19.12 - the body
/// of every censored line is `{"censored": CENSORED, "changed": <changed>}`.
pub const CENSORED: &str =
    "the output has been hidden due to the fact that 'no_log: true' was specified for this result";

/// What a `no_log` loop item shows instead of its label, measured on the same release:
/// `changed: [h1] => (item=(censored due to no_log))`.
pub const CENSORED_ITEM: &str = "(censored due to no_log)";

const OK: Style = AnsiColor::Green.on_default();
const CHANGED: Style = AnsiColor::Yellow.on_default();
const FAILED: Style = AnsiColor::Red.on_default();
const UNREACHABLE: Style = AnsiColor::BrightRed.on_default();
const SKIPPED: Style = AnsiColor::Cyan.on_default();
const WARNING: Style = AnsiColor::BrightMagenta.on_default();

impl Renderer {
    /// Stdout renderer. Colour follows the terminal, `NO_COLOR`, `CLICOLOR_FORCE`, or the caller.
    pub fn new(choice: ColorChoice, verbosity: u8) -> Self {
        let stream = AutoStream::new(std::io::stdout(), choice);
        let color = stream.current_choice() != ColorChoice::Never;
        let width = std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.parse().ok())
            .filter(|w| *w >= 40)
            .unwrap_or(79);
        Self {
            out: Box::new(stream),
            color,
            width,
            verbosity,
            warned: std::collections::HashSet::new(),
            errored: std::collections::HashSet::new(),
        }
    }

    #[cfg(test)]
    pub fn with_writer(out: Box<dyn Write>, color: bool, width: usize, verbosity: u8) -> Self {
        Self {
            out,
            color,
            width,
            verbosity,
            warned: std::collections::HashSet::new(),
            errored: std::collections::HashSet::new(),
        }
    }

    fn paint(&self, style: Style, text: &str) -> String {
        if self.color {
            format!("{style}{text}{style:#}")
        } else {
            text.to_string()
        }
    }

    /// The star run fills the width past the text, so the line is one character wider than the
    /// width, and never shorter than three stars: what `ansible-playbook` prints.
    fn banner(&mut self, text: &str) {
        let stars = self.width.saturating_sub(text.chars().count()).max(3);
        let _ = writeln!(self.out, "\n{text} {}", "*".repeat(stars));
    }

    pub fn play(&mut self, name: &str) {
        self.banner(&format!("PLAY [{name}]"));
    }

    pub fn task(&mut self, name: &str) {
        self.banner(&format!("TASK [{name}]"));
    }

    /// The banner a handler gets instead of a task's, measured on ansible-core 2.19.12:
    /// `RUNNING HANDLER [second handler]`, and `RUNNING HANDLER [base : base handler]` for one
    /// that came out of a role.
    pub fn handler(&mut self, name: &str) {
        self.banner(&format!("RUNNING HANDLER [{name}]"));
    }

    /// The line an include prints once the coordinator knows which hosts asked for what.
    ///
    /// Measured on ansible-core 2.19.12: `included: /abs/inc-a.yml for h1, h2` for a file, with
    /// the absolute path and the hosts in the play's own order; `included: base for h1` for a
    /// role, by its bare name; and `=> (item=1)` behind either when a loop asked for it. It is
    /// painted like an `ok:` line, which is what the reference's own callback does with it.
    pub fn included(&mut self, what: &str, hosts: &[String], label: Option<&str>) {
        let item = label.map(|l| format!(" => (item={l})")).unwrap_or_default();
        let line = format!("included: {what} for {}{item}", hosts.join(", "));
        let _ = writeln!(self.out, "{}", self.paint(OK, &line));
    }

    pub fn no_hosts(&mut self) {
        let _ = writeln!(
            self.out,
            "{}",
            self.paint(SKIPPED, "skipping: no hosts matched")
        );
    }

    /// A warning goes to stderr, which is where ansible-playbook puts every one of its own -
    /// measured on ansible-core 2.19.12, and measured here as a difference: the listing golden
    /// compares stdout alone, and a `[WARNING]` about an unmatched host pattern was landing in
    /// the middle of it. A warning mixed into stdout also reaches anything piping a listing or
    /// a JSON stream into another program, which is the reason the reference separates them.
    ///
    /// `censored` is the `no_log` of the task the warning belongs to: a diagnostic a task
    /// produced is that task speaking, so the policy that hides its result hides this too. It is
    /// false for every warning the engine writes about itself, and false for the `environment`
    /// warning as well - measured on ansible-core 2.19.12, that one quotes the playbook's own
    /// source (`['{{ secret }}']`) rather than a rendered value, so there is nothing in it to
    /// hide and the reference does not hide it either.
    ///
    /// A line already written is not written again, for the whole run, whichever task or host
    /// it came from: ansible-core 2.19.12's `Display._warning` keeps every `[WARNING]: <text>`
    /// it has shown in `_warns` and drops a repeat (`_deduplicate`, keyed by that text alone). A
    /// registered result still holds every copy, as the reference's does.
    pub fn warning(&mut self, text: &str, censored: bool) {
        let body = if censored { CENSORED } else { text };
        let line = format!("[WARNING]: {body}");
        if !self.warned.insert(line.clone()) {
            return;
        }
        let _ = writeln!(anstream::stderr(), "{}", self.paint(WARNING, &line));
    }

    /// A line an action writes on stdout as it runs, as it wrote it.
    pub fn display(&mut self, line: &str) {
        let _ = writeln!(self.out, "{line}");
    }

    /// The line a failed attempt prints before the task is reported, measured on ansible-core
    /// 2.19.12: it goes to stdout from verbosity 0, it names the task rather than the module,
    /// and it is printed after **every** failed attempt, the last one included.
    ///
    /// `result` is that attempt's, `attempts` and `retries` on it. Its body follows on the same
    /// line, `Result was: {...}` indented, from `-vvv` on or when it carries [`VERBOSE_ALWAYS`]
    /// (`v2_runner_retry`): measured with a `debug` and an `assert` at verbosity 0, a `command`
    /// at `-vv` (nothing) and at `-vvv`. It is the raw attempt, never cleaned as a `debug`; a
    /// `no_log` one shows its censored body, and only from `-vvv` on.
    pub fn retrying(
        &mut self,
        host: &str,
        name: &str,
        left: u32,
        result: &TaskResult,
        censored: bool,
    ) {
        let mut line = format!("FAILED - RETRYING: [{host}]: {name} ({left} retries left).");
        if self.verbosity > 2 || verbose_always(result, censored) {
            let body = if censored {
                censored_body(result)
            } else {
                let mut body = result.0.clone();
                body.retain(|k, _| !k.starts_with("_ansible_") && !k.starts_with("_volant_"));
                for key in [
                    "failed",
                    "skipped",
                    "exception",
                    "warnings",
                    "deprecations",
                    ERROR_KEY,
                ] {
                    body.remove(key);
                }
                if self.verbosity < 3 {
                    body.remove("invocation");
                    body.remove("diff");
                }
                body
            };
            line.push_str("Result was: ");
            line.push_str(&ansible_json_indented(&serde_json::Value::Object(body), 0));
        }
        let _ = writeln!(self.out, "{line}");
    }

    /// `label` is the loop item's display text, present only for a loop item result. `dump`
    /// forces the JSON tail even for an `ok` result at verbosity 0; see [`Dump`].
    ///
    /// `censored` is the task's `no_log`: the body becomes `{"censored": CENSORED, "changed":
    /// ...}` and the item label becomes [`CENSORED_ITEM`], which is what keeps a secret out of
    /// every line this function can print - the ordinary one, the `fatal:`, the loop item and
    /// the `debug`, at every verbosity. What it does **not** touch is the result itself: the
    /// registered variable and the recap read the real one, measured.
    #[expect(
        clippy::too_many_arguments,
        reason = "one task result's whole display context: the outcome plus four parameters that change what gets printed - two flags (dump, censored) and two optional strings (label, delegate)"
    )]
    pub fn result(
        &mut self,
        host: &str,
        outcome: Outcome,
        result: &TaskResult,
        label: Option<&str>,
        dump: Dump,
        censored: bool,
        delegate: Option<&str>,
    ) {
        let label = if censored {
            label.map(|_| CENSORED_ITEM)
        } else {
            label
        };
        let verbose = verbose_always(result, censored);
        let mut body = if censored {
            censored_body(result)
        } else {
            result.0.clone()
        };
        body.retain(|k, _| !k.starts_with("_ansible_") && !k.starts_with("_volant_"));
        // The reference's callback never shows `failed` or `skipped`: its `TaskResult` drops both
        // (`_IGNORE` in `executor/task_result.py`, 2.19.12) from every result it hands over, a loop
        // item's included. Measured on the `fatal:` line of a timed-out `pause`, a failed `copy`
        // validate and a failed `lineinfile`, which also show no `invocation` and no `diff`: its
        // `_dump_results` keeps those two from `-vvv` on, read off the source and not measured.
        if !censored {
            body.remove("failed");
            body.remove("skipped");
            if self.verbosity < 3
                && matches!(
                    outcome,
                    Outcome::Failed | Outcome::Rescued | Outcome::Ignored
                )
            {
                body.remove("invocation");
                body.remove("diff");
            }
        }
        // Registered and never shown: the reference's callback pops it off every line, measured.
        body.remove("exception");
        body.remove(ERROR_KEY);
        if dump == Dump::Debug {
            // The reference's `_clean_results` for a `debug`, ansible-core 2.19.12: a result with
            // a `msg` keeps it alone, whatever `changed_when`, `failed_when` and `until` added
            // (measured: a `debug` retried twice shows `{"msg": "probe"}` and registers
            // `attempts: 2`); a `var` one loses `changed`, `failed`, `skipped`, `invocation`,
            // `skip_reason` and the loop's own keys, but keeps the item (measured with a loop).
            if body.contains_key("msg") {
                body.retain(|k, _| {
                    matches!(
                        k.as_str(),
                        "msg" | "exception" | "warnings" | "deprecations"
                    )
                });
            } else {
                for key in [
                    "changed",
                    "failed",
                    "skipped",
                    "invocation",
                    "skip_reason",
                    "ansible_loop_var",
                    "ansible_index_var",
                    "ansible_loop",
                ] {
                    body.remove(key);
                }
            }
        }
        // `_dump_results` indents by 4 for a result carrying [`VERBOSE_ALWAYS`] and for every
        // result from `-vvv` on.
        let pretty = verbose || self.verbosity > 2;
        let json = if pretty {
            ansible_json_indented(&serde_json::Value::Object(body), 0)
        } else {
            ansible_json(&serde_json::Value::Object(body))
        };
        let item = label.map(|l| format!(" => (item={l})")).unwrap_or_default();
        // A censored `debug` still goes through the cleanup above - which is what leaves its
        // body as `{"censored": ...}` with no `changed` - but shows nothing at verbosity 0:
        // measured, `ok: [h1]` alone there and the censored body from `-v` on.
        let show = verbose || self.verbosity > 0;
        let tail = if show {
            format!(" => {json}")
        } else {
            String::new()
        };
        // `[h1 -> h3]` wherever a delegated task has a line, measured on ansible-core 2.19.12 -
        // `ok:`, `changed:`, the `fatal:` of a failure and a loop item's own line all carry the
        // arrow. `skipping:` is the one that does not: a task a `when` left out never reached
        // the delegate, and the reference prints `skipping: [h1]` for it.
        let who = match delegate {
            Some(d) => format!("{host} -> {d}"),
            None => host.to_string(),
        };
        let line = match outcome {
            Outcome::Ok => self.paint(OK, &format!("ok: [{who}]{item}{tail}")),
            Outcome::Changed => self.paint(CHANGED, &format!("changed: [{who}]{item}{tail}")),
            // Measured on ansible-core 2.19.12: a loop item's own `skipping:` line ends with a
            // trailing space (`skipping: [h] => (item=x) `); the task's own `skipping: [h]`, with
            // no item, carries none. From `-v` on the body follows: `skipping: [h] => (item=2)  =>
            // {...}` for an item, `skipping: [h] => {...}` for the task, measured for a `when`,
            // an empty loop and a loop whose every item was skipped.
            Outcome::Skipped => {
                let trailer = if label.is_some() { " " } else { "" };
                self.paint(SKIPPED, &format!("skipping: [{host}]{item}{trailer}{tail}"))
            }
            // A rescued failure shows exactly like one nothing catches: measured on
            // ansible-core 2.19.12, the line is the same `fatal: ... FAILED!` and the only
            // difference is in the recap. Showing it any other way would hide from the
            // operator that the task failed at all.
            Outcome::Failed | Outcome::Rescued | Outcome::Ignored if label.is_some() => self.paint(
                FAILED,
                &format!(
                    "failed: [{who}] (item={}) => {json}",
                    label.unwrap_or_default()
                ),
            ),
            Outcome::Failed | Outcome::Rescued | Outcome::Ignored => {
                self.paint(FAILED, &format!("fatal: [{who}]: FAILED! => {json}"))
            }
        };
        let _ = writeln!(self.out, "{line}");
        if outcome == Outcome::Ignored && label.is_none() {
            self.ignoring();
        }
    }

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

    /// The line that follows a failure `ignore_errors` swallowed. A looping task prints it on its
    /// own, with no result line before it: the reference shows the items and then this, never an
    /// aggregate line of its own.
    pub fn ignoring(&mut self) {
        let _ = writeln!(self.out, "{}", self.paint(SKIPPED, "...ignoring"));
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

    pub fn recap(&mut self, stats: &Stats) {
        self.banner("PLAY RECAP");
        for (host, h) in stats.hosts() {
            let host_style = if h.failed > 0 || h.unreachable > 0 {
                FAILED
            } else if h.changed > 0 {
                CHANGED
            } else {
                OK
            };
            let name = if self.color {
                let painted = self.paint(host_style, host);
                let width = HOST_COLUMN + (painted.len() - host.len());
                format!("{painted:<width$}")
            } else {
                format!("{host:<HOST_COLUMN$}")
            };
            let field = |lead: &str, n: u32, style: Style| {
                let text = format!("{lead}={n:<4}");
                if n > 0 {
                    self.paint(style, &text)
                } else {
                    text
                }
            };
            let _ = writeln!(
                self.out,
                "{name} : {} {} {} {} {} {} {}",
                field("ok", h.ok, OK),
                field("changed", h.changed, CHANGED),
                field("unreachable", h.unreachable, UNREACHABLE),
                field("failed", h.failed, FAILED),
                field("skipped", h.skipped, SKIPPED),
                field("rescued", h.rescued, OK),
                field("ignored", h.ignored, SKIPPED),
            );
        }
        let _ = writeln!(self.out);
    }
}

/// Ansible dumps results as JSON with sorted keys and `": "` and `", "` separators.
pub(crate) fn ansible_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    format!(
                        "{}: {}",
                        serde_json::to_string(k).unwrap_or_default(),
                        ansible_json(&map[k])
                    )
                })
                .collect();
            format!("{{{}}}", fields.join(", "))
        }
        serde_json::Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(ansible_json)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// `json.dumps(value, indent=4, sort_keys=True, ensure_ascii=False)`: one key or item per line,
/// `,` at the end of each but the last, an empty mapping or list kept as `{}` or `[]`.
pub(crate) fn ansible_json_indented(value: &serde_json::Value, depth: usize) -> String {
    let pad = |depth: usize| " ".repeat(4 * depth);
    match value {
        serde_json::Value::Object(map) if !map.is_empty() => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    format!(
                        "{}{}: {}",
                        pad(depth + 1),
                        serde_json::to_string(k).unwrap_or_default(),
                        ansible_json_indented(&map[k], depth + 1)
                    )
                })
                .collect();
            format!("{{\n{}\n{}}}", fields.join(",\n"), pad(depth))
        }
        serde_json::Value::Array(items) if !items.is_empty() => {
            let items: Vec<String> = items
                .iter()
                .map(|v| format!("{}{}", pad(depth + 1), ansible_json_indented(v, depth + 1)))
                .collect();
            format!("[\n{}\n{}]", items.join(",\n"), pad(depth))
        }
        other => ansible_json(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    struct Shared(Arc<Mutex<Vec<u8>>>);
    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture_at(color: bool, verbosity: u8, f: impl FnOnce(&mut Renderer)) -> String {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut r = Renderer::with_writer(Box::new(Shared(buf.clone())), color, 79, verbosity);
        f(&mut r);
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    }

    fn capture_with_color(color: bool, f: impl FnOnce(&mut Renderer)) -> String {
        capture_at(color, 0, f)
    }

    fn capture(f: impl FnOnce(&mut Renderer)) -> String {
        capture_with_color(false, f)
    }

    /// The source lines under `Origin:`, read the way Python's text mode reads them, and what
    /// shows when the file cannot be read.
    #[rstest]
    #[case::crlf(b"a: 1\r\nb: 2\r\n".as_slice(), vec!["1 a: 1", "2 b: 2", "  ^ column 1"])]
    #[case::lone_cr(b"a: 1\rb: 2\r".as_slice(), vec!["1 a: 1", "2 b: 2", "  ^ column 1"])]
    #[case::not_utf8(b"a: \xff\nb: 2\n".as_slice(), vec!["(source not shown: UnicodeDecodeError)"])]
    fn source_lines_read_as_python_does(#[case] bytes: &[u8], #[case] expected: Vec<&str>) {
        let dir = std::env::temp_dir().join(format!("volant-lines-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch dir");
        let path = dir.join(format!("{}.yml", expected.len() + bytes.len()));
        std::fs::write(&path, bytes).expect("the source");
        let origin = crate::playbook::Origin {
            path: path.display().to_string().into(),
            line: 2,
            col: 1,
        };
        assert_eq!(annotated_lines(&origin), expected);
        let missing = crate::playbook::Origin {
            path: dir.join("missing.yml").display().to_string().into(),
            ..origin
        };
        assert_eq!(
            annotated_lines(&missing),
            ["(source not shown: FileNotFoundError)"]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn shorten_keeps_whole_words_as_textwrap_does() {
        assert_eq!(shorten("a  b\n c", 120), "a b c");
        assert_eq!(shorten("aaaa bbbb cccc dddd", 15), "aaaa bbbb [...]");
        assert_eq!(shorten("aaaaaaaaaaaaaaaaaaaa", 10), "[...]");
    }

    fn result(v: serde_json::Value) -> TaskResult {
        TaskResult(v.as_object().unwrap().clone())
    }

    /// Strips ANSI CSI sequences (`ESC [ ... letter`), the shape anstyle emits, so a
    /// coloured capture can be compared against the plain one on visible text alone.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' && chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    /// Banners pad with stars to the width, count characters and keep three past it; the recap,
    /// the unreachable, no-hosts and retry lines are the reference's own.
    #[test]
    fn banners_recap_and_status_lines() {
        let out = capture(|r| r.play("Smoke test"));
        let line = out.lines().nth(1).unwrap();
        assert!(line.starts_with("PLAY [Smoke test] ***"));
        assert_eq!(line.len(), 80);
        assert_eq!(out, format!("\n{line}\n"));
        let out = capture(|r| r.play("Déploiement été"));
        assert_eq!(out.lines().nth(1).unwrap().chars().count(), 80);
        let name = "x".repeat(120);
        let out = capture(|r| r.play(&name));
        assert_eq!(out.lines().nth(1).unwrap(), format!("PLAY [{name}] ***"));
        let out = capture(|r| r.task("Say hello"));
        assert!(
            out.lines()
                .nth(1)
                .unwrap()
                .starts_with("TASK [Say hello] ***")
        );

        let out = capture(|r| {
            r.unreachable("db1", "agent binary not found", false, None);
            r.no_hosts();
            r.unreachable("h1", "starting ssh: secret-host", true, None);
            r.retrying(
                "h1",
                "retry until file",
                3,
                &result(json!({"rc": 1})),
                false,
            );
            let debug = result(
                json!({"_ansible_verbose_always": true, "attempts": 1, "failed": true, "msg": "probe", "retries": 3}),
            );
            r.retrying("h1", "probe", 2, &debug, false);
            r.retrying("h1", "hushed", 2, &debug, true);
        });
        assert_eq!(
            out,
            format!(
                "fatal: [db1]: UNREACHABLE! => {{\"changed\": false, \"msg\": \"agent binary not found\", \"unreachable\": true}}\nskipping: no hosts matched\nfatal: [h1]: UNREACHABLE! => {{\"censored\": \"{CENSORED}\", \"changed\": false}}\nFAILED - RETRYING: [h1]: retry until file (3 retries left).\nFAILED - RETRYING: [h1]: probe (2 retries left).Result was: {{\n    \"attempts\": 1,\n    \"msg\": \"probe\",\n    \"retries\": 3\n}}\nFAILED - RETRYING: [h1]: hushed (2 retries left).\n"
            )
        );

        let mut stats = Stats::default();
        stats.record("localhost", Outcome::Changed, true);
        stats.record("localhost", Outcome::Ok, false);
        stats.record("localhost", Outcome::Ignored, false);
        let out = capture(|r| r.recap(&stats));
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[1].starts_with("PLAY RECAP ***"));
        assert_eq!(
            lines[2],
            "localhost                  : ok=3    changed=1    unreachable=0    failed=0    skipped=0    rescued=0    ignored=1   "
        );
    }

    /// The line a result prints, in the reference's words; `<CENSORED>` stands for the
    /// censored sentence.
    #[rstest]
    #[case::changed(0, Outcome::Changed, json!({"changed": true, "stdout": "hi"}), None, Dump::No, false, "changed: [h]\n")]
    #[case::ok(0, Outcome::Ok, json!({"changed": false}), None, Dump::No, false, "ok: [h]\n")]
    #[case::skipped(0, Outcome::Skipped, json!({"skipped": true}), None, Dump::No, false, "skipping: [h]\n")]
    #[case::failed(
        0, Outcome::Failed, json!({"failed": true, "rc": 1, "msg": "non-zero return code"}), None, Dump::No, false,
        "fatal: [h]: FAILED! => {\"msg\": \"non-zero return code\", \"rc\": 1}\n"
    )]
    #[case::ignored(
        0, Outcome::Ignored, json!({"failed": true, "rc": 1}), None, Dump::No, false,
        "fatal: [h]: FAILED! => {\"rc\": 1}\n...ignoring\n"
    )]
    #[case::exception_stays_off_the_line(
        0, Outcome::Failed,
        json!({"changed": false, "exception": "(traceback unavailable)", "failed": true, "msg": "Error executing command.", "rc": 2}),
        None, Dump::No, false,
        "fatal: [h]: FAILED! => {\"changed\": false, \"msg\": \"Error executing command.\", \"rc\": 2}\n"
    )]
    #[case::failed_diff_and_invocation_stay_off(
        0, Outcome::Failed,
        json!({"changed": false, "diff": [], "failed": true, "invocation": {"module_args": {"path": "/tmp/x"}}, "msg": "boom"}),
        None, Dump::No, false,
        "fatal: [h]: FAILED! => {\"changed\": false, \"msg\": \"boom\"}\n"
    )]
    #[case::verbose_ok(
        1, Outcome::Ok, json!({"changed": false, "stdout": "x"}), None, Dump::No, false,
        "ok: [h] => {\"changed\": false, \"stdout\": \"x\"}\n"
    )]
    #[case::verbose_ok_does_not_say_it_did_not_fail(
        1, Outcome::Ok, json!({"changed": false, "failed": false, "ping": "pong"}), None, Dump::No, false,
        "ok: [h] => {\"changed\": false, \"ping\": \"pong\"}\n"
    )]
    #[case::changed_item(0, Outcome::Changed, json!({"changed": true}), Some("one"), Dump::No, false, "changed: [h] => (item=one)\n")]
    #[case::skipped_item(0, Outcome::Skipped, json!({"skipped": true}), Some("two"), Dump::No, false, "skipping: [h] => (item=two) \n")]
    #[case::skipped_item_verbose(
        1, Outcome::Skipped, json!({"changed": false, "item": 2, "skipped": true}), Some("2"), Dump::No, false,
        "skipping: [h] => (item=2)  => {\"changed\": false, \"item\": 2}\n"
    )]
    #[case::failed_item(
        0, Outcome::Failed, json!({"failed": true, "rc": 1}), Some("three"), Dump::No, false,
        "failed: [h] (item=three) => {\"rc\": 1}\n"
    )]
    #[case::forced_dump(0, Outcome::Ok, json!({"_ansible_verbose_always": true, "msg": "shown"}), None, Dump::Debug, false, "ok: [h] => {\n    \"msg\": \"shown\"\n}\n")]
    #[case::debug_var_keeps_its_item(
        0, Outcome::Ok, json!({"_ansible_verbose_always": true, "ansible_loop_var": "item", "changed": false, "item": "a"}), Some("a"), Dump::Debug, false,
        "ok: [h] => (item=a) => {\n    \"item\": \"a\"\n}\n"
    )]
    #[case::raised_failure_on_one_line(
        0, Outcome::Failed, json!({"changed": false, "failed": true, "msg": "Task failed: x"}), None, Dump::Debug, false,
        "fatal: [h]: FAILED! => {\"msg\": \"Task failed: x\"}\n"
    )]
    #[case::censored_keeps_attempts(
        0, Outcome::Failed, json!({"attempts": 2, "changed": false, "failed": true, "msg": "secret"}), None, Dump::No, true,
        "fatal: [h]: FAILED! => {\"attempts\": 2, \"censored\": \"<CENSORED>\", \"changed\": false}\n"
    )]
    #[case::censored_changed(0, Outcome::Changed, json!({"changed": true, "stdout": "secret"}), None, Dump::No, true, "changed: [h]\n")]
    #[case::censored_ignored(
        0, Outcome::Ignored, json!({"changed": true, "failed": true, "stdout": "secret"}), None, Dump::No, true,
        "fatal: [h]: FAILED! => {\"censored\": \"<CENSORED>\", \"changed\": true}\n...ignoring\n"
    )]
    #[case::censored_item(
        0, Outcome::Changed, json!({"changed": true, "stdout": "secret"}), Some("a"), Dump::No, true,
        "changed: [h] => (item=(censored due to no_log))\n"
    )]
    #[case::censored_debug(0, Outcome::Ok, json!({"_ansible_verbose_always": true, "changed": false, "msg": "hush"}), None, Dump::Debug, true, "ok: [h]\n")]
    #[case::censored_changed_verbose(
        1, Outcome::Changed, json!({"changed": true, "stdout": "secret"}), None, Dump::No, true,
        "changed: [h] => {\"censored\": \"<CENSORED>\", \"changed\": true}\n"
    )]
    #[case::censored_debug_verbose(
        1, Outcome::Ok, json!({"_ansible_verbose_always": true, "changed": false, "msg": "hush"}), None, Dump::Debug, true,
        "ok: [h] => {\"censored\": \"<CENSORED>\"}\n"
    )]
    fn a_result_prints_the_reference_line(
        #[case] verbosity: u8,
        #[case] outcome: Outcome,
        #[case] body: serde_json::Value,
        #[case] item: Option<&str>,
        #[case] dump: Dump,
        #[case] censored: bool,
        #[case] expected: &str,
    ) {
        let out = capture_at(false, verbosity, |r| {
            r.result("h", outcome, &result(body), item, dump, censored, None);
        });
        assert_eq!(out, expected.replace("<CENSORED>", CENSORED));
    }

    /// Colour wraps the visible text and never rewrites it: the recap still lines up, and every
    /// result line is contained verbatim in the coloured output.
    #[test]
    fn colour_only_wraps_what_is_printed() {
        let mut stats = Stats::default();
        stats.record("localhost", Outcome::Changed, true);
        stats.record("localhost", Outcome::Ok, false);
        stats.record("localhost", Outcome::Ignored, false);
        let plain = capture(|r| r.recap(&stats));
        let coloured = capture_with_color(true, |r| r.recap(&stats));
        assert_ne!(coloured, plain, "colour should change the output at all");
        assert_eq!(strip_ansi(&coloured), plain, "the recap columns");

        let cases = [
            (Outcome::Ok, result(json!({"changed": false}))),
            (Outcome::Changed, result(json!({"changed": true}))),
            (Outcome::Skipped, result(json!({"skipped": true}))),
            (Outcome::Failed, result(json!({"failed": true, "rc": 1}))),
            (Outcome::Ignored, result(json!({"failed": true, "rc": 1}))),
        ];
        for (outcome, task_result) in cases {
            let plain =
                capture(|r| r.result("h", outcome, &task_result, None, Dump::No, false, None));
            let coloured = capture_with_color(true, |r| {
                r.result("h", outcome, &task_result, None, Dump::No, false, None);
            });
            assert_ne!(
                coloured, plain,
                "{outcome:?}: colour should change the output"
            );
            assert_eq!(strip_ansi(&coloured), plain, "{outcome:?}: visible text");
            for line in plain.lines() {
                assert!(coloured.contains(line), "{outcome:?}: {line:?} rewritten");
            }
        }
    }
}
