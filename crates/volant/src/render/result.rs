// SPDX-License-Identifier: GPL-3.0-or-later
//! One task's result as the console shows it: the line, the retry notice, the diff, and what
//! `no_log` hides.

use std::io::Write;

use volant_protocol::TaskResult;

use super::{
    CENSORED, CENSORED_ITEM, CHANGED, DIFF_KEY, Dump, ERROR_KEY, FAILED, OK, Renderer, SKIPPED,
    VERBOSE_ALWAYS,
};
use crate::stats::Outcome;

use super::recap_json::{ansible_json, ansible_json_indented};

/// What the reference's callback shows of a `no_log` result: [`CENSORED`] and the keys its
/// `_PRESERVE` keeps (`attempts`, `changed`, `retries`), `changed` present unless a skipped result had none.
pub(super) fn censored_body(result: &TaskResult) -> serde_json::Map<String, serde_json::Value> {
    let mut body = serde_json::Map::new();
    body.insert("censored".into(), serde_json::json!(CENSORED));
    // A skipped `meta` carries no `changed` and the reference shows none for it.
    if result.0.contains_key("changed") || !result.skipped() {
        body.insert("changed".into(), serde_json::json!(result.changed()));
    }
    for key in ["attempts", "retries"] {
        if let Some(value) = result.0.get(key) {
            body.insert(key.into(), value.clone());
        }
    }
    body
}

/// Whether the reference shows this result's body at every verbosity: it carries
/// [`VERBOSE_ALWAYS`] and is not censored, which drops the key.
pub(super) fn verbose_always(result: &TaskResult, censored: bool) -> bool {
    !censored && result.0.get(VERBOSE_ALWAYS) == Some(&serde_json::Value::Bool(true))
}

impl Renderer {
    /// What a result line shows of `result`: the internal keys and what the reference's
    /// `TaskResult` and `_dump_results` drop taken off, and a `debug`'s cleaned when `dump` says so.
    pub(super) fn shown_body(
        &self,
        result: &TaskResult,
        dump: Dump,
        censored: bool,
    ) -> serde_json::Map<String, serde_json::Value> {
        let mut body = if censored {
            censored_body(result)
        } else {
            result.0.clone()
        };
        let items = result
            .0
            .get(super::LOOP_RESULTS)
            .filter(|_| self.minimal.is_some());
        body.retain(|k, _| !k.starts_with("_ansible_") && !k.starts_with("_volant_"));
        if let Some(items) = items.filter(|_| !censored) {
            body.insert("results".into(), items.clone());
        }
        // The reference's callback never shows `failed` or `skipped`: its `TaskResult` drops both
        // (`_IGNORE` in `executor/task_result.py`, 2.19.12) from every result it hands over, a loop
        // item's included. Measured on the `fatal:` line of a timed-out `pause`, a failed `copy`
        // validate and a failed `lineinfile`, which also show no `invocation` and no `diff`: its
        // `_dump_results` keeps those two from `-vvv` on, read off the source and not measured.
        if !censored {
            body.remove("failed");
            body.remove("skipped");
            if self.verbosity < 3 {
                // The reference's `_dump_results` keeps the diff and the `invocation` off the line
                // of every outcome below `-vvv`; it prints the diff apart. Measured on `ok`,
                // `changed`, `failed` and `skipped` results at `-v` and `-vv`, no module keeps it.
                body.remove("diff");
                body.remove("invocation");
            }
        }
        // Registered and never shown: the reference's callback pops it off every line, measured.
        // Its `_dump_results` pops the warnings and the deprecations too, which it shows apart.
        body.remove("exception");
        body.remove("warnings");
        body.remove("deprecations");
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
        body
    }

