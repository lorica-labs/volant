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
        match bridge.diff_text(diff) {
            Ok(text) if text.is_empty() => {}
            Ok(text) => {
                let painted: String = text
                    .split_inclusive('\n')
                    .map(|line| match line.as_bytes().first() {
                        Some(b'+') => self.paint(OK, line),
                        Some(b'-') => self.paint(FAILED, line),
                        Some(b'@') if line.starts_with("@@") => self.paint(SKIPPED, line),
                        _ => line.to_string(),
                    })
                    .collect();
                // Measured: the block ends with one blank line, which `_get_diff` already put there.
                let end = if painted.ends_with('\n') { "" } else { "\n" };
                let _ = write!(self.out, "{painted}{end}");
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
        self.print_diff(outcome, result, censored);
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
            if self.verbosity < 3 {
                // The reference's `_dump_results` keeps the diff and the `invocation` off the line
                // of every outcome below `-vvv`; it prints the diff apart. Measured on `ok`,
                // `changed`, `failed` and `skipped` results at `-v` and `-vv`, no module keeps it.
                body.remove("diff");
                body.remove("invocation");
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
}
