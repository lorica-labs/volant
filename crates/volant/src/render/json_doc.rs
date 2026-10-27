// SPDX-License-Identifier: GPL-3.0-or-later
//! The `json` stdout callback, ansible.posix 2.2.2's `ansible.posix.json`: nothing while the run
//! goes, then one JSON document of every play, task and host result, the recap and the custom
//! stats.

use std::fmt::Write as _;
use std::io::Write;

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;

use super::recap_json::{ansible_json, dumps_indented};
use super::result::censored_body;
use super::{LOOP_RESULTS, Renderer};
use crate::playbook::Origin;
use crate::stats::{CustomStats, Outcome, Stats};

/// The document being built, and where it goes once the run ends.
pub(super) struct JsonDoc {
    out: Box<dyn Write>,
    /// `json_indent`: `None` writes the document on one line.
    indent: Option<usize>,
    plays: Vec<Value>,
    /// The action of the task whose results come next, as the task names it.
    action: String,
    /// The ids of plays and tasks: a prefix drawn for the run and a counter, as the reference's
    /// own uuids are.
    prefix: String,
    next_id: u64,
}

fn now() -> String {
    format!("{}Z", crate::executor::utc_timestamp('T'))
}

/// `file:line` made absolute, as `get_path()` gives it.
fn path_of(origin: Option<&Origin>) -> Value {
    origin.map_or(Value::Null, |origin| {
        let path = std::path::absolute(&*origin.path)
            .unwrap_or_else(|_| std::path::PathBuf::from(&*origin.path));
        json!(format!("{}:{}", path.display(), origin.line))
    })
}

impl JsonDoc {
    pub(super) fn new(out: Box<dyn Write>, indent: Option<usize>) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        let prefix = format!(
            "{:08x}-{:04x}-{:04x}-{:04x}",
            (seed as u32) ^ pid,
            (seed >> 32) as u16,
            (seed >> 48) as u16,
            pid as u16
        );
        Self {
            out,
            indent,
            plays: Vec::new(),
            action: String::new(),
            prefix,
            next_id: 0,
        }
    }

    pub(super) fn set_indent(&mut self, indent: Option<usize>) {
        self.indent = indent;
    }

    fn id(&mut self) -> String {
        self.next_id += 1;
        format!("{}-{:012x}", self.prefix, self.next_id)
    }

    fn entry(&mut self, name: &str, origin: Option<&Origin>) -> Value {
        json!({
            "name": name,
            "id": self.id(),
            "path": path_of(origin),
            "duration": {"start": now()},
        })
    }

    fn tasks(&mut self) -> Option<&mut Vec<Value>> {
        self.plays.last_mut()?.get_mut("tasks")?.as_array_mut()
    }

    /// What one host's result adds: the result as the task handed it over, censored under
    /// `no_log`, its items back under `results`, `failed` or `skipped` set by how it ended, and
    /// the action. A loop's aggregate carries no `_ansible_no_log` of its own; each item does,
    /// with its label.
    fn host_result(&self, outcome: Outcome, result: &TaskResult, censored: bool) -> Value {
        let mut body = if censored {
            censored_body(result)
        } else {
            result.0.clone()
        };
        let items = result.0.get(LOOP_RESULTS).filter(|_| !censored).cloned();
        // The reference's `TaskResult` drops both; the callback sets the one that is true.
        body.retain(|k, _| !k.starts_with("_volant_") && k != "failed" && k != "skipped");
        match items {
            Some(Value::Array(items)) => {
                let debug = self.action.rsplit('.').next() == Some("debug");
                let items = items
                    .into_iter()
                    .map(|item| match item {
                        Value::Object(mut item) => {
                            let label = item.get("item").cloned().unwrap_or(Value::Null);
                            item.insert("_ansible_item_label".into(), label);
                            item.insert("_ansible_no_log".into(), Value::Bool(false));
                            if debug {
                                item.insert("_ansible_verbose_always".into(), Value::Bool(true));
                            }
                            Value::Object(item)
                        }
                        other => other,
                    })
                    .collect();
                body.insert("results".into(), Value::Array(items));
            }
            _ => {
                body.insert("_ansible_no_log".into(), Value::Bool(censored));
            }
        }
        match outcome {
            Outcome::Failed | Outcome::Rescued | Outcome::Ignored => {
                body.insert("failed".into(), Value::Bool(true));
                body.entry("exception")
                    .or_insert_with(|| json!("(traceback unavailable)"));
            }
            Outcome::Skipped => {
                body.insert("skipped".into(), Value::Bool(true));
            }
            Outcome::Ok | Outcome::Changed => {}
        }
        body.insert("action".into(), json!(self.action));
        Value::Object(body)
    }

    fn record(&mut self, host: &str, value: Value) {
        let end = now();
        if let Some(task) = self.tasks().and_then(|tasks| tasks.last_mut()) {
            task["hosts"][host] = value;
            task["task"]["duration"]["end"] = json!(end);
        }
        if let Some(play) = self.plays.last_mut() {
            play["play"]["duration"]["end"] = json!(end);
        }
    }

    /// The whole document, printed once: Python's `json.dumps` with sorted keys and
    /// `ensure_ascii`, every character past ASCII written as `\uXXXX`.
    fn finish(&mut self, stats: &Stats, custom: Option<&CustomStats>) {
        let summary: Map<String, Value> = stats
            .hosts()
            .map(|(host, h)| {
                (
                    host.to_string(),
                    json!({
                        "ok": h.ok,
                        "failures": h.failed,
                        "unreachable": h.unreachable,
                        "changed": h.changed,
                        "skipped": h.skipped,
                        "rescued": h.rescued,
                        "ignored": h.ignored,
                    }),
                )
            })
            .collect();
        let (hosts, run) = custom.map_or_else(
            || (Map::new(), Map::new()),
            |custom| {
                let hosts = custom
                    .hosts()
                    .iter()
                    .map(|(host, data)| (host.clone(), Value::Object(data.clone())))
                    .collect();
                (hosts, custom.run().clone())
            },
        );
        let doc = json!({
            "plays": std::mem::take(&mut self.plays),
            "stats": summary,
            "custom_stats": hosts,
            "global_custom_stats": run,
        });
        let text = match self.indent {
            Some(step) => dumps_indented(&doc, 0, step),
            None => ansible_json(&doc),
        };
        let mut ascii = String::with_capacity(text.len());
        for c in text.chars() {
            if c.is_ascii() {
                ascii.push(c);
            } else {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(ascii, "\\u{unit:04x}");
                }
            }
        }
        let _ = writeln!(self.out, "{ascii}");
    }
}

