// SPDX-License-Identifier: GPL-3.0-or-later
//! What Python callback plugins hear of a run, through the helper of their own. Nothing here runs
//! for a run that loaded none: each entry point first asks whether the renderer has one.

use std::collections::{HashMap, HashSet};
use std::io::Write;

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;

use super::{DIFF_KEY, LOOP_RESULTS, Renderer};
use crate::python::CallbackHelper;
use crate::stats::{CustomStats, Outcome, Stats};

/// The exit code of a run a callback's exception ended, as the reference exits on one the
/// dispatch does not catch.
const CALLBACK_RAISED: i32 = 250;

/// The running helper, and what the events still to come need to know.
pub struct PythonCallbacks {
    helper: CallbackHelper,
    /// The task whose banner was last printed: a result's line is always printed under its task's
    /// banner, so it is that result's task.
    current: Option<Current>,
    /// The tasks already announced, for `free` and `host_pinned`, which announce a task once.
    announced: HashSet<String>,
    /// The hosts each task has started on.
    started: HashMap<String, HashSet<String>>,
    /// The plays whose `vars_prompt` was told, which a later `serial` batch does not tell again.
    prompted: HashSet<String>,
    /// A flush point the compiler put in was passed: a handler next means it flushed.
    flush_passed: bool,
}

struct Current {
    uuid: String,
    /// A `meta` runs on no worker, so no host starts it.
    meta: bool,
}

/// A task starting, as a plugin reads it: ansible-core's attribute names and values.
pub struct PythonTask {
    /// Names the task across `serial` batches, as the reference's task uuid does.
    pub uuid: String,
    pub meta: bool,
    pub fields: Map<String, Value>,
}

impl PythonCallbacks {
    pub fn new(helper: CallbackHelper) -> Self {
        Self {
            helper,
            current: None,
            announced: HashSet::new(),
            started: HashMap::new(),
            prompted: HashSet::new(),
            flush_passed: false,
        }
    }

    /// Asks the helper to load the callbacks; how many it holds.
    ///
    /// # Errors
    /// The sentence the load ended with.
    pub fn load(&mut self, request: &Value) -> Result<u64, String> {
        let answer = self.helper.ask(&json!({ "load": request }))?;
        Ok(answer.get("loaded").and_then(Value::as_u64).unwrap_or(0))
    }
}

impl Renderer {
    /// Sends the events of the run to `python` from here on. When it `owns_stdout` the renderer
    /// prints as the `oneline` callback, which shows no note and no error block, into nothing.
    pub fn set_python_callbacks(&mut self, python: PythonCallbacks, owns_stdout: bool) {
        self.python = Some(python);
        if owns_stdout {
            self.minimal.get_or_insert_with(String::new);
            self.oneline = true;
            self.python_stdout = true;
            self.silence();
        }
    }

    /// Whether anything hears these events, to spare a caller building what it would send.
    pub fn hears_python(&self) -> bool {
        self.python.is_some()
    }

    fn hear(&mut self, request: Value) {
        let Some(python) = &mut self.python else {
            return;
        };
        // What the plugins print follows what is printed here, whichever writes it.
        let _ = self.out.flush();
        if let Err(why) = python.helper.ask(&request) {
            let _ = writeln!(anstream::stderr(), "[ERROR]: {why}");
            std::process::exit(CALLBACK_RAISED);
        }
    }

    pub fn hear_playbook(&mut self, file: &std::path::Path) {
        if self.hears_python() {
            self.hear(json!({"event": "playbook", "file": file}));
        }
    }

    pub(super) fn hear_play(&mut self, play: &crate::playbook::Play) {
        if let Some(python) = &mut self.python {
            python.announced.clear();
            let first = python.prompted.insert(play.name.clone());
            for entry in play.vars_prompt.iter().filter(|_| first) {
                self.hear(json!({"event": "vars_prompt", "fields": prompt_fields(entry)}));
            }
            let fields = json!({
                "name": play.name,
                "hosts": play.host_patterns,
                "strategy": play.strategy,
                "gather_facts": play.gather_facts,
            });
            self.hear(json!({"event": "play", "fields": fields}));
        }
    }

    /// `v2_playbook_on_task_start`, or its handler's. `once` for the banners of `free` and
    /// `host_pinned`, which come back as the hosts take turns: the plugins hear of the task once.
    pub fn hear_task(&mut self, task: PythonTask, once: bool) {
        let Some(python) = &mut self.python else {
            return;
        };
        let current = Current {
            uuid: task.uuid.clone(),
            meta: task.meta,
        };
        python.current = Some(current);
        let flushed = std::mem::take(&mut python.flush_passed)
            && task.fields.get("handler") == Some(&Value::Bool(true));
        if flushed {
            self.hear_implicit("flush_handlers");
        }
        let Some(python) = &mut self.python else {
            return;
        };
        if !python.announced.insert(task.uuid.clone()) && once {
            return;
        }
        python.started.remove(&task.uuid);
        let mut fields = task.fields;
        fields.insert("uuid".into(), task.uuid.into());
        self.hear(json!({"event": "task", "fields": fields}));
    }