    /// The block `v2_on_file_diff` prints in front of a changed result's line, when the task
    /// asked for diffs and the result carries one: the helper is asked only then.
    fn print_diff(&mut self, outcome: Outcome, result: &TaskResult, censored: bool) {
        let asked = result.0.get(DIFF_KEY) == Some(&serde_json::Value::Bool(true));
        let diff = result.0.get("diff").filter(|d| match d {
            serde_json::Value::Null | serde_json::Value::Bool(false) => false,
            serde_json::Value::String(s) => !s.is_empty(),
            serde_json::Value::Array(a) => !a.is_empty(),
            serde_json::Value::Object(o) => !o.is_empty(),
            _ => true,
        });
        let (true, Some(diff), Some(bridge), true) = (
            asked && !censored && matches!(outcome, Outcome::Ok | Outcome::Changed),
            diff,
            self.diffs.clone(),
            result.changed(),
        ) else {
            return;
        };
        match bridge.diff_text(diff, self.color) {
            Ok(text) if text.is_empty() => {}
            Ok(text) => {
                // Measured: the block ends with one blank line, which `_get_diff` already put there.
                let end = if text.ends_with('\n') { "" } else { "\n" };
                let _ = write!(self.out, "{text}{end}");
            }
            Err(err) => self.warning(&format!("the diff could not be rendered: {err}"), false),
        }
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
        // Only the `default` callback shows a retry.
        if self.minimal.is_some() {
            return;
        }
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
            line.push_str(&self.dump_result(body, verbose_always(result, censored), None));
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
        // `minimal`, `oneline` and `json` show no loop item, only the task's own result.
        if label.is_some() && self.minimal.is_some() {
            return;
        }
        if self.json_result(host, outcome, result, censored) {
            return;
        }
        let label = if censored {
            label.map(|_| CENSORED_ITEM)
        } else {
            label
        };
        // The `oneline` callback has no `v2_on_file_diff`.
        if !self.oneline {
            self.print_diff(outcome, result, censored);
        }
        // The `tree` callback hears of every result but a skipped one.
        if self.tree.is_some() && outcome != Outcome::Skipped {
            let event = if matches!(outcome, Outcome::Ok | Outcome::Changed) {
                "v2_runner_on_ok"
            } else {
                "v2_runner_on_failed"
            };
            let body = self.shown_body(result, Dump::No, censored);
            let pretty = verbose_always(result, censored);
            self.tree_file(host, event, body, pretty);
        }
        if self.oneline {
            self.oneline_line(host, outcome, result, censored);
            return;
        }
        if self.minimal.is_some() {
            self.minimal_line(host, outcome, result, dump, censored);
            return;
        }
        let verbose = verbose_always(result, censored);
        let body = self.shown_body(result, dump, censored);
        let json = self.dump_result(body, verbose, None);
        let item = label.map(|l| format!(" => (item={l})")).unwrap_or_default();
        // A censored `debug` still goes through the cleanup above - which is what leaves its
        // body as `{"censored": ...}` with no `changed` - but shows nothing at verbosity 0:
        // measured, `ok: [h1]` alone there and the censored body from `-v` on.
        let show = verbose || self.verbosity > 0;
        let tail = if show
            && !(dump == Dump::Setup && matches!(outcome, Outcome::Ok | Outcome::Changed))
        {
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
    /// One host's line as the `minimal` callback prints it (`plugins/callback/minimal.py`,
    /// ansible-core 2.19.12): `host | STATUS => ` and the result as JSON, four-space indent and
    /// keys sorted, or for a command action the `rc=N >>` form with stdout, stderr and msg run
    /// together. Only an `ok` result goes through `_clean_results`.
    fn minimal_line(
        &mut self,
        host: &str,
        outcome: Outcome,
        result: &TaskResult,
        dump: Dump,
        censored: bool,
    ) {
        let succeeded = matches!(outcome, Outcome::Ok | Outcome::Changed);
        let body = self.shown_body(result, if succeeded { dump } else { Dump::No }, censored);
        let command = self
            .minimal
            .as_deref()
            .is_some_and(|action| MODULE_NO_JSON.contains(&action));
        let (style, line) = match outcome {
            Outcome::Skipped => (SKIPPED, format!("{host} | SKIPPED")),
            Outcome::Ok | Outcome::Changed => {
                let (style, state) = if outcome == Outcome::Changed {
                    (CHANGED, "CHANGED")
                } else {
                    (OK, "SUCCESS")
                };
                let line = if command && !result.0.contains_key("ansible_job_id") {
                    command_text(host, state, &body)
                } else {
                    format!("{host} | {state} => {}", self.minimal_dump(body))
                };
                (style, line)
            }
            Outcome::Failed | Outcome::Rescued | Outcome::Ignored => {
                let line = if command && !result.0.contains_key("module_stderr") {
                    command_text(host, "FAILED", &body)
                } else {
                    format!("{host} | FAILED! => {}", self.minimal_dump(body))
                };
                (FAILED, line)
            }
        };
        let _ = writeln!(self.out, "{}", self.paint(style, &line));
    }
}

/// `result.get('rc', -1)` as `%s` prints it.
pub(super) fn rc_text(body: &serde_json::Map<String, serde_json::Value>) -> String {
    match body.get("rc") {
        None => "-1".to_string(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => crate::playbook::python_repr(other),
    }
}

impl Renderer {
    /// `_dump_results(result, indent=4)`, the `minimal` callback's.
    pub(super) fn minimal_dump(&self, body: serde_json::Map<String, serde_json::Value>) -> String {
        self.dump_result(body, false, Some(4))
    }

    /// `_dump_results` of ansible-core 2.19.12, for a body already cleaned: JSON with sorted keys,
    /// or with `result_format = yaml` a line break and the YAML dump indented by four
    /// (`textwrap.indent`). The indent is four for a result carrying [`VERBOSE_ALWAYS`]
    /// (`verbose_always`), from `-vvv` on, for YAML unless `pretty_results` says otherwise, and
    /// when `pretty_results` is on; `pretty_results` off takes it away whatever asked for it. With
    /// YAML and `pretty_results` not off, `stdout_lines` and `stderr_lines` are written
    /// `<omitted>` beside their text and a block of text is cleaned to keep its block form. The
    /// trailing line break is the line's own.
    pub(super) fn dump_result(
        &self,
        mut body: serde_json::Map<String, serde_json::Value>,
        verbose_always: bool,
        indent: Option<usize>,
    ) -> String {
        let indent = match self.pretty {
            Some(false) => None,
            pretty => indent.or_else(|| {
                (verbose_always
                    || (pretty.is_none() && self.yaml)
                    || pretty == Some(true)
                    || self.verbosity > 2)
                    .then_some(4)
            }),
        };
        if !self.yaml {
            let body = serde_json::Value::Object(body);
            return match indent {
                Some(_) => ansible_json_indented(&body, 0),
                None => ansible_json(&body),
            };
        }
        let lossy = self.pretty != Some(false);
        if lossy {
            for (text, lines) in [("stdout", "stdout_lines"), ("stderr", "stderr_lines")] {
                if body.contains_key(text) && body.contains_key(lines) {
                    body.insert(lines.into(), "<omitted>".into());
                }
            }
        }
        let dumped =
            crate::template::callback_yaml(&serde_json::Value::Object(body), indent, lossy);
        let mut out = String::from("\n");
        // `str.splitlines(keepends=True)`'s boundaries; a line of whitespace only gets no prefix.
        let breaks = |c: char| {
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
        for line in dumped.split_inclusive(breaks) {
            if !line.trim().is_empty() {
                out.push_str("    ");
            }
            out.push_str(line);
        }
        out.strip_suffix('\n').map(str::to_string).unwrap_or(out)
    }
}

/// `C.MODULE_NO_JSON` of ansible-core 2.19.12, its short names with their `ansible.builtin.` and
/// `ansible.legacy.` forms: the actions whose `minimal` line is the command form.
pub(super) const MODULE_NO_JSON: [&str; 17] = [
    "command",
    "ansible.builtin.command",
    "ansible.legacy.command",
    "win_command",
    "ansible.builtin.win_command",
    "ansible.legacy.win_command",
    "shell",
    "ansible.builtin.shell",
    "ansible.legacy.shell",
    "win_shell",
    "ansible.builtin.win_shell",
    "ansible.legacy.win_shell",
    "raw",
    "ansible.builtin.raw",
    "ansible.legacy.raw",
    "ansible.windows.win_command",
    "ansible.windows.win_shell",
];

/// `_command_generic_msg`: the header, then `stdout`, `stderr` and `msg` with nothing between.
/// The line's own newline is the one the reference adds after them.
fn command_text(
    host: &str,
    caption: &str,
    body: &serde_json::Map<String, serde_json::Value>,
) -> String {
    // `%s` of the value, and `''` for a key that is not there.
    let text = |key: &str| match body.get(key) {
        None => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => crate::playbook::python_repr(other),
    };
    let rc = rc_text(body);
    format!(
        "{host} | {caption} | rc={rc} >>
{}{}{}",
        text("stdout"),
        text("stderr"),
        text("msg")
    )
}