impl Renderer {
    /// A play starts, for the `json` callback.
    pub(super) fn json_play(&mut self, name: &str, origin: Option<&Origin>) {
        if let Some(doc) = &mut self.json {
            let play = doc.entry(name, origin);
            doc.plays.push(json!({"play": play, "tasks": []}));
        }
    }

    /// A task or a handler starts: `action` is the one its results are reported under.
    pub fn task_started(&mut self, name: &str, action: &str, origin: Option<&Origin>) {
        if let Some(doc) = &mut self.json {
            action.clone_into(&mut doc.action);
            let task = doc.entry(name, origin);
            if let Some(tasks) = doc.tasks() {
                tasks.push(json!({"task": task, "hosts": {}}));
            }
        }
    }

    /// Records one host's result for the `json` callback; `false` when there is no document.
    pub(super) fn json_result(
        &mut self,
        host: &str,
        outcome: Outcome,
        result: &TaskResult,
        censored: bool,
    ) -> bool {
        let Some(doc) = &mut self.json else {
            return false;
        };
        let value = doc.host_result(outcome, result, censored);
        doc.record(host, value);
        true
    }

    /// Records an unreachable host for the `json` callback; `false` when there is no document.
    pub(super) fn json_unreachable(&mut self, host: &str, msg: &str) -> bool {
        let Some(doc) = &mut self.json else {
            return false;
        };
        let value = json!({
            "changed": false,
            "msg": msg,
            "unreachable": true,
            "action": doc.action,
        });
        doc.record(host, value);
        true
    }

    /// Prints the document at the recap; `false` when there is none.
    pub(super) fn json_finish(&mut self, stats: &Stats, custom: Option<&CustomStats>) -> bool {
        let Some(doc) = &mut self.json else {
            return false;
        };
        doc.finish(stats, custom);
        true
    }
}