    /// A flush point the compiler put in was reached: a handler next means it flushed.
    pub fn hear_flush_point(&mut self) {
        if let Some(python) = &mut self.python {
            python.flush_passed = true;
        }
    }

    /// A `meta` the reference makes itself, which only a plugin that `wants_implicit_tasks` hears.
    pub fn hear_implicit(&mut self, action: &str) {
        let fields = json!({
            "uuid": format!("implicit {action}"),
            "action": "meta",
            "implicit": true,
            "args": {"_raw_params": action},
        });
        self.hear(json!({"event": "task", "fields": fields}));
    }

    /// `v2_runner_on_start`, in front of what a host says first of the current task. Returns the
    /// task's uuid.
    fn start_host(&mut self, host: &str) -> Option<String> {
        let python = self.python.as_mut()?;
        let current = python.current.as_ref()?;
        let (uuid, meta) = (current.uuid.clone(), current.meta);
        let new = python
            .started
            .entry(uuid.clone())
            .or_default()
            .insert(host.to_string());
        if new && !meta {
            self.hear(json!({"event": "runner_start", "host": host, "task": uuid}));
        }
        Some(uuid)
    }

    fn hear_result(&mut self, method: &str, host: &str, data: Map<String, Value>, kwargs: Value) {
        if let Some(uuid) = self.start_host(host) {
            self.hear(json!({
                "event": "result",
                "method": method,
                "host": host,
                "task": uuid,
                "data": data,
                "kwargs": kwargs,
            }));
        }
    }

    /// One task or item result, with `v2_on_file_diff` in front of it when it carries a diff.
    pub(crate) fn hear_outcome(
        &mut self,
        host: &str,
        outcome: Outcome,
        result: &TaskResult,
        label: Option<&str>,
        censored: bool,
    ) {
        if !self.hears_python() {
            return;
        }
        let data = plugin_data(result, label, censored);
        // An include that could not load: the reference heard the include itself succeed first.
        if outcome != Outcome::Skipped && label.is_none() && result.0.contains_key("reason") {
            self.hear_result("v2_runner_on_ok", host, Map::new(), json!({}));
        }
        let asked = result.0.get(DIFF_KEY) == Some(&Value::Bool(true));
        if asked && result.0.contains_key("diff") && outcome != Outcome::Skipped {
            self.hear_result("v2_on_file_diff", host, data.clone(), json!({}));
        }
        let (method, kwargs) = match (outcome, label.is_some()) {
            (Outcome::Ok | Outcome::Changed, false) => ("v2_runner_on_ok", json!({})),
            (Outcome::Ok | Outcome::Changed, true) => ("v2_runner_item_on_ok", json!({})),
            (Outcome::Skipped, false) => ("v2_runner_on_skipped", json!({})),
            (Outcome::Skipped, true) => ("v2_runner_item_on_skipped", json!({})),
            (_, true) => ("v2_runner_item_on_failed", json!({})),
            (other, false) => (
                "v2_runner_on_failed",
                json!({"ignore_errors": other == Outcome::Ignored}),
            ),
        };
        self.hear_result(method, host, data, kwargs);
    }

    pub(crate) fn hear_retry(&mut self, host: &str, result: &TaskResult, censored: bool) {
        if self.hears_python() {
            let data = plugin_data(result, None, censored);
            self.hear_result("v2_runner_retry", host, data, json!({}));
        }
    }

    /// `v2_runner_on_async_poll`, `_ok` or `_failed`.
    pub(crate) fn hear_async(&mut self, host: &str, method: &str, result: &TaskResult) {
        if self.hears_python() {
            let data = plugin_data(result, None, false);
            self.hear_result(method, host, data, json!({}));
        }
    }

    /// `v2_runner_on_unreachable`, after each item that could not be reached.
    pub(crate) fn hear_unreachable(
        &mut self,
        host: &str,
        msg: &str,
        items: &[(Option<String>, Map<String, Value>)],
        censored: bool,
    ) {
        if !self.hears_python() {
            return;
        }
        for (label, item) in items {
            let item = TaskResult(item.clone());
            let data = plugin_data(&item, label.as_deref(), censored);
            self.hear_result("v2_runner_item_on_failed", host, data, json!({}));
        }
        let mut data = Map::new();
        data.insert("msg".into(), msg.into());
        data.insert("unreachable".into(), true.into());
        data.insert("changed".into(), false.into());
        if censored {
            data.insert("_ansible_no_log".into(), true.into());
        }
        self.hear_result("v2_runner_on_unreachable", host, data, json!({}));
    }

