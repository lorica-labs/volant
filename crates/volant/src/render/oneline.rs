// SPDX-License-Identifier: GPL-3.0-or-later
//! The `-o` output: the `oneline` callback of ansible-core 2.19.12.

use std::io::Write;

use volant_protocol::TaskResult;

use super::recap_json::{ansible_json_indented, dumps_indented};
use super::result::{MODULE_NO_JSON, rc_text, verbose_always};
use super::{CHANGED, Dump, FAILED, OK, Renderer, SKIPPED};
use crate::stats::Outcome;

impl Renderer {
    /// One host's line as the `oneline` callback prints it (`plugins/callback/oneline.py`,
    /// ansible-core 2.19.12): the result as JSON on a single line, the command form for a command
    /// action, and for a failure the note about its `exception` in front of the `FAILED!` line.
    /// Unlike `minimal` it cleans nothing, shows no warnings and prints no diff.
    pub(super) fn oneline_line(
        &mut self,
        host: &str,
        outcome: Outcome,
        result: &TaskResult,
        censored: bool,
    ) {
        let body = self.shown_body(result, Dump::No, censored);
        let command = self
            .minimal
            .as_deref()
            .is_some_and(|action| MODULE_NO_JSON.contains(&action));
        // `indent=0` puts every key on a line of its own and the callback joins them again. A
        // result that asks for the pretty form, or `-vvv`, gets four spaces, which stay.
        let json = |body: serde_json::Map<String, serde_json::Value>| {
            let value = serde_json::Value::Object(body);
            if verbose_always(result, censored) || self.verbosity > 2 {
                ansible_json_indented(&value, 0).replace('\n', "")
            } else {
                dumps_indented(&value, 0, 0).replace('\n', "")
            }
        };
        let (style, text) = match outcome {
            Outcome::Skipped => (SKIPPED, format!("{host} | SKIPPED")),
            Outcome::Ok | Outcome::Changed => {
                let (style, state) = if outcome == Outcome::Changed {
                    (CHANGED, "CHANGED")
                } else {
                    (OK, "SUCCESS")
                };
                let text = if command && !result.0.contains_key("ansible_job_id") {
                    oneline_command(host, state, &body)
                } else {
                    format!("{host} | {state} => {}", json(body))
                };
                (style, text)
            }
            Outcome::Failed | Outcome::Rescued | Outcome::Ignored => {
                let text = format!("{host} | FAILED! => {}", json(body.clone()));
                if let Some(exception) = result.0.get("exception") {
                    let note = if command && !result.0.contains_key("module_stderr") {
                        oneline_command(host, "FAILED", &body)
                    } else {
                        self.exception_note(exception)
                    };
                    let _ = writeln!(self.out, "{}", self.paint(FAILED, &note));
                }
                (FAILED, text)
            }
        };
        let _ = writeln!(self.out, "{}", self.paint(style, &text));
    }

    /// What `oneline` says of a failed result's `exception` when the action is not a command: the
    /// last line of it, or from `-vvv` the whole text with its line breaks taken out. A value
    /// that is not text is shown as Python prints it, where the reference fails.
    fn exception_note(&self, exception: &serde_json::Value) -> String {
        let text = match exception {
            serde_json::Value::String(text) => text.clone(),
            other => crate::playbook::python_repr(other),
        };
        if self.verbosity < 3 {
            let last = text.trim().rsplit('\n').next().unwrap_or_default();
            format!(
                "An exception occurred during task execution. To see the full traceback, use -vvv. The error was: {last}"
            )
        } else {
            format!(
                "An exception occurred during task execution. The full traceback is:\n{}",
                text.replace('\n', "")
            )
        }
    }
}

/// `_command_generic_msg` of `oneline`: the header and the result's `stdout`, with `stderr` after
/// it when there is some, their line breaks written out.
fn oneline_command(
    host: &str,
    caption: &str,
    body: &serde_json::Map<String, serde_json::Value>,
) -> String {
    let escaped = |key: &str| match body.get(key) {
        Some(serde_json::Value::String(s)) => s.replace('\n', "\\n").replace('\r', "\\r"),
        _ => String::new(),
    };
    let rc = rc_text(body);
    let stdout = escaped("stdout");
    let stderr = escaped("stderr");
    if stderr.is_empty() {
        format!("{host} | {caption} | rc={rc} | (stdout) {stdout}")
    } else {
        format!("{host} | {caption} | rc={rc} | (stdout) {stdout} (stderr) {stderr}")
    }
}