    /// `v2_playbook_on_notify`, from the line the `default` callback prints for it.
    pub(crate) fn hear_notified(&mut self, line: &str) {
        if let Some((handler, host)) = line
            .strip_prefix("NOTIFIED HANDLER ")
            .and_then(|rest| rest.rsplit_once(" for "))
            && self.hears_python()
        {
            self.hear(json!({"event": "notify", "handler": handler, "host": host}));
        }
    }

    /// An include: its own result for each host, then `v2_playbook_on_include`.
    pub(super) fn hear_included(&mut self, what: &str, hosts: &[String], label: Option<&str>) {
        if !self.hears_python() {
            return;
        }
        let role = !std::path::Path::new(what).is_absolute();
        let key = if role { "include_role" } else { "include" };
        for host in hosts {
            let mut data = Map::new();
            data.insert(key.into(), what.into());
            let method = if label.is_some() {
                "v2_runner_item_on_ok"
            } else {
                "v2_runner_on_ok"
            };
            self.hear_result(method, host, data, json!({}));
        }
        if let Some(uuid) = self
            .python
            .as_ref()
            .and_then(|p| p.current.as_ref())
            .map(|c| c.uuid.clone())
        {
            self.hear(json!({
                "event": "included",
                "file": what,
                "role": role,
                "hosts": hosts,
                "task": uuid,
            }));
        }
    }

    pub(super) fn hear_no_hosts_matched(&mut self) {
        if self.hears_python() {
            self.hear(json!({"event": "no_hosts_matched"}));
        }
    }

    pub(super) fn hear_no_hosts_remaining(&mut self) {
        if self.hears_python() {
            self.hear(json!({"event": "no_hosts_remaining"}));
        }
    }

    pub(super) fn hear_stats(&mut self, stats: &Stats, custom: Option<&CustomStats>) {
        if !self.hears_python() {
            return;
        }
        let hosts: Map<String, Value> = stats
            .hosts()
            .map(|(host, h)| {
                let counts = json!({
                    "ok": h.ok,
                    "changed": h.changed,
                    "dark": h.unreachable,
                    "failures": h.failed,
                    "skipped": h.skipped,
                    "rescued": h.rescued,
                    "ignored": h.ignored,
                });
                (host.to_string(), counts)
            })
            .collect();
        let custom = custom.map_or_else(Map::new, |custom| {
            let mut all: Map<String, Value> = custom
                .hosts()
                .iter()
                .map(|(host, stats)| (host.clone(), Value::Object(stats.clone())))
                .collect();
            if !custom.run().is_empty() {
                all.insert("_run".into(), Value::Object(custom.run().clone()));
            }
            all
        });
        self.hear(json!({"event": "stats", "hosts": hosts, "custom": custom}));
    }
}

/// The arguments of `v2_playbook_on_vars_prompt` for one entry of `vars_prompt`.
fn prompt_fields(entry: &Map<String, Value>) -> Map<String, Value> {
    let get = |key: &str| entry.get(key).cloned().unwrap_or(Value::Null);
    let mut fields = Map::new();
    fields.insert("varname".into(), get("name"));
    for key in [
        "private",
        "prompt",
        "encrypt",
        "confirm",
        "salt_size",
        "salt",
        "default",
        "unsafe",
    ] {
        fields.insert(key.into(), get(key));
    }
    fields
}

/// A result as ansible-core's strategy holds it: the engine's own keys gone, a loop's items under
/// `results`, and `no_log` and the item's label as the keys a plugin's result is made from.
fn plugin_data(result: &TaskResult, label: Option<&str>, censored: bool) -> Map<String, Value> {
    let mut data = result.0.clone();
    if let Some(items) = data.remove(LOOP_RESULTS) {
        data.insert("results".into(), items);
    }
    data.retain(|key, _| !key.starts_with("_volant_"));
    if censored {
        data.insert("_ansible_no_log".into(), true.into());
    }
    if let Some(label) = label {
        data.insert("_ansible_item_label".into(), label.into());
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plugin_reads_a_result_as_the_strategy_holds_it() {
        let mut result = Map::new();
        result.insert("changed".into(), true.into());
        result.insert("_volant_diff".into(), true.into());
        result.insert(LOOP_RESULTS.into(), json!([1]));
        let data = plugin_data(&TaskResult(result), Some("x"), true);
        assert_eq!(
            Value::Object(data),
            json!({
                "changed": true,
                "results": [1],
                "_ansible_no_log": true,
                "_ansible_item_label": "x",
            })
        );
    }
}
