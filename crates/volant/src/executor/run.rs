// SPDX-License-Identifier: GPL-3.0-or-later
//! Running one task and judging its result, on the controller as well as on the agent.

use std::collections::{BTreeSet, HashMap};
use std::io::IsTerminal as _;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::watch;
use volant_protocol::modules::{
    ASSERT, CHECK_MODE_ARG, DIFF_ARG, FAIL, ModuleSpec, PAUSE, short_name,
};
use volant_protocol::{BatchOutcome, FromAgent, StagedFile, Task, TaskResult, ToAgent};

use crate::agent::{AgentLink, AgentSource, BlobFrame, BlobMemory};
use crate::compile::{Compiled, Step};
use crate::playbook::PlayTask;
use crate::preflight::PROMPT_REFUSED;
use crate::python::{ModulePayload, Union};
use crate::stats::Outcome;
use crate::template::{Templar, TemplateError};
use crate::transport::{ConnectError, ConnectionDefaults, Escalation, Transport};
use crate::vars::{HostVars, VarStore, load_vars_file};

use super::prepare::{Item, display};
use super::{CANCEL_GRACE, LinkKey, Reboots, RunOptions, as_bool_value, python_type};

/// What a `debug: var:` naming a value that came from a managed host reports. The reference's
/// own sentence, measured on ansible-core 2.19.12 against `var: "{{ r.stdout }}"` and against
/// `var: "{{ item }}"` over a registered list: both fail the task with this `msg`, the second
/// once per item.
const UNTRUSTED_VAR: &str = "Task failed: Error while resolving `var` expression: Encountered untrusted template or expression.";

/// The help the reference's error block gives under that refusal's last cause.
const UNTRUSTED_HELP: &str = "Templates and expressions must be defined by trusted sources such as playbooks or roles, not untrusted sources such as module results.";

/// `include_vars`, run where every other controller-side module runs.
///
/// Measured on ansible-core 2.19.12: a file that is there sets each of its keys as a fact and
/// answers `{"ansible_facts": {...}, "ansible_included_var_files": ["<abs>"], "changed": false}`,
/// a `name:` puts the whole mapping under that one key, and a file that is nowhere fails the task
/// with the paths it looked in spelled out one per line.
///
/// A deliberate divergence, recorded rather than hidden: these land as facts, which is precedence
/// 20 here against the reference's own rank 19 for `include_vars`. The two differ only for a name
/// a host variable also carries. They land once the task is judged, through
/// [`record_local_facts`].
fn run_include_vars(item: &Item, step: &Step, store: &Mutex<VarStore>) -> TaskResult {
    let Some(name) = item
        .args
        .get("file")
        .or_else(|| item.args.get("_raw_params"))
        .and_then(Value::as_str)
    else {
        return TaskResult::failed_with("Task failed: 'include_vars' takes a file name");
    };
    let playbook_dir = store
        .lock()
        .expect("vars lock")
        .playbook_dir()
        .to_path_buf();
    let searched = crate::vars::include_vars_paths(
        &step.origin.file_dir,
        step.origin.role_dir.as_deref(),
        &playbook_dir,
        name,
    );
    let Some(path) = searched.iter().find(|p| p.is_file()) else {
        // The reference's own wording and its own shape, measured: `ansible_facts` and
        // `ansible_included_var_files` are both present and empty, the paths are listed one per
        // tab-indented line, and `msg` carries the sentence a task that died rather than failed
        // gets.
        let mut body = Map::new();
        body.insert("ansible_facts".into(), json!({}));
        body.insert("ansible_included_var_files".into(), json!([]));
        body.insert("changed".into(), json!(false));
        body.insert(
            "message".into(),
            json!(format!(
                "Could not find or access '{name}'\nSearched in:\n{} on the Ansible Controller.\nIf you are using a module and expect the file to exist on the remote, see the remote_src option",
                searched
                    .iter()
                    .map(|p| format!("\t{}", p.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            )),
        );
        body.insert(
            "msg".into(),
            json!("Task failed: Action failed: Unknown error."),
        );
        body.insert("failed".into(), json!(true));
        return TaskResult(body);
    };
    let loaded = match load_vars_file(path) {
        Ok(map) => map,
        // Measured: the reason travels as `message`, and `msg` is the sentence of a task that
        // died, with the facts and the file list empty.
        Err(err) => {
            if let Some(vault) = err.downcast_ref::<crate::vars::VaultFile>() {
                return TaskResult(
                    json!({
                        "ansible_facts": {},
                        "ansible_included_var_files": [],
                        "changed": false,
                        "failed": true,
                        "message": vault.reason,
                        "msg": "Task failed: Action failed: Unknown error.",
                    })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                );
            }
            return TaskResult::failed_with(format!("Task failed: {err:#}"));
        }
    };
    let facts = match item.args.get("name").and_then(Value::as_str) {
        Some(under) => {
            let mut one = Map::new();
            one.insert(under.to_string(), Value::Object(loaded));
            one
        }
        None => loaded,
    };
    let mut r = Map::new();
    r.insert("ansible_facts".into(), Value::Object(facts));
    r.insert(
        "ansible_included_var_files".into(),
        json!([path.display().to_string()]),
    );
    r.insert("changed".into(), json!(false));
    r.insert("failed".into(), json!(false));
    TaskResult(r)
}

/// The modules in `LOCAL_MODULES` never leave the controller. `stop` is the run's interruption,
/// which a `pause` waits on beside its timer: `None` is a pause the run's stop ended, which the
/// driver answers the way it answers every other wait the stop ends, with no task line and no
/// count.
pub(super) async fn run_local(
    task: &PlayTask,
    item: &Item,
    step: &Step,
    templar: &Templar,
    store: &Mutex<VarStore>,
    verbosity: u8,
    stop: &mut watch::Receiver<bool>,
) -> Option<TaskResult> {
    let mut result = if short_name(&task.module) == "pause" {
        pause(
            &task.module,
            &task.name,
            item,
            std::io::stdin().is_terminal(),
            stop,
        )
        .await?
    } else if crate::action_plugins::controller_side(&task.module) {
        controller_side(task, item, step, templar, stop).await?
    } else {
        local_result(task, item, step, templar, store, verbosity)
    };
    // The reference's `maybe_raise_on_result` normalises every task's final result, wherever it
    // ran, the way `run_agent_batch` normalises an agent's.
    summarise_exception(&mut result);
    Some(result)
}

/// An action plugin that never touches the host (`add_host`, `group_by`, `set_stats`), run as
/// ansible-core's own with no link: the reference runs these on the controller and never
/// connects. What the plugin asks of a host is answered with a failure it reports itself.
/// `None` when the run's stop came first.
async fn controller_side(
    task: &PlayTask,
    item: &Item,
    step: &Step,
    templar: &Templar,
    stop: &mut watch::Receiver<bool>,
) -> Option<TaskResult> {
    use crate::action_plugins::{Context, Step as Next, kind, start};
    let Some(kind) = kind(&task.module) else {
        return Some(TaskResult::failed_with(format!(
            "no action plugin runs '{}'",
            task.module
        )));
    };
    let mut warnings = Vec::new();
    let mut plugin = start(
        kind,
        Context {
            args: &item.args,
            args_untrusted: &item.args_untrusted,
            running_vars: &item.vars,
            delegated: false,
            escalated: false,
            local: true,
            item_vars: &item.vars,
            templar,
            origin: &step.origin,
            playbook_dir: std::path::Path::new("."),
            warnings: &mut warnings,
            environment: Some(&item.environment),
            timeout: task.timeout,
            check_mode: item.check_mode == Some(true),
            diff: item.diff == Some(true),
        },
    );
    let mut last = None;
    loop {
        match plugin.next(last.take()) {
            Next::Done(result) => return Some(result),
            Next::Wait(ready) => {
                tokio::select! {
                    () = ready.notified() => {}
                    stopped = stop.wait_for(|stopped| *stopped) => {
                        if stopped.is_ok() {
                            return None;
                        }
                    }
                }
            }
            _ => {
                last = Some(TaskResult::failed_with(format!(
                    "'{}' runs on the controller and cannot reach a host",
                    task.module
                )));
            }
        }
    }
}

/// The names a `set_fact` sets, each with its value and whether it is data from a managed host.
///
/// Its keys are rendered, as ansible-core's `set_fact` action renders them ("a rare case where
/// key templating is allowed"), and each rendered name must be a variable name: a string that
/// is an ASCII identifier and not one of Jinja's `true`/`false`/`none` (either case) or `not`.
/// Measured on ansible-core 2.19.12: `{"k_{{ n }}": 1}` sets `k_dyn`; a name rendering to
/// `bad-name`, `''` or a literal `bad-lit` fails with `Task failed: Invalid variable name
/// 'bad-name'.`; one rendering to `1` with `Invalid variable name '1' of type 'int'.`; an
/// undefined name with `Task failed: ` and the render's error; `if` is a valid name; and nothing
/// but `cacheable` fails with `No key/value pairs provided, at least one is required for this
/// action to succeed`, without the prefix. A name is data when its value was, or when its own
/// render read a managed host.
///
/// Rendered once per item and kept on it, so that every reader - the module itself, the
/// conditions, the facts written after the task - sees the same names with the same trust: a key
/// built from `random` or a `lookup('pipe', ...)` renders differently each time, and matching a
/// second render against the first would lose which name holds a managed host's value.
pub(super) fn set_fact_names<'a>(
    item: &'a Item,
    templar: &Templar,
) -> Result<Vec<(String, &'a Value, bool)>, TaskResult> {
    let rendered = item.set_fact_names.get_or_init(|| {
        let mut names = Vec::new();
        for key in item.args.keys().filter(|k| *k != "cacheable") {
            let (name, tainted) = templar
                .render_value_tainted(&Value::String(key.clone()), &item.vars)
                .map_err(|e| TaskResult::failed_with(format!("Task failed: {}", e.0)))?;
            if let Some(why) = invalid_variable_name(&name) {
                return Err(TaskResult::failed_with(format!("Task failed: {why}")));
            }
            let Value::String(name) = name else {
                unreachable!("a valid variable name is a string")
            };
            names.push((
                key.clone(),
                name,
                tainted || item.args_untrusted.contains(key),
            ));
        }
        if names.is_empty() {
            return Err(TaskResult::failed_with(
                "No key/value pairs provided, at least one is required for this action to succeed",
            ));
        }
        Ok(names)
    });
    let names = rendered.as_ref().map_err(Clone::clone)?;
    Ok(names
        .iter()
        .map(|(key, name, data)| (name.clone(), &item.args[key], *data))
        .collect())
}

/// The names a `set_fact` item sets that are data, as [`set_fact_names`] renders them; empty for
/// any other module.
fn set_fact_data(task: &PlayTask, item: &Item, templar: &Templar) -> BTreeSet<String> {
    if short_name(&task.module) != "set_fact" {
        return BTreeSet::new();
    }
    set_fact_names(item, templar)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, _, data)| *data)
        .map(|(name, _, _)| name)
        .collect()
}

/// ansible-core's `validate_variable_name`: why this value cannot name a variable, or `None`.
fn invalid_variable_name(name: &Value) -> Option<String> {
    let valid = |s: &str| {
        s.chars()
            .next()
            .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
            && s.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
            && !["true", "false", "none", "True", "False", "None", "not"].contains(&s)
    };
    let (shown, kind) = match name {
        Value::String(s) if valid(s) => return None,
        Value::String(s) => (format!(" '{s}'"), ""),
        Value::Number(n) if n.is_f64() => (format!(" '{n}'"), "float"),
        Value::Number(n) => (format!(" '{n}'"), "int"),
        Value::Bool(b) => (format!(" '{}'", if *b { "True" } else { "False" }), "bool"),
        Value::Null => (" 'None'".to_string(), "NoneType"),
        Value::Array(_) => (String::new(), "list"),
        Value::Object(_) => (String::new(), "dict"),
    };
    let of = if kind.is_empty() {
        String::new()
    } else {
        format!(" of type '{kind}'")
    };
    Some(format!("Invalid variable name{shown}{of}."))
}

/// Every controller-side module but `pause`, none of which waits.
#[expect(clippy::too_many_lines)]
fn local_result(
    task: &PlayTask,
    item: &Item,
    step: &Step,
    templar: &Templar,
    store: &Mutex<VarStore>,
    verbosity: u8,
) -> TaskResult {
    let mut r = Map::new();
    match short_name(&task.module) {
        "assert" => return assert_module(task, item, templar),
        "fail" => {
            let unknown = unknown_args(&FAIL, &item.args);
            if !unknown.is_empty() {
                return unchanged_failure(format!(
                    "Invalid options for {}: {}",
                    action_name(&task.module, "ansible.builtin.fail"),
                    unknown.join(", ")
                ));
            }
            r.insert("changed".into(), json!(false));
            r.insert("failed".into(), json!(true));
            r.insert(
                "msg".into(),
                item.args
                    .get("msg")
                    .cloned()
                    .unwrap_or_else(|| json!("Failed as requested from task")),
            );
        }
        "include_vars" => return run_include_vars(item, step, store),
        "set_fact" => {
            // Written by `record_local_facts` once the task is judged.
            let names = match set_fact_names(item, templar) {
                Ok(names) => names,
                Err(failed) => return failed,
            };
            let facts: Map<String, Value> = names
                .into_iter()
                .map(|(name, value, _)| (name, value.clone()))
                .collect();
            r.insert("ansible_facts".into(), Value::Object(facts));
            r.insert("changed".into(), json!(false));
            r.insert("failed".into(), json!(false));
        }
        "debug" => {
            // The action refuses both before it looks at either, which is how a `module_defaults`
            // `msg` turns a task's own `var` into a failure.
            if item.args.contains_key("msg") && item.args.contains_key("var") {
                let msg = "parameters are mutually exclusive: msg|var";
                let mut failed = TaskResult::failed_with(msg);
                failed.0.insert(
                    crate::render::ERROR_KEY.into(),
                    Value::from(crate::render::task_failed(msg)),
                );
                return failed;
            }
            let wanted: u8 = item
                .args
                .get("verbosity")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u8;
            if wanted > verbosity {
                r.insert("skipped".into(), json!(true));
                r.insert(
                    "skipped_reason".into(),
                    json!("Verbosity threshold not met."),
                );
                r.insert("changed".into(), json!(false));
                return TaskResult(r);
            }
            if let Some(var) = item.args.get("var").and_then(Value::as_str) {
                // `var` is the one argument this engine reads back as source text: it names an
                // expression and that expression is compiled here. A value a managed host put
                // there is data, and compiling data is how a remote string runs code on the
                // controller. Refused with the reference's own sentence, measured on
                // ansible-core 2.19.12 for both the plain form and the loop form.
                if item.args_untrusted.contains("var") {
                    let at = task.value_origins.get("var");
                    let mut refused = TaskResult::failed_with(UNTRUSTED_VAR);
                    refused.0.insert(
                        crate::render::CHAIN_KEY.into(),
                        json!([
                            crate::render::cause(
                                "Error while resolving `var` expression.",
                                at,
                                None
                            ),
                            crate::render::cause(
                                "Encountered untrusted template or expression.",
                                at,
                                Some(UNTRUSTED_HELP),
                            ),
                        ]),
                    );
                    return refused;
                }
                match templar.evaluate(var, &item.vars) {
                    Ok(v) => r.insert(var.to_string(), v),
                    Err(e) => r.insert(
                        var.to_string(),
                        Value::String(format!("VARIABLE IS NOT DEFINED!: {e}")),
                    ),
                };
            } else {
                r.insert(
                    "msg".into(),
                    item.args
                        .get("msg")
                        .cloned()
                        .unwrap_or_else(|| json!("Hello world!")),
                );
            }
            // The callback never prints these for a `debug`, but `register` stores them, so a
            // later `when: reg.changed` or `when: not reg.failed` has something to read. The
            // renderer drops them again on the way out.
            r.insert("changed".into(), json!(false));
            r.insert("failed".into(), json!(false));
            r.insert(crate::render::VERBOSE_ALWAYS.into(), json!(true));
        }
        "validate_argument_spec" => return validate_argument_spec(item),
        other => {
            return TaskResult::failed_with(format!("{other} is not a controller-side module"));
        }
    }
    TaskResult(r)
}

/// An action's refusal of its arguments, measured on ansible-core 2.19.12 for `assert`, `fail`
/// and `pause`: `changed: false`, and an error block that says `Task failed: ` and the message,
/// with no `Action failed: ` between, since the action raised it before it ran.
fn unchanged_failure(msg: impl Into<String>) -> TaskResult {
    let msg = msg.into();
    let mut failed = raised(&msg, Vec::new());
    failed.0.insert(
        crate::render::ERROR_KEY.into(),
        Value::from(crate::render::task_failed(&msg)),
    );
    failed
}

/// The name an action's refusal gives it, measured on ansible-core 2.19.12: the plugin's full
/// import path under `ansible.builtin.`, the bare name written bare or under `ansible.legacy.`.
fn action_name(module: &str, long: &str) -> String {
    if module.starts_with("ansible.builtin.") {
        long.to_string()
    } else {
        short_name(module).to_string()
    }
}

/// The names a task wrote that the module does not have, sorted.
fn unknown_args<'a>(spec: &ModuleSpec, args: &'a Map<String, Value>) -> Vec<&'a str> {
    let mut unknown: Vec<&str> = args
        .keys()
        .map(String::as_str)
        .filter(|key| !spec.args.iter().any(|a| a.name == *key))
        .collect();
    unknown.sort_unstable();
    unknown
}

/// The reference's refusal of an argument its `assert` or `pause` action plugin does not have,
/// measured on ansible-core 2.19.12. It lists aliases apart, in brackets after the names.
fn unsupported(
    module: &str,
    spec: &ModuleSpec,
    aliases: &[&str],
    args: &Map<String, Value>,
) -> Option<String> {
    let unknown = unknown_args(spec, args);
    if unknown.is_empty() {
        return None;
    }
    let names: Vec<&str> = spec
        .args
        .iter()
        .map(|a| a.name)
        .filter(|name| !aliases.contains(name))
        .collect();
    let aliases = if aliases.is_empty() {
        String::new()
    } else {
        format!(" ({})", aliases.join(", "))
    };
    Some(format!(
        "Unsupported parameters for ({}) module: {}. Supported parameters include: {}{aliases}.",
        action_name(
            module,
            &format!(
                "ansible_collections.ansible.builtin.plugins.action.{}",
                spec.name
            )
        ),
        unknown.join(", "),
        names.join(", ")
    ))
}

/// `assert`, measured on ansible-core 2.19.12: the conditions are evaluated in order, the first
/// false one is reported as the playbook wrote it, a string or a boolean, and an undefined name
/// fails the task the way any other conditional does.
///
/// `that` is read from the task as the playbook wrote it, never as `prepare` rendered it, and
/// each element goes through `Templar::condition`, the code `when` goes through: rendering it
/// first and evaluating the result would evaluate a value rather than the author's expression.
/// The one render is the reference's own: a `that` that is one template naming a list is that
/// list. A list a managed host wrote is data, so its strings are refused rather than compiled.
fn assert_module(task: &PlayTask, item: &Item, templar: &Templar) -> TaskResult {
    if let Some(refusal) = unsupported(&task.module, &ASSERT, &["msg"], &item.args) {
        return unchanged_failure(refusal);
    }
    let Some(that) = task.args.get("that").filter(|v| !v.is_null()) else {
        return TaskResult::failed_with("missing required arguments: that");
    };
    let conditions = match that {
        Value::Array(list) => list.clone(),
        Value::String(s) if Templar::is_template(s) => {
            match templar.render_value_tainted(that, &item.vars) {
                Ok((Value::Array(list), from_host)) => {
                    if from_host && list.iter().any(Value::is_string) {
                        return TaskResult::failed_with(conditional_error(&TemplateError(
                            "Encountered untrusted template or expression.".into(),
                        )));
                    }
                    list
                }
                _ => vec![that.clone()],
            }
        }
        one => vec![one.clone()],
    };
    let mut r = Map::new();
    r.insert("changed".into(), json!(false));
    // `quiet` is the one thing that argument does: measured on ansible-core 2.19.12.
    if item
        .args
        .get("quiet")
        .and_then(volant_protocol::modules::arg_bool)
        != Some(true)
    {
        r.insert(crate::render::VERBOSE_ALWAYS.into(), json!(true));
    }
    for condition in &conditions {
        // A YAML boolean reaches `condition` spelled the way `when` spells one.
        let text = match condition {
            Value::String(s) => s.clone(),
            Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
            other => other.to_string(),
        };
        match templar.condition(&text, &item.vars) {
            Err(e) => return TaskResult::failed_with(conditional_error(&e)),
            Ok(true) => {}
            Ok(false) => {
                let msg = item.args.get("fail_msg").or_else(|| item.args.get("msg"));
                r.insert("assertion".into(), condition.clone());
                r.insert("evaluated_to".into(), json!(false));
                r.insert("failed".into(), json!(true));
                r.insert(
                    "msg".into(),
                    msg.cloned().unwrap_or_else(|| json!("Assertion failed")),
                );
                return TaskResult(r);
            }
        }
    }
    r.insert(
        "msg".into(),
        item.args
            .get("success_msg")
            .cloned()
            .unwrap_or_else(|| json!("All assertions passed")),
    );
    TaskResult(r)
}

/// How long a `pause` waits and in which unit it was asked, `None` for a prompt with no
/// duration, or the failure its arguments get.
fn pause_duration(module: &str, item: &Item) -> Result<Option<(i64, &'static str)>, TaskResult> {
    if let Some(refusal) = unsupported(module, &PAUSE, &[], &item.args) {
        return Err(unchanged_failure(refusal));
    }
    let whole = |v: &Value| match v {
        Value::Number(n) => n.as_f64().map(|f| f.trunc() as i64),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    };
    let asked = match (item.args.get("minutes"), item.args.get("seconds")) {
        (Some(_), Some(_)) => {
            return Err(TaskResult::failed_with(
                "parameters are mutually exclusive: minutes|seconds",
            ));
        }
        (Some(v), None) => whole(v).map(|m| (m.saturating_mul(60), "minutes")).ok_or(v),
        (None, Some(v)) => whole(v).map(|s| (s, "seconds")).ok_or(v),
        (None, None) => Err(&Value::Null),
    };
    let asked = match asked {
        Ok(asked) => Some(asked),
        Err(Value::Null) => None,
        Err(v) => {
            return Err(TaskResult::failed_with(format!(
                "non-integer value given for prompt duration: {v}"
            )));
        }
    };
    // Python's clock counts nanoseconds in a signed 64-bit integer, and the reference fails a
    // pause it cannot hold rather than wait it out. Measured on ansible-core 2.19.12.
    if asked.is_some_and(|(seconds, _)| seconds > i64::MAX / 1_000_000_000) {
        return Err(TaskResult::failed_with(PYTIME_OUT_OF_RANGE));
    }
    Ok(asked)
}

/// What a `pause` with a duration prints on stdout before it waits, measured on ansible-core
/// 2.19.12: `Pausing for N seconds`, at least one, `(output is hidden)` behind it under `echo:
/// false`, and the interrupt keys, ending in `\r`, when it also has a `prompt`.
pub(super) fn pause_lines(module: &str, item: &Item) -> Vec<String> {
    let Ok(Some((seconds, _))) = pause_duration(module, item) else {
        return Vec::new();
    };
    let echo = item
        .args
        .get("echo")
        .and_then(volant_protocol::modules::arg_bool)
        .unwrap_or(true);
    let hidden = if echo { "" } else { " (output is hidden)" };
    let mut lines = vec![format!("Pausing for {} seconds{hidden}", seconds.max(1))];
    if item.args.get("prompt").is_some_and(|p| !python_falsy(p)) {
        lines.push("(ctrl+C then 'C' = continue early, ctrl+C then 'A' = abort)\r".to_string());
    }
    lines
}

/// `pause`, measured on ansible-core 2.19.12: a duration under one second waits one, `delta` is
/// the whole seconds waited, and `stdout` gives the time waited to two decimals, in minutes
/// unless `seconds` was the argument. A pause that asks for an answer, through `prompt` or by
/// having no duration at all, warns and goes on at once when standard input is not a terminal.
///
/// On a terminal a pause with no duration asks, as the reference does (`[name]`, the prompt,
/// the answer in `user_input`). One that has a duration and a `prompt` is refused: nothing here
/// waits for the answer and the time at once. `interactive` is passed in so that both sides of
/// that can be tested. `None` is the run's interruption, which ends the wait early.
async fn pause(
    module: &str,
    name: &str,
    item: &Item,
    interactive: bool,
    stop: &mut watch::Receiver<bool>,
) -> Option<TaskResult> {
    let asked = match pause_duration(module, item) {
        Ok(asked) => asked,
        Err(refused) => return Some(refused),
    };
    if interactive && asked.is_some() && item.args.contains_key("prompt") {
        return Some(TaskResult::failed_with(PROMPT_REFUSED));
    }
    let echo = item
        .args
        .get("echo")
        .and_then(volant_protocol::modules::arg_bool)
        .unwrap_or(true);
    let mut r = Map::new();
    r.insert("start".into(), json!(timestamp()));
    let began = std::time::Instant::now();
    let mut user_input = String::new();
    match asked {
        Some((seconds, _)) => {
            let wait = Duration::from_secs(seconds.max(1).unsigned_abs());
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = interrupted(stop) => return None,
            }
        }
        None if interactive => {
            let prompt = match item.args.get("prompt") {
                Some(p) if !python_falsy(p) => {
                    p.as_str().map_or_else(|| p.to_string(), str::to_string)
                }
                _ => "Press enter to continue, Ctrl+C to interrupt".to_string(),
            };
            let hidden = if echo { "" } else { " (output is hidden)" };
            let message = format!(
                "[{}]
{prompt}{hidden}:",
                name.trim()
            );
            let read =
                tokio::task::spawn_blocking(move || crate::prompt::at_terminal(&message, !echo));
            let answer = tokio::select! {
                answer = read => answer,
                () = interrupted(stop) => return None,
            };
            match answer {
                Ok(Ok(line)) => user_input = line,
                Ok(Err(e)) => return Some(TaskResult::failed_with(format!("{e:#}"))),
                Err(e) => return Some(TaskResult::failed_with(format!("reading the answer: {e}"))),
            }
        }
        None => {
            r.insert(
                "warnings".into(),
                json!(["Not waiting for response to prompt as stdin is not interactive"]),
            );
        }
    }
    let waited = began.elapsed();
    let unit = asked.map_or("minutes", |(_, unit)| unit);
    let shown = waited.as_secs_f64() / if unit == "minutes" { 60.0 } else { 1.0 };
    let shown = (shown * 100.0).round() / 100.0;
    // Python prints a whole float with one decimal, `1.0`, and any other the shortest way.
    let shown = if shown.fract() == 0.0 {
        format!("{shown:.1}")
    } else {
        shown.to_string()
    };
    r.insert("changed".into(), json!(false));
    r.insert("delta".into(), json!(waited.as_secs()));
    r.insert("echo".into(), json!(echo));
    r.insert("rc".into(), json!(0));
    r.insert("stderr".into(), json!(""));
    r.insert("stdout".into(), json!(format!("Paused for {shown} {unit}")));
    r.insert("stop".into(), json!(timestamp()));
    r.insert("user_input".into(), json!(user_input));
    Some(TaskResult(r))
}

/// Resolves when the run is interrupted, and never once nothing is left that could interrupt it.
async fn interrupted(stop: &mut watch::Receiver<bool>) {
    let gone = stop.wait_for(|stopped| *stopped).await.is_err();
    if gone {
        std::future::pending::<()>().await;
    }
}

/// Now, in the shape the reference's `pause` gives `start` and `stop`. UTC where the reference
/// uses local time, as the agent's own timestamps are.
fn timestamp() -> String {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = since.as_secs() as i64;
    // Days since 1970-01-01 to a Gregorian date, Howard Hinnant's algorithm.
    let z = secs.div_euclid(86_400) + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let day = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:06}",
        day / 3600,
        (day % 3600) / 60,
        day % 60,
        since.subsec_micros()
    )
}

/// The argument check a role with a `meta/argument_specs.yml` gets in front of it.
///
/// Where the arguments come from is measured, not assumed: a value the role entry never named
/// is still found when the host has a variable of that name (a `-e` on the command line passes
/// the check), while a name that is **not** in the spec is refused only when the entry wrote it,
/// because every host carries hundreds of variables the spec has never heard of.
///
/// The four checks run in the order the reference runs them, measured by asking for all four
/// failures at once: missing required arguments first, then types, then choices, then the
/// arguments the spec does not have. Each sentence is the reference's own.
///
/// What is **not** checked here, and is written down in the record rather than left to be
/// discovered: `aliases`, `default`, sub-options (`options` inside an option), `mutually_
/// exclusive` and the rest of the spec's vocabulary. A converted value is not written back
/// either, which is what the reference does too - measured, a role reading `count: "3"` against
/// a `type: int` sees the string it was given.
fn validate_argument_spec(item: &Item) -> TaskResult {
    let spec = match item.args.get("argument_spec") {
        Some(Value::Object(spec)) => spec.clone(),
        _ => Map::new(),
    };
    let provided = match item.args.get("provided_arguments") {
        Some(Value::Object(provided)) => provided.clone(),
        _ => Map::new(),
    };
    let mut errors: Vec<String> = Vec::new();
    // Every option, with the value in force for it: the entry's own first, then whatever the
    // host can see under that name.
    let value_of =
        |name: &str| -> Option<&Value> { provided.get(name).or_else(|| item.vars.get(name)) };
    let mut missing: Vec<&String> = spec
        .iter()
        .filter(|(name, option)| {
            option
                .get("required")
                .and_then(as_bool_value)
                .unwrap_or(false)
                && !matches!(value_of(name), Some(v) if !v.is_null())
        })
        .map(|(name, _)| name)
        .collect();
    missing.sort();
    if !missing.is_empty() {
        errors.push(format!(
            "missing required arguments: {}",
            missing
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for (name, option) in &spec {
        let Some(value) = value_of(name) else {
            continue;
        };
        if let Some(wanted) = option.get("type").and_then(Value::as_str)
            && let Err(err) = converts(value, wanted)
        {
            errors.push(format!(
                "argument '{name}' is of type {} and we were unable to convert to {wanted}: {err}",
                python_type(value)
            ));
        }
    }
    for (name, option) in &spec {
        let (Some(Value::Array(choices)), Some(value)) = (option.get("choices"), value_of(name))
        else {
            continue;
        };
        if !choices.iter().any(|c| c == value) {
            errors.push(format!(
                "value of {name} must be one of: {}, got: {}",
                choices.iter().map(display).collect::<Vec<_>>().join(", "),
                display(value)
            ));
        }
    }
    let mut supported: Vec<&str> = spec.keys().map(String::as_str).collect();
    supported.sort_unstable();
    for name in provided.keys() {
        if !spec.contains_key(name) {
            errors.push(format!(
                "{name}. Supported parameters include: {}.",
                supported.join(", ")
            ));
        }
    }
    let context = item
        .args
        .get("validate_args_context")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    let mut r = Map::new();
    r.insert("changed".into(), json!(false));
    if errors.is_empty() {
        r.insert("msg".into(), json!("The arg spec validation passed"));
        r.insert("validate_args_context".into(), context);
        return TaskResult(r);
    }
    r.insert(
        "msg".into(),
        json!(format!(
            "Validation of arguments failed:\n{}",
            errors.join("\n")
        )),
    );
    r.insert("argument_errors".into(), json!(errors));
    r.insert("argument_spec_data".into(), Value::Object(spec));
    r.insert("validate_args_context".into(), context);
    r.insert("failed".into(), json!(true));
    TaskResult(r)
}

/// Whether a value can be read as the type the spec asked for, with the reference's own
/// complaint when it cannot.
///
/// Ansible converts permissively and this follows it where the conversion was measured: a string
/// `"3"` is an `int`, `"yes"` is a `bool`. Only the two numeric types and `bool` can fail; `str`,
/// `path`, `raw`, `list` and `dict` take what they are given, which is what the reference does
/// with them for every shape a playbook can write.
fn converts(value: &Value, wanted: &str) -> Result<(), String> {
    let quoted = || {
        format!(
            "\"'{}'\" cannot be converted to an {wanted}",
            display(value)
        )
    };
    match wanted {
        "int" => match value {
            Value::Number(n) if n.is_i64() || n.is_u64() => Ok(()),
            Value::String(s) if s.trim().parse::<i64>().is_ok() => Ok(()),
            _ => Err(quoted()),
        },
        "float" => match value {
            Value::Number(_) => Ok(()),
            Value::String(s) if s.trim().parse::<f64>().is_ok() => Ok(()),
            _ => Err(format!(
                "\"'{}'\" cannot be converted to a {wanted}",
                display(value)
            )),
        },
        "bool" => match value {
            Value::Bool(_) => Ok(()),
            Value::String(s) if crate::yaml::bool_from_str(s.trim()).is_some() => Ok(()),
            Value::Number(n) if n.as_i64() == Some(0) || n.as_i64() == Some(1) => Ok(()),
            _ => Err(format!(
                "\"'{}'\" cannot be converted to a {wanted}",
                display(value)
            )),
        },
        _ => Ok(()),
    }
}

/// One module result, ready to report: `changed_when` and `failed_when` decide its outcome
/// wherever the module ran, controller side as well as on the agent.
pub(super) fn finish(
    task: &PlayTask,
    item: &Item,
    result: TaskResult,
    templar: &Templar,
) -> TaskResult {
    // A run that hit the `timeout` keyword is reported as it stands. In ansible-core 2.19.12 the
    // timeout is an exception raised out of the handler's run, past everything that would have
    // judged the result, so `failed_when: false` cannot make a pass of it. A refused raw
    // argument is raised before the handler runs, and stands the same way.
    if timed_out(&result) || raw_params_refusal(task).is_some() {
        return result;
    }
    let mut r = apply_conditions(task, item, result, templar);
    settle_failure(&mut r, task);
    r
}

/// Whether a result is the one [`TaskResult::timed_out`] builds: a failure carrying the period
/// it ran out of.
fn timed_out(result: &TaskResult) -> bool {
    result.failed()
        && result
            .0
            .get("timedout")
            .and_then(|t| t.get("period"))
            .is_some()
}

/// Applies `changed_when` and `failed_when` to one result, with `result` bound to it.
///
/// First, `failed` is settled: filled in when the result does not carry it, with the value
/// [`TaskResult::failed`] reads off `rc`, and made `true` on a result that fails by a truthy
/// non-boolean. ansible-core's `TaskExecutor._execute` fills it in on every result that ran,
/// before the conditions and before `register`, and registers `True` for a failure. A skipped
/// item never comes here, and the reference leaves `failed` off it too.
fn apply_conditions(
    task: &PlayTask,
    item: &Item,
    mut result: TaskResult,
    templar: &Templar,
) -> TaskResult {
    // A skip an action plugin raised ends `_execute` before `failed` is filled in.
    if result.0.remove(RAISED_SKIP).is_some() {
        return result;
    }
    result.settle_failed();
    // `_execute` judges a result with the conditions only when it is not a skip, as check mode
    // makes of a command it will not run.
    if result.0.contains_key("skipped")
        || (task.changed_when.is_empty() && task.failed_when.is_empty())
    {
        return result;
    }
    let vars = condition_vars(task, item, &result, templar);
    if !task.changed_when.is_empty() {
        match all_hold(&task.changed_when, &vars, templar) {
            Ok(changed) => result.0.insert("changed".into(), json!(changed)),
            Err(e) => return unevaluable(result, "changed", &e),
        };
    }
    if !task.failed_when.is_empty() {
        let failed = match all_hold(&task.failed_when, &vars, templar) {
            Ok(failed) => failed,
            Err(e) => return unevaluable(result, "failed", &e),
        };
        result.0.insert("failed_when_result".into(), json!(failed));
        // Over a non-zero rc too: the condition has spoken.
        result.0.insert("failed".into(), json!(failed));
        // Measured on ansible-core 2.19.12: `command: "false"` under `failed_when: false`
        // registers `failed_when_suppressed_exception: "(traceback unavailable)"` and no
        // `exception`.
        if !failed && let Some(exception) = result.0.remove("exception") {
            result
                .0
                .insert("failed_when_suppressed_exception".into(), exception);
        }
    }
    result
}

/// A result whose `changed_when` or `failed_when` could not be evaluated, as ansible-core's
/// `_execute` leaves it: every key the module returned kept, `failed` set, and the error under
/// `changed_when_result` or `failed_when_result`, the conditions after it not evaluated. Its `msg`
/// is then [`settle_failure`]'s. Measured on ansible-core 2.19.12: `command: "true"` under
/// `failed_when: nosuch == 1` registers `failed_when_result: "Error while evaluating conditional:
/// 'nosuch' is undefined"` beside its `rc` and its empty `msg`; `changed_when: a.nokey` registers
/// `changed_when_result` and never reads a `failed_when: true` behind it; a syntax error reads
/// `Syntax error in template: unexpected '*'`. The prefixes are the reference's; what follows them
/// is this engine's wording of the same fault.
fn unevaluable(mut result: TaskResult, condition: &str, err: &TemplateError) -> TaskResult {
    let text = match err.0.strip_prefix("syntax error: ") {
        Some(rest) => format!("Syntax error in template: {rest}"),
        None => format!("Error while evaluating conditional: {}", err.0),
    };
    result.0.insert("failed".into(), json!(true));
    result
        .0
        .insert(format!("{condition}_when_result"), json!(text));
    result
}

/// Whether the loop ends after this item: every `break_when` condition holds against the item's
/// variables with the result under the registered name, the way the reference evaluates it
/// right after the item. A condition that cannot be evaluated fails the item instead, and the
/// loop ends there too.
pub(super) fn break_after(
    task: &PlayTask,
    item: &Item,
    result: &mut TaskResult,
    templar: &Templar,
) -> bool {
    if task.break_when.is_empty() {
        return false;
    }
    let mut vars = item.vars.clone();
    if let Some(name) = &task.register {
        // What the host returned: data, never templated again.
        vars.insert_untrusted(name.clone(), Value::Object(result.0.clone()));
    }
    for condition in &task.break_when {
        match templar.condition(condition, &vars) {
            Ok(true) => {}
            Ok(false) => return false,
            Err(err) => {
                *result = unevaluable(std::mem::take(result), "break", &err);
                return true;
            }
        }
    }
    true
}

/// Walks the items `from` up to `upto` and says which one `break_when` ends the loop at, if any,
/// and how many items it passed without one. A skipped item counts, as in the reference; an item
/// that never ran has no result and is passed. The reference pauses after every item that does
/// not end the loop and has another behind it, so the caller owes one pause per item passed,
/// less one when `upto` is the end of the loop.
pub(super) fn broke_before(
    task: &PlayTask,
    items: &[Item],
    received: &mut [Option<TaskResult>],
    from: &mut usize,
    upto: usize,
    templar: &Templar,
) -> (Option<usize>, usize) {
    let mut passed = 0;
    while *from < upto {
        let ii = *from;
        *from += 1;
        let mut result = match (&items[ii].skipped, &received[ii]) {
            (Some(s), _) => s.clone(),
            (None, Some(r)) => r.clone(),
            (None, None) => {
                passed += 1;
                continue;
            }
        };
        if break_after(task, &items[ii], &mut result, templar) {
            received[ii] = Some(result);
            return (Some(ii), passed);
        }
        passed += 1;
    }
    (None, passed)
}

/// The variables `changed_when`, `failed_when` and `until` read for one result: the item's, with
/// the facts the result returned laid over them, then the registered name and `result`.
///
/// ansible-core's `TaskExecutor._execute` updates its copy of the variables with
/// `result['ansible_facts']` before it judges, and registers the result again after that, so a
/// fact never hides the registered name. `set_fact` and `include_vars` write theirs as they are,
/// unless `delegate_facts` sends them to the delegate, which leaves them out of sight here; any
/// other module's go through [`HostVars::returned_facts`], `delegate_facts` or not. `debug` is
/// the one module whose facts are never read. Measured on ansible-core 2.19.12: `set_fact:
/// {seen: 1}` passes `failed_when: seen is not defined`, a module's fact wins over a `-e` of the
/// same name, and a delegated `set_fact` under `delegate_facts: true` does not see its own fact.
fn condition_vars(
    task: &PlayTask,
    item: &Item,
    result: &TaskResult,
    templar: &Templar,
) -> HostVars {
    let mut vars = item.vars.clone();
    if let Some(facts) = result.0.get("ansible_facts").and_then(Value::as_object) {
        let delegated =
            task.delegate_to.as_deref().is_some_and(|to| !to.is_empty()) && task.delegates_facts();
        match short_name(&task.module) {
            "debug" => {}
            "set_fact" | "include_vars" if delegated => {}
            // The trust `record_local_facts` gives the same names once they are written.
            name @ ("set_fact" | "include_vars") => {
                let data = set_fact_data(task, item, templar);
                for (key, value) in facts {
                    if name == "set_fact" && data.contains(key) {
                        vars.insert_untrusted(key.clone(), value.clone());
                    } else {
                        vars.insert(key.clone(), value.clone());
                    }
                }
            }
            _ => vars.returned_facts(facts),
        }
    }
    if let Some(reg) = &task.register {
        vars.insert_untrusted(reg.clone(), Value::Object(result.0.clone()));
    }
    vars.insert_untrusted("result".into(), Value::Object(result.0.clone()));
    vars
}

/// What ansible-core's `maybe_raise_on_result` leaves on a failed final result, once the
/// conditions and `until` are done with it: `exception: "(traceback unavailable)"`, and a `msg`
/// when the result carries none. Measured on ansible-core 2.19.12: a `set_fact`, a `ping` and a
/// `ping` loop item under `failed_when: true`, and a `ping` whose `until: false` ran out, all
/// register `msg: "Task failed: Action failed: Unknown error."`; a `debug` keeps its own `msg`
/// and a `command` its empty one. `until` never sees that `msg`: `until: r.msg is defined` over
/// `failed_when: true` retries. A module result that already carried a truthy `exception` was
/// normalised as the module's error, and reads `Module failed` instead; a controller-side action
/// (`fail`, `assert`) is the action's own error whatever it carries, measured too.
fn settle_failure(result: &mut TaskResult, task: &PlayTask) {
    let cause = result.0.remove(CAUSE_KEY);
    if result.failed() {
        let local = volant_protocol::modules::local(&task.module).is_some();
        let by = match result.0.get("exception") {
            Some(e) if !local && !python_falsy(e) => "Module",
            _ => "Action",
        };
        result
            .0
            .entry("msg")
            .or_insert_with(|| json!(format!("Task failed: {by} failed: Unknown error.")));
        // The block the reference prints over the failure's line: `Task failed: <by> failed: `
        // and the error's brief message, the module's summary when it sent one.
        let msg = match result.0.get("msg") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => crate::render::ansible_json(other),
            None => String::new(),
        };
        let detail = cause
            .as_ref()
            .and_then(Value::as_str)
            .map_or(msg.clone(), str::to_string);
        // `AnsibleError`: an empty message is the default one alone, `Action failed.`.
        let by_failed = format!("{by} failed.");
        // `Unexpected AnsibleActionFail error: ...` is the sentence the reference's own `copy`
        // fails with for a source that is not there, and its block carries it as it is. Any
        // other result starting with it only loses the `Action failed: ` in its error block.
        let message = if msg.starts_with("Task failed: ") {
            msg
        } else if msg.starts_with("Unexpected AnsibleActionFail error: ") {
            crate::render::task_failed(&msg)
        } else if detail.is_empty() {
            crate::render::task_failed(&by_failed)
        } else {
            crate::render::task_failed(&crate::render::concat_message(&by_failed, &detail))
        };
        // A failure raised before the action ran already carries its block's message.
        result
            .0
            .entry(crate::render::ERROR_KEY)
            .or_insert_with(|| Value::from(message));
        result
            .0
            .insert("exception".into(), json!("(traceback unavailable)"));
    }
}

fn all_hold(
    conditions: &[String],
    vars: &HostVars,
    templar: &Templar,
) -> Result<bool, TemplateError> {
    for c in conditions {
        if !templar.condition(c, vars)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// What a loop over an empty list reports and registers. The reference shows the task as
/// `skipping` and gives `skipped_reason` this exact wording, with no items behind it.
pub(super) fn empty_loop_result() -> TaskResult {
    let mut r = Map::new();
    r.insert("changed".into(), json!(false));
    r.insert("skipped".into(), json!(true));
    r.insert("skipped_reason".into(), json!("No items in the list"));
    TaskResult(r)
}

/// Whether a task's results are a loop's, given its first item's element. A loop whose list was
/// an undefined variable and whose `when` left it out has one item with no element, and it is
/// reported and registered as a plain task: the reference's `items` is `None` there, so the task
/// goes through `_execute` once, never through `_run_loop`.
pub(super) fn loops(task: &PlayTask, first: Option<&Option<Value>>) -> bool {
    task.loop_items.is_some() && !matches!(first, Some(None))
}

/// What `register` stores: the single result, or Ansible's loop aggregate.
pub(super) fn registered_value(task: &PlayTask, results: &[(Option<Value>, TaskResult)]) -> Value {
    if !loops(task, results.first().map(|(element, _)| element)) {
        return results.first().map_or(Value::Null, |(_, r)| {
            Value::Object(super::report::without_error(r).0)
        });
    }
    if results.is_empty() {
        let mut agg = empty_loop_result().0;
        agg.insert("results".into(), Value::Array(Vec::new()));
        return Value::Object(agg);
    }
    let mut list = Vec::new();
    for (element, r) in results {
        let mut m = super::report::without_error(r).0;
        if let Some(el) = element {
            super::report::item_keys(task, &mut m, el);
        }
        list.push(Value::Object(m));
    }
    let mut agg = Map::new();
    agg.insert("results".into(), Value::Array(list));
    agg.insert(
        "changed".into(),
        json!(results.iter().any(|(_, r)| r.changed())),
    );
    // Measured on ansible-core 2.19.12: a loop with a failed item registers `One or more items
    // failed`, `ignore_errors` or not, and one whose every item was skipped `All items skipped`.
    let failed = results.iter().any(|(_, r)| r.failed());
    if failed {
        agg.insert("failed".into(), json!(true));
    }
    let skipped = results.iter().all(|(_, r)| r.skipped());
    agg.insert("skipped".into(), json!(skipped));
    let msg = if skipped {
        "All items skipped"
    } else if failed {
        "One or more items failed"
    } else {
        "All items completed"
    };
    agg.insert("msg".into(), json!(msg));
    Value::Object(agg)
}

/// `ansible_failed_task`: the task that failed, as a rescue reads it.
///
/// Measured on ansible-core 2.19.12 with a `command` carrying `register`, `when`, `tags` and
/// `vars`: the dictionary holds one key per task keyword, `action` holding the module's short
/// name, plus six keys of the engine's own (`uuid`, `finalized`, `squashed`, `_resolved_action`,
/// `async_val`, `loop_with`) that describe nothing a playbook wrote. Those six are deliberately
/// not reproduced, and neither are the `_ansible_*` keys the reference adds to `args` on its way
/// to the module.
///
/// The keys come from the keyword table itself rather than from a list written here, so a
/// keyword added to the grammar appears in this dictionary too. Two families are left out
/// because the reference has already resolved them by the time it builds it: `local_action`,
/// which is folded into `action`, and the `with_*` lookups, which are folded into `loop`.
///
/// A keyword this release parks reads `null` here rather than the reference's default. The
/// default would describe something nothing in this engine honours, and a rescue reading
/// `ansible_failed_task.diff` is better told nothing than told `false` by an engine that
/// never looked at the keyword. `connection`, `port` and `remote_user` read what the task, its
/// block or its play wrote, and `null` when none did.
///
/// `become_method`, `become_exe` and `become_flags` read the same way: what the task, its block
/// or its play wrote, and `null` when none did.
pub(super) fn failed_task_value(task: &PlayTask) -> Value {
    let mut out = Map::new();
    let strings = |list: &[String]| Value::Array(list.iter().map(|s| json!(s)).collect());
    for kw in crate::keywords::TASK_KEYWORDS {
        if kw.name == "local_action" || kw.name.starts_with("with_") {
            continue;
        }
        let value = match kw.name {
            "action" => json!(short_name(&task.module)),
            "args" => Value::Object(task.args.clone()),
            "become" => json!(task.r#become),
            "become_user" => json!(task.become_user),
            "become_method" => json!(task.connect.become_method),
            "become_exe" => json!(task.connect.become_exe),
            "become_flags" => json!(task.connect.become_flags),
            "changed_when" => strings(&task.changed_when),
            "connection" => json!(task.connect.connection),
            "failed_when" => strings(&task.failed_when),
            "ignore_errors" => match &task.ignore_errors {
                Some(crate::playbook::Flag::Fixed(b)) => json!(b),
                Some(crate::playbook::Flag::Template(t)) => json!(t),
                None => Value::Null,
            },
            "loop" => task.loop_items.clone().unwrap_or(Value::Null),
            "loop_control" => json!({"loop_var": task.loop_var, "label": task.loop_label}),
            "name" => json!(task.name),
            "port" => match &task.connect.port {
                Some(port) => port
                    .parse::<u16>()
                    .map_or_else(|_| json!(port), |n| json!(n)),
                None => Value::Null,
            },
            "register" => json!(task.register),
            "remote_user" => json!(task.connect.remote_user),
            "tags" => strings(&task.tags),
            "timeout" => json!(task.timeout),
            "vars" => Value::Object(task.vars.clone()),
            "when" => strings(&task.when),
            _ => Value::Null,
        };
        out.insert(kw.name.to_string(), value);
    }
    Value::Object(out)
}

/// The handlers one finished task asks for, if it changed anything and did not fail, and the
/// warnings looking them up gave.
///
/// Measured on ansible-core 2.19.12 and each half worth stating: a task that came back `ok`
/// notifies nothing, a name notified twice runs its handler once, and for a loop it is the
/// **aggregate** that decides - one changed item is a changed task, and then every item's
/// rendered names are notified, the unchanged ones' included. A failed task notifies nothing,
/// `ignore_errors` or not: the reference files a notification from the ok branch of
/// `_process_pending_results` alone. Notifications live in the driver and nowhere else: they are
/// one host's business, and the flush the coordinator opens is the same flush whether this host
/// has anything to run in it or not.
///
/// The names come from [`Item::notify`], rendered before the task ran. A handler's `name` may be
/// a template too, rendered here against the first item's variables; `listen` never is. One that
/// does not render answers to nothing by name and warns, as the reference does, unless it
/// listens for a topic. What a host's variables render to is only ever compared with the handler
/// names, never templated again. A name no handler answers to is the `Err`: the reference raises
/// it from its strategy (`ERROR_ON_MISSING_HANDLER`, on by default), so it ends the run rather
/// than failing the task - no `ignore_errors` and no `rescue` can take it, and it is measured to
/// come after the notifying task's banner and before its result line.
pub(super) fn notified_handlers(
    compiled: &Compiled,
    task: &PlayTask,
    items: &[Item],
    templar: &Templar,
    results: &[(Option<Value>, TaskResult)],
) -> Result<(Vec<usize>, Vec<String>), String> {
    let none = (Vec::new(), Vec::new());
    let Some(vars) = items.first().map(|i| &i.vars) else {
        return Ok(none);
    };
    if task.notify.is_empty()
        || results.iter().any(|(_, r)| r.failed())
        || !results.iter().any(|(_, r)| r.changed())
    {
        return Ok(none);
    }
    let mut warnings = Vec::new();
    let names: Vec<Option<String>> = compiled
        .handlers
        .iter()
        .map(|h| {
            let name = &h.task.name;
            if !Templar::is_template(name) {
                return Some(name.clone());
            }
            match templar.render(name, vars) {
                Ok(Value::String(rendered)) => Some(rendered),
                Ok(other) => Some(display(&other)),
                Err(e) => {
                    if h.listen.is_empty() {
                        warnings.push(format!(
                            "Handler '{name}' is unusable because it has no listen topics and the name could not be templated (host-specific variables are not supported in handler names). The error: {}",
                            e.0
                        ));
                    }
                    None
                }
            }
        })
        .collect();
    let mut notified = Vec::new();
    for name in items.iter().flat_map(|i| &i.notify) {
        let found = crate::compile::resolve_notify(compiled, name, &names);
        if found.is_empty() {
            return Err(format!(
                "The requested handler '{name}' was not found in either the main handlers list nor in the listening handlers list"
            ));
        }
        // Not deduplicated here, deliberately: what makes a handler run once is the driver
        // taking **every** copy of its index off the list when it runs it.
        notified.extend(found);
    }
    Ok((notified, warnings))
}

/// Takes the `warnings` off every result of a task, in item order, for the driver to show as
/// `[WARNING]:` lines under it and for [`record_registered`] to put back where the reference
/// registers them. Off the results, because no line the reference prints shows the key: its
/// callback pops it before it dumps a result.
pub(super) fn take_warnings(results: &mut [(Option<Value>, TaskResult)]) -> Vec<String> {
    let mut out = Vec::new();
    for (_, result) in results.iter_mut() {
        if let Some(Value::Array(list)) = result.0.remove("warnings") {
            out.extend(list.iter().map(warning_text));
        }
    }
    out
}

/// A warning as the reference registers and shows it. A string is kept as it is. A module of
/// ansible-core 2.19 answers a `WarningSummary` object, rendered as `format_event_brief_message`
/// renders it: the event's `msg`, then each cause its chain follows, joined by `concat_message`
/// (`left.rstrip(". ") + ": " + right`), a part that already ends with what follows it kept
/// alone. Anything else is shown as its JSON.
fn warning_text(warning: &Value) -> String {
    if let Value::String(text) = warning {
        return text.clone();
    }
    let mut parts = Vec::new();
    let mut event = warning.get("event");
    while let Some(msg) = event.and_then(|e| e.get("msg")).and_then(Value::as_str) {
        parts.push(msg);
        event = event
            .and_then(|e| e.get("chain"))
            .filter(|chain| chain.get("follow") == Some(&Value::Bool(true)))
            .and_then(|chain| chain.get("event"));
    }
    let Some((deepest, outer)) = parts.split_last() else {
        return warning.to_string();
    };
    let mut message = (*deepest).to_string();
    for part in outer.iter().rev() {
        message = if part.ends_with(&message) {
            (*part).to_string()
        } else {
            format!("{}: {message}", part.trim_end_matches(['.', ' ']))
        };
    }
    message
}

/// What `until`, `retries` and `delay` ask of one task, rendered against its own variables.
///
/// The rule is ansible-core 2.19.12's `TaskExecutor._execute` (`task_executor.py`, the attempt
/// loop): `retries: R` is **R + 1 runs**, the first plus R retries, and `until` with no `retries`
/// is R = 3. A retry line and the `delay` follow each of the first R failed runs, never the last;
/// the line after run `i` says `(R + 1 - i retries left)`, so the last line says `(1 retries left)`
/// and is followed by one more run. A run out of attempts reports `attempts: R`, not R + 1.
/// Measured there on the dev machine: `retries: 2` on a failing `shell` that appends a line to a
/// file prints both retry lines, reports `"attempts": 2`, and leaves **three** lines in the file.
/// Also measured: `retries` with no `until` retries while the result is failed; `retries` under 1
/// (0 and -1) turns the whole machinery off, so the task runs once, prints no retry line, carries
/// no `attempts` and is not failed by a condition that never held; the default `delay` is five
/// seconds.
#[derive(Clone)]
pub(super) struct Retry {
    /// R: the runs after the first, never zero.
    pub(super) retries: u32,
    pub(super) delay: Duration,
    /// The conditions that end the loop. Empty means "the result did not fail".
    until: Vec<String>,
    /// What `time.sleep(delay)` raises in the reference, for a delay it refuses.
    unsleepable: Option<&'static str>,
}

/// `time.sleep` on a value Python's clock, nanoseconds in a signed 64-bit integer, cannot hold.
const PYTIME_OUT_OF_RANGE: &str = "Task failed: timestamp out of range for C PyTime_t";

/// `time.sleep(float('nan'))`.
const SLEEP_NAN: &str = "Task failed: Invalid value NaN (not a number)";

impl Retry {
    /// The failure the reference's wait between two attempts raises for a `delay` its
    /// `time.sleep` refuses, in place of the wait: the item ends there, after the retry line.
    /// Measured on ansible-core 2.19.12: `retries: 1` on a failing `command` with `delay: 1e20`
    /// prints `(1 retries left)`, then fails the item with `{"changed": false, "msg": "Task
    /// failed: timestamp out of range for C PyTime_t"}`, and `delay: nan` with `Task failed:
    /// Invalid value NaN (not a number)`.
    pub(super) fn sleep_failure(&self) -> Option<TaskResult> {
        self.unsleepable.map(|msg| {
            let mut failed = TaskResult::failed_with(msg);
            failed.0.insert("changed".into(), json!(false));
            failed
        })
    }
}

/// `item` is the one whose variables `retries` and `delay` are rendered against.
pub(super) fn retry_plan(
    task: &PlayTask,
    item: Option<&Item>,
    templar: &Templar,
) -> Result<Option<Retry>, TaskResult> {
    let empty = HostVars::default();
    let vars = item.map_or(&empty, |i| &i.vars);
    // `retries` is the reference's `isa='int'`, which refuses a value it would truncate (its
    // `Decimal` check: `'2.5'` and `2.5` fail, `'2.0'` and `true` pass), and `delay` its
    // `isa='float'`. Measured on ansible-core 2.19.12, both messages included.
    let number = |raw: &Value, keyword: &'static str, isa: &str| -> Result<f64, TaskResult> {
        let rendered = templar
            .render_value(raw, vars)
            .map_err(|e| raised(&format!("Task failed: {}", e.0), Vec::new()))?;
        let n = match &rendered {
            Value::Bool(b) => Some(f64::from(u8::from(*b))),
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        };
        n.filter(|n| isa == "float" || (n.is_finite() && n.fract() == 0.0))
            .ok_or_else(|| {
                let keyword_error = format!("Error processing keyword '{keyword}'.");
                let value_error = format!(
                    "The value {} could not be converted to '{isa}'.",
                    crate::playbook::python_repr(&rendered)
                );
                let at = task.value_origins.get(keyword);
                // Measured on ansible-core 2.19.12: the keyword's cause points at the keyword's
                // value, and the conversion's at where the value was written - the same place
                // for a literal (`delay: x`). A value a template produced was written somewhere
                // this engine does not track, so its sentence joins the keyword's, as the
                // reference joins a cause that has no origin of its own.
                let literal = !matches!(raw, Value::String(s) if Templar::is_template(s));
                let causes = if literal {
                    vec![
                        crate::render::cause(&keyword_error, at, None),
                        crate::render::cause(&value_error, at, None),
                    ]
                } else {
                    vec![crate::render::cause(
                        &crate::render::concat_message(&keyword_error, &value_error),
                        at,
                        None,
                    )]
                };
                raised(
                    &format!(
                        "Task failed: {}",
                        crate::render::concat_message(&keyword_error, &value_error)
                    ),
                    causes,
                )
            })
    };
    let retries = match &task.retries {
        Some(raw) => {
            let n = number(raw, "retries", "int")?;
            if n < 1.0 {
                return Ok(None);
            }
            n as u32
        }
        None if task.until.is_empty() => return Ok(None),
        None => 3,
    };
    // `if delay < 0: delay = 1` in ansible-core 2.19.12 `task_executor.py`, then `time.sleep`,
    // which refuses NaN and anything its nanosecond clock cannot hold.
    let (delay, unsleepable) = match &task.delay {
        Some(raw) => match number(raw, "delay", "float")? {
            d if d.is_nan() => (Duration::MAX, Some(SLEEP_NAN)),
            d if d < 0.0 => (Duration::from_secs(1), None),
            d if d * 1e9 > i64::MAX as f64 => (Duration::MAX, Some(PYTIME_OUT_OF_RANGE)),
            d => (Duration::from_secs_f64(d), None),
        },
        None => (Duration::from_secs(5), None),
    };
    Ok(Some(Retry {
        retries,
        delay,
        until: task.until.clone(),
        unsleepable,
    }))
}

/// One item's retry plan, or the failed result it reports instead of running: its `retries` or
/// `delay` did not render, or the task's `timeout` is out of range.
pub(super) type ItemRetry = Result<Option<Retry>, TaskResult>;

/// The largest `timeout` the reference accepts (`_alarm_timeout.py`, `_MAX_TIMEOUT`).
const MAX_TIMEOUT: u64 = 100_000_000;

/// Each item's retry plan, rendered against its own variables: the reference post-validates
/// `retries` and `delay` once per item. An item whose render fails reports that failure and the
/// other items run: measured on ansible-core 2.19.12, a loop with `retries: "{{ item.r }}"` over
/// `r: 1`, `r: 2`, `r: x` and `r: 0` retries the first once and the second twice, fails the
/// third alone with `Task failed: Error processing keyword 'retries': The value 'x' could not be
/// converted to 'int'.`, and runs the fourth once with no `attempts`. A skipped item renders
/// nothing.
///
/// A `timeout` over [`MAX_TIMEOUT`] fails each item that runs, after the retry plan rendered, as
/// the reference's alarm refuses it at the first attempt: measured on ansible-core 2.19.12,
/// `timeout: 100000001` fails every item of a loop with `Task failed: Timeout 100000001 is
/// invalid, it must be between 0 and 100000000.` and runs nothing. Every deadline behind this
/// point is then a sum that fits.
pub(super) fn retry_plans(task: &PlayTask, items: &[Item], templar: &Templar) -> Vec<ItemRetry> {
    items
        .iter()
        .map(|item| {
            if item.skipped.is_some() {
                return Ok(None);
            }
            let plan = retry_plan(task, Some(item), templar)?;
            match task.timeout {
                Some(t) if t > MAX_TIMEOUT => Err(raised(
                    &format!(
                        "Task failed: Timeout {t} is invalid, it must be between 0 and {MAX_TIMEOUT}."
                    ),
                    Vec::new(),
                )),
                _ => Ok(plan),
            }
        })
        .collect()
}

/// A controller-side task its `timeout` ended. Measured on ansible-core 2.19.12: its error block
/// chains `Timed out after N second(s).` behind `Task failed.`, where a module's timeout prints
/// one line.
pub(super) fn local_timed_out(seconds: u64) -> TaskResult {
    let mut r = TaskResult::timed_out(seconds);
    r.0.insert(
        crate::render::CHAIN_KEY.into(),
        json!([crate::render::cause(
            &format!("Timed out after {seconds} second(s)."),
            None,
            None
        )]),
    );
    r
}

/// A failure the executor raised rather than a module returned: `changed: false` and the message,
/// measured on ansible-core 2.19.12, and the causes its error block chains, if any.
pub(super) fn raised(msg: &str, causes: Vec<Value>) -> TaskResult {
    let mut failed = TaskResult::failed_with(msg);
    failed.0.insert("changed".into(), json!(false));
    if !causes.is_empty() {
        failed
            .0
            .insert(crate::render::CHAIN_KEY.into(), Value::Array(causes));
    }
    failed
}

/// Whether any item of [`retry_plans`] retries or failed to render, which sends the task down
/// the one-item-at-a-time path.
pub(super) fn any_retried(plans: &[ItemRetry]) -> bool {
    plans.iter().any(|plan| !matches!(plan, Ok(None)))
}

/// Whether the attempt loop stops here: the `until` conditions all hold, or - when the task gave
/// none - the result did not fail.
///
/// Measured on ansible-core 2.19.12, both halves: a task whose `failed_when` makes it fail while
/// its `until` holds stops at once (`attempts: 1`, no retry line), and a task whose
/// `changed_when` makes `until: r.changed` false retries although the module itself succeeded.
/// So the conditions decide the result first and `until` reads what they decided, with the
/// registered name bound to it.
pub(super) fn until_holds(
    task: &PlayTask,
    item: &Item,
    result: &TaskResult,
    retry: &Retry,
    templar: &Templar,
) -> Result<bool, TemplateError> {
    if retry.until.is_empty() {
        return Ok(!result.failed());
    }
    all_hold(
        &retry.until,
        &condition_vars(task, item, result, templar),
        templar,
    )
}

/// One `FAILED - RETRYING` line: the count it shows and the attempt that failed.
pub(super) type RetryLine = (u32, TaskResult);

/// What one run of a retried task decides.
pub(super) enum Attempt {
    /// The loop ends with this result.
    Done(TaskResult),
    /// Another run follows, after a retry line showing this count, then the `delay`. The result
    /// is the failed attempt's, with `attempts` and `retries` on it, for that line's `Result was:`.
    Again(u32, TaskResult),
}

/// Run `attempt` (from 1) of a retried task, judged: `changed_when` and `failed_when` first,
/// `attempts` set so `until` can read it, then `until`. The one place the attempt rule lives, for
/// the controller-side, remote and action-plugin loops alike.
///
/// An `until` that cannot be evaluated ends the task there, with no further run and no
/// `attempts` - measured, and the reference's own prefix for a task that dies rather than fails.
/// Running out is a failed task even when the module passed (measured with `changed_when: false`
/// under `until: r.changed`), and it reports `attempts: R` although R + 1 runs happened.
pub(super) fn judge_attempt(
    task: &PlayTask,
    item: &Item,
    raw: TaskResult,
    attempt: u32,
    retry: &Retry,
    templar: &Templar,
) -> Attempt {
    // Out of the whole loop, as the reference's timeout exception is: see `finish`.
    if timed_out(&raw) {
        return Attempt::Done(raw);
    }
    // Before the conditions, which may read it, and again after them, since a condition that
    // could not be evaluated hands back a result of its own.
    let mut raw = raw;
    raw.0.insert("attempts".into(), json!(attempt));
    let mut r = apply_conditions(task, item, raw, templar);
    r.0.insert("attempts".into(), json!(attempt));
    match until_holds(task, item, &r, retry, templar) {
        // Measured on ansible-core 2.19.12: `changed: false`, and a cause pointing at the
        // `until` value.
        Err(e) => {
            let cause = format!("Error while evaluating conditional: {}", e.0);
            return Attempt::Done(raised(
                &conditional_error(&e),
                vec![crate::render::cause(
                    &cause,
                    task.value_origins.get("until"),
                    None,
                )],
            ));
        }
        Ok(true) => {
            settle_failure(&mut r, task);
            return Attempt::Done(r);
        }
        Ok(false) => {}
    }
    if attempt > retry.retries {
        r.0.insert("attempts".into(), json!(retry.retries));
        r.0.insert("failed".into(), json!(true));
        settle_failure(&mut r, task);
        return Attempt::Done(r);
    }
    // `retries` is the reference's count of runs, R + 1, measured in its `Result was:`.
    r.0.insert("retries".into(), json!(retry.retries + 1));
    Attempt::Again(retry.retries + 1 - attempt, r)
}

/// What an `until` that cannot be evaluated reports.
///
/// Measured on ansible-core 2.19.12: `Task failed: Error while evaluating conditional: object of
/// type 'dict' has no attribute 'nosuchkey'`, with no further attempt and no `attempts`. The
/// prefix is the reference's, which is what a playbook testing `'Task failed' in result.msg`
/// reads; the sentence behind it is this engine's own wording for the same fault.
pub(super) fn conditional_error(err: &TemplateError) -> String {
    format!("Task failed: Error while evaluating conditional: {}", err.0)
}

/// One step's items, as the tasks to send and the results of those that cannot be sent at all.
///
/// The results go in here rather than at the call site so that they cannot be dropped there: an
/// item that cannot travel and reports nothing leaves the driver with no result for it, and a
/// step with no results at all is a step the run reports **no line for** - it finishes `ok`,
/// counts nothing, and says nothing about a task that never ran. That is this project's worst
/// failure, and this function is where it is kept shut.
pub(super) fn step_tasks(
    task: &PlayTask,
    items: &[Item],
    python: &Result<Option<(&ModulePayload, &str)>, TaskResult>,
    results: &mut [Option<TaskResult>],
) -> Vec<(usize, Task)> {
    let mut out = Vec::new();
    let refused = raw_params_refusal(task);
    for (ii, item) in items.iter().enumerate() {
        // A skipped item already has its result and never had a task.
        if item.skipped.is_some() {
            continue;
        }
        match (&refused, python) {
            (Some(failure), _) | (None, Err(failure)) => results[ii] = Some(failure.clone()),
            (None, Ok(python)) => out.push((ii, protocol_task(task, item, *python))),
        }
    }
    out
}

/// The name the reference gives a task's action in its messages: a builtin by its resolved
/// FQCN, a collection's by the name the task wrote.
pub(super) fn action_fqcn(module: &str) -> String {
    if crate::python::is_resolved_name(module) {
        module.to_string()
    } else {
        format!("ansible.builtin.{}", short_name(module))
    }
}

/// What each item of a task reports when its module's string argument left words that are not
/// `key=value`, the module not reading it as a command line or as raw parameters.
///
/// Measured on ansible-core 2.19.12 with `ns.coll.m: v=1 stray`: the task fails when it runs,
/// after its `when` and item by item in a loop, registering `{changed: false, exception:
/// "(traceback unavailable)", failed: true, msg: "Task failed: Action 'ns.coll.m' does not
/// support raw params."}`; `ignore_errors` and `rescue` take it, and a play it fails exits 2.
/// It is the task's `post_validate` raising, before any attempt, so neither `failed_when:
/// false` nor `retries` changes it. A string that is one template is left to the pre-flight.
///
/// A builtin is named by its resolved FQCN, as the reference names it: `ping: foo` and
/// `ansible.legacy.ping: foo` both fail with `Action 'ansible.builtin.ping' does not support raw
/// params.` (measured, like `debug`, `file`, `copy`, `assert`, `fail`, `stat` and `lineinfile`).
pub(super) fn raw_params_refusal(task: &PlayTask) -> Option<TaskResult> {
    crate::playbook::stray_raw_params(task)
        .filter(|raw| !crate::playbook::possibly_all_template(raw))?;
    let action = action_fqcn(&task.module);
    let mut r = Map::new();
    r.insert("changed".into(), json!(false));
    r.insert("exception".into(), json!("(traceback unavailable)"));
    r.insert("failed".into(), json!(true));
    r.insert(
        "msg".into(),
        json!(format!(
            "Task failed: Action '{action}' does not support raw params."
        )),
    );
    Some(TaskResult(r))
}

/// The variables of the host a task's module actually runs on: the delegate's when the task has
/// one, the task's own host's otherwise.
///
/// One expression, used for the connection and for the interpreter both, because they have to be
/// the same host: a link opened to the delegate and an interpreter read off the delegating host
/// runs a module on one machine under the Python of another, which is a failure only a path that
/// does not exist on both would show.
pub(super) fn running_host_vars<'a>(
    delegate: Option<&'a (String, HostVars)>,
    items: &'a [Item],
) -> &'a HostVars {
    delegate.map_or(&items[0].vars, |(_, vars)| vars)
}

/// The interpreter a playbook asked for, if it asked one of them: `ansible_python_interpreter`
/// off the variables of the host the module will run on.
///
/// Four values are not interpreters at all. `auto`, `auto_legacy`, `auto_silent` and
/// `auto_legacy_silent` are ansible-core's way of **asking for discovery** - `auto` is its own
/// default and `auto_silent` is the usual way an inventory silences the discovery warning - so
/// they mean nobody named an interpreter and the agent's list governs. Sent as paths they would
/// fail every Python task of the group that set one, at rc 127, naming `auto_silent` as a file.
///
/// A blank value is unset too, the way a blank `ansible_remote_tmp` already is: a playbook
/// writing `"{{ py_override | default('') }}"` asked for nothing, and an empty path would have
/// the host trying to start nothing and naming nothing when it failed.
pub(super) fn requested_interpreter<V: crate::vars::HostSettings + ?Sized>(
    vars: &V,
) -> Option<String> {
    crate::vars::host_setting(vars, "ansible_python_interpreter")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|asked| {
            !asked.is_empty()
                && !matches!(
                    *asked,
                    "auto" | "auto_legacy" | "auto_silent" | "auto_legacy_silent"
                )
        })
        .map(str::to_string)
}

/// The fact the reference adds to the first result that discovered a host's interpreter.
pub(super) const DISCOVERED_INTERPRETER: &str = "discovered_interpreter_python";

/// What the warning on screen ends with; the copy a task registers stops before it.
pub(super) const DISCOVERY_HELP: &str = "See https://docs.ansible.com/ansible-core/2.19/reference_appendices/interpreter_discovery.html for more information.";

/// Whether the reference discovers the interpreter this task's module runs under, once nobody
/// named one. Measured on ansible-core 2.19.12: every module sent to a host does, but not `raw`
/// and `script` (they hand a line or a file to the shell) and not `fetch`, `reboot` and
/// `wait_for_connection`, whose result is not a module's. `add_host`, `group_by` and
/// `set_stats` send no module at all.
pub(super) fn discovers_interpreter<V: crate::vars::HostSettings + ?Sized>(
    module: &str,
    vars: &V,
) -> bool {
    !matches!(
        short_name(module),
        "raw"
            | "script"
            | "fetch"
            | "reboot"
            | "wait_for_connection"
            | "add_host"
            | "group_by"
            | "set_stats"
    ) && requested_interpreter(vars).is_none()
}

/// Whether the host asked for discovery without the warning, `auto_silent` or
/// `auto_legacy_silent`.
pub(super) fn silent_discovery<V: crate::vars::HostSettings + ?Sized>(vars: &V) -> bool {
    crate::vars::host_setting(vars, "ansible_python_interpreter")
        .and_then(Value::as_str)
        .is_some_and(|asked| matches!(asked.trim(), "auto_silent" | "auto_legacy_silent"))
}

/// Puts the discovered interpreter in the result's `ansible_facts`, where the reference returns
/// it and where [`record_facts`] makes it the host's own fact.
pub(super) fn stamp_discovery(result: &mut TaskResult, interpreter: &str) {
    let facts = result
        .0
        .entry("ansible_facts")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(facts) = facts {
        facts.insert(DISCOVERED_INTERPRETER.into(), json!(interpreter));
    }
}

/// The warning for an interpreter found rather than named, as a task registers it.
pub(super) fn discovery_warning(host: &str, interpreter: &str) -> String {
    format!(
        "Host '{host}' is using the discovered Python interpreter at '{interpreter}', but future installation of another Python interpreter could cause a different interpreter to be discovered."
    )
}

/// The Python interpreter a host runs a module under, or the failure its tasks report instead.
///
/// `ansible_python_interpreter` is the playbook's own choice and is used exactly as written,
/// whether or not the agent found it. The list the agent reports is the **auto-discovery** list -
/// the eight names the reference looks for on `PATH` - so it governs the choice nobody made and
/// nothing else. Reading it as an allow-list would refuse `/opt/python3.12/bin/python`, a real
/// interpreter the agent never probes for and the reference runs without comment.
///
/// An explicit interpreter that turns out not to be there is the host's to discover, by trying to
/// start it: measured on ansible-core 2.19.12, `ansible_python_interpreter=/nonexistent` is a task
/// failure at rc 127 carrying `The module interpreter '/nonexistent' was not found.` - a failure
/// of the task, notably not an `UNREACHABLE`, and one that needs the host to produce it.
pub(super) fn chosen_interpreter(
    named: Option<&str>,
    reported: &[String],
) -> Result<String, TaskResult> {
    match named {
        Some(named) => Ok(named.to_string()),
        // Worded as what it is. The controller uploads the agent it shipped with, so an agent
        // that predates the interpreter list reports none exactly as a host with no Python does,
        // and a sentence claiming the host has no Python would send the operator looking in the
        // wrong place.
        None => reported.first().cloned().ok_or_else(|| {
            TaskResult::failed_with(
                "the agent on this host reported no python interpreter, so this controller has nothing to run a module under",
            )
        }),
    }
}

/// What one step's tasks travel with: its payload under the interpreter chosen for this host, or
/// the failure they report instead of being sent.
///
/// A step with no payload travels as it always did. A step with one, on a host whose interpreter
/// could not be chosen, fails on that - per task, so a `rescue:` or an `ignore_errors` still
/// does its work. A payload with no interpreter chosen at all is a controller bug and says so
/// rather than sending a payload the agent cannot run.
pub(super) fn python_for<'a>(
    module: &str,
    payload: Option<&'a ModulePayload>,
    interpreter: Option<&'a Result<String, TaskResult>>,
) -> Result<Option<(&'a ModulePayload, &'a str)>, TaskResult> {
    let Some(payload) = payload else {
        // A module that runs through the payload path, on a task that carries none, is a module
        // nothing could have named when the run built its blob: a dynamic include reads the file
        // it names while the play runs. Refused by name rather than sent as if it were native,
        // which the agent would answer with an unknown module and the operator would read as a
        // typo.
        if crate::python::is_python_module(module) {
            return Err(no_payload(module));
        }
        return Ok(None);
    };
    match interpreter {
        Some(Ok(path)) => Ok(Some((payload, path.as_str()))),
        Some(Err(failure)) => Err(failure.clone()),
        None => Err(TaskResult::failed_with(format!(
            "the module payload {} was built for this task with no interpreter chosen for the host",
            payload.blob
        ))),
    }
}

/// What a task reports for a Python module the run's union does not hold.
fn no_payload(module: &str) -> TaskResult {
    TaskResult::failed_with(format!(
        "module '{module}' needs a python payload, and this run built none for it: only the modules the compiled plays named are in the blob"
    ))
}

/// One sub-task of an action plugin, as the agent is asked to run it, or the failure the item
/// reports instead of sending it.
///
/// The module is the plugin's choice and its payload is the union's entry for it, under the
/// interpreter chosen for the host the link goes to, exactly as for a Python task the playbook
/// wrote. `ignore_errors`, `timeout` and `environment` are the item's.
///
/// A module the union does not hold fails by name, and is never sent as if it were native: every
/// module a plugin can pick is in the union by construction ([`crate::python::modules_to_build`]),
/// so reaching that arm is a controller bug, and `service` is a name the agent would take for a
/// module it runs itself. The agent's own `command`, `raw` and `shell` are the exception, sent
/// with no payload as a playbook's would be: `reboot` runs its commands through `raw`.
fn sub_task(
    task: &PlayTask,
    item: &Item,
    sub: &crate::action_plugins::Sub,
    union: Option<&Union>,
    interpreters: &[String],
    asked: Option<&str>,
) -> Result<Task, TaskResult> {
    if volant_protocol::modules::native(sub.module).is_some() {
        return Ok(Task {
            module: sub.module.to_string(),
            // A command a plugin runs is the connection's, which check mode never reaches: the
            // reference's `_low_level_execute_command` makes its temporary directory and removes
            // it in check mode too.
            args: {
                // The agent's own modules know no `_ansible_module_name`, which is for Python.
                let mut args = sub.args.clone();
                args.remove("_ansible_module_name");
                if sub.module == "raw" {
                    args
                } else {
                    with_modes(item, args)
                }
            },
            job: None,
            ..protocol_task(task, item, None)
        });
    }
    let Some(payload) = union.and_then(|union| union.payload(sub.module)) else {
        return Err(no_payload(sub.module));
    };
    let interpreter = chosen_interpreter(asked, interpreters)?;
    Ok(Task {
        module: sub.module.to_string(),
        args: with_modes(item, sub.args.clone()),
        ignore_errors: item.ignore_errors.unwrap_or_else(|| task.ignores_errors())
            || task.loop_items.is_some(),
        timeout: task.timeout,
        environment: item.environment.clone(),
        payload: Some(payload.under(&interpreter)),
        // Named only: the bytes go up just before the batch, and [`files_staged`] refuses the
        // task unless they went up for it.
        files: sub
            .files
            .iter()
            .map(|(arg, blob)| StagedFile {
                arg: arg.clone(),
                blob: blob.hash.clone(),
            })
            .collect(),
        force_python: payload.force_python,
        job: None,
    })
}

/// A plugin's result that ansible-core shaped as an unreachable host (`unreachable: true` and not
/// failed: a connection failure the plugin did not catch) ends the item with no result, as a link
/// that went away does, so the driver reports the host unreachable. The written plugins' own
/// `gone` results are failed ones and stay results.
fn unreachable_ending(result: TaskResult) -> Result<TaskResult, Result<BatchOutcome, String>> {
    if result.0.get("unreachable") != Some(&Value::Bool(true)) || result.failed() {
        return Ok(result);
    }
    let msg = result
        .0
        .get("msg")
        .and_then(Value::as_str)
        .unwrap_or("the plugin reported the host unreachable");
    Err(Err(msg.to_string()))
}

/// One item of a task an action plugin backs, run to its end.
///
/// Each sub-task the plugin asks for goes out alone, as a batch of one over the link the task
/// already has, and its result is handed back to the plugin on the next call. `Ok` is the item's
/// result, the one the plugin ended with; nothing the sub-tasks returned on the way leaves this
/// function, so the filtered `setup` a plugin asks for can never reach `record_facts`.
///
/// `Err` is the outcome that ended the host's batch instead - the link lost, the run interrupted -
/// with no result for the item, which the driver then handles the way it does for any batch. A
/// module that failed is not one of those: the plugin is handed its result like any other, so
/// the items behind it still run and `report_task` decides from all of them.
#[expect(
    clippy::too_many_arguments,
    reason = "the item, the plugin driving it, and the link and run state each sub-task goes over"
)]
#[expect(clippy::too_many_lines)]
pub(super) async fn run_plugin_item<C: AgentChannel, R: Relink<C>>(
    link: &mut C,
    relink: &mut R,
    host: &str,
    batch_id: &mut u64,
    plugin: &mut dyn crate::action_plugins::Plugin,
    task: &PlayTask,
    item: &Item,
    union: Option<&Union>,
    interpreters: &[String],
    asked: Option<&str>,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    logs: &mut Vec<String>,
) -> Result<TaskResult, Result<BatchOutcome, String>> {
    use crate::action_plugins::{Step, gone};
    use tokio::time::Instant;
    // The `timeout` keyword bounds the whole item, as the reference's alarm bounds the whole
    // action: every sub-task, every wait for the host to come back. Past it, the item's result is
    // the timeout's, never whatever the plugin would have made of the time it had left.
    let limit = task.timeout.filter(|t| *t > 0);
    let ends = limit.and_then(|t| Instant::now().checked_add(Duration::from_secs(t)));
    let expired = || ends.is_some_and(|e| Instant::now() >= e);
    let capped =
        |d: Duration| ends.map_or(d, |e| d.min(e.saturating_duration_since(Instant::now())));
    // A sub-task gets what is left of the item's time, in the whole seconds the agent counts in.
    let bounded = |mut built: Task| {
        if let Some(e) = ends {
            let left = e
                .saturating_duration_since(Instant::now())
                .as_secs_f64()
                .ceil();
            built.timeout = Some((left as u64).max(1));
        }
        built
    };
    let timeout_result = || TaskResult::timed_out(limit.unwrap_or(0));
    let mut last = None;
    // Tries in the reconnection in hand, which the pause before the next one grows with: the
    // plugin asks again after a try that failed and after a probe it was not satisfied with, and
    // the reference's loop counts both as failures.
    let mut tries: Option<u32> = None;
    loop {
        if expired() {
            sweep_abandoned(
                plugin,
                link,
                host,
                batch_id,
                task,
                item,
                union,
                interpreters,
                asked,
                stop,
                stop_broken,
                logs,
            )
            .await;
            return Ok(timeout_result());
        }
        let (sub, dropping) = match plugin.next(last.take()) {
            Step::Done(result) => return unreachable_ending(result),
            Step::Wait(ready) => {
                let mut watcher = stop.clone();
                let stopped = async move {
                    if watcher.wait_for(|stopped| *stopped).await.is_err() {
                        std::future::pending::<()>().await;
                    }
                };
                let until = async {
                    match ends {
                        Some(e) => tokio::time::sleep_until(e).await,
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    () = ready.notified() => {}
                    () = until => {}
                    () = stopped => return Err(Ok(BatchOutcome::Cancelled { at: 0 })),
                }
                continue;
            }
            Step::PutFile {
                bytes,
                remote,
                mode,
            } => {
                let put = put_file_within(
                    link,
                    host,
                    &bytes,
                    &remote,
                    mode,
                    logs,
                    capped(PUT_FILE_WAIT),
                )
                .await;
                last = Some(match put {
                    Ok(changed) => {
                        let mut done = Map::new();
                        done.insert("changed".into(), Value::Bool(changed));
                        TaskResult(done)
                    }
                    Err(why) => TaskResult::failed_with(why),
                });
                continue;
            }
            Step::Run(sub) => (sub, false),
            Step::RunDropping(sub) => (sub, true),
            Step::Reconnect {
                probe,
                wait,
                timeout,
                attempt,
            } => {
                if wait_or_stop(capped(wait), stop, stop_broken)
                    .await
                    .is_none()
                {
                    return Err(Ok(BatchOutcome::Cancelled { at: 0 }));
                }
                let deadline = Instant::now() + capped(timeout);
                let failed = tries.map_or(0, |t| t + 1);
                tries = Some(failed);
                // The plugin's own `sleep` is its pause between tries.
                let waiting = crate::action_plugins::waits_for_connection(&task.module);
                if failed > 0
                    && !waiting
                    && wait_or_stop(capped(relink.pause(failed).min(timeout)), stop, stop_broken)
                        .await
                        .is_none()
                {
                    return Err(Ok(BatchOutcome::Cancelled { at: 0 }));
                }
                if expired() {
                    return Ok(timeout_result());
                }
                // A host that was down from the start has no interpreter to name yet: the probe
                // is built again once a try has connected.
                let built = sub_task(task, item, &probe, union, interpreters, asked);
                if let (Err(failure), false) = (&built, link.stand_in()) {
                    return Ok(failure.clone());
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    // Nothing here awaits, so a plugin asking again with no time left would hold
                    // the runtime's thread for as long as it kept asking.
                    tokio::task::yield_now().await;
                    last = Some(gone("the host did not come back in time"));
                    continue;
                }
                // Watched on the side as well, since a try can take as long as `left` and an
                // interruption is not made to wait for it.
                let mut watcher = stop.clone();
                let stopped = async move {
                    if watcher.wait_for(|stopped| *stopped).await.is_err() {
                        std::future::pending::<()>().await;
                    }
                };
                let sent_before = *batch_id;
                let once = async {
                    relink.relink(link, attempt).await?;
                    let built = match built {
                        Ok(built) => built,
                        Err(failure) => {
                            match sub_task(task, item, &probe, union, &link.interpreters(), asked) {
                                Ok(built) => built,
                                Err(_) => return Ok(Some(failure)),
                            }
                        }
                    };
                    // What is left once the connection is up, not before it.
                    let built = bounded(built);
                    *batch_id += 1;
                    let (mut flat, ended) =
                        send_batch(link, host, *batch_id, vec![built], stop, stop_broken, logs)
                            .await;
                    let answer: Result<Option<TaskResult>, String> = match ended {
                        Ok(BatchOutcome::Cancelled { .. }) => Ok(None),
                        Ok(_) => flat.pop().flatten().map(Some).ok_or_else(|| {
                            "the agent ended the probe without its result".to_string()
                        }),
                        Err(err) => Err(err),
                    };
                    answer
                };
                let tried = tokio::select! {
                    tried = tokio::time::timeout(left, once) => tried,
                    () = stopped => return Err(Ok(BatchOutcome::Cancelled { at: 0 })),
                };
                last = Some(match tried {
                    Ok(Ok(Some(result))) => result,
                    Ok(Ok(None)) => return Err(Ok(BatchOutcome::Cancelled { at: 0 })),
                    // The connection's own error, as the plugin's loop words it: the reference
                    // names the error, not the task that failed on it.
                    Ok(Err(why)) if waiting => {
                        gone(why.strip_prefix("Task failed: ").unwrap_or(&why))
                    }
                    Ok(Err(why)) => gone(&why),
                    Err(_) => {
                        // A probe that went out may still be running on the host, and the agent
                        // drops any `RunBatch` that arrives while a batch runs: left there, it
                        // would swallow the next sub-task and leave it unanswered. Cancelled, or,
                        // when the agent does not say it stopped, the link replaced like one the
                        // host outlived.
                        // Both waits past the deadline still give way to an interruption.
                        if *batch_id != sent_before {
                            let id = *batch_id;
                            let mut watcher = stop.clone();
                            let stopped = async move {
                                if watcher.wait_for(|stopped| *stopped).await.is_err() {
                                    std::future::pending::<()>().await;
                                }
                            };
                            let settle = async {
                                if link.stop_batch(id, CANCEL_GRACE).await {
                                    return Ok(());
                                }
                                relink.relink(link, attempt).await
                            };
                            tokio::select! {
                                settled = settle => settled.map_err(|why| {
                                    Err(format!("replacing a link busy with a probe: {why}"))
                                })?,
                                () = stopped => return Err(Ok(BatchOutcome::Cancelled { at: 0 })),
                            }
                        }
                        gone(&format!("no answer after {} seconds", left.as_secs_f64()))
                    }
                });
                continue;
            }
        };
        tries = None;
        let built = match sub_task(task, item, &sub, union, interpreters, asked) {
            Ok(built) => built,
            // The ping of a host that was down from the start: the plugin's own loop goes on to
            // reconnect, and names the interpreter once the host answers.
            Err(_) if dropping && link.stand_in() => {
                last = Some(gone("the host has not answered yet"));
                continue;
            }
            Err(failure) => return Ok(failure),
        };
        *batch_id += 1;
        let tasks = vec![built];
        // What the sub-task needs on the host and could not be put there fails the item as it
        // stands, and the plugin never sees it: the module did not run, and a plugin handed the
        // error as the module's result would dress it as one (`copy` adds `diff` and the local
        // checksum). The reference's action fails the same way on a transfer that raised.
        if let Err(err) = blob_preflight(link, host, &tasks, union, &sub.files, logs).await {
            return Ok(TaskResult::failed_with(err));
        }
        // The transfer ran on the item's time. Past its end the module is not sent, so a `copy`
        // whose upload outlived `timeout` leaves `dest` alone and reports the timeout; before
        // it, the module gets what is left after the transfer.
        if expired() {
            sweep_staged(
                link,
                host,
                batch_id,
                &tasks[0],
                union,
                stop,
                stop_broken,
                logs,
            )
            .await;
            return Ok(timeout_result());
        }
        let tasks: Vec<Task> = tasks.into_iter().map(bounded).collect();
        let (mut flat, ended) =
            send_batch(link, host, *batch_id, tasks, stop, stop_broken, logs).await;
        let result = flat.pop().flatten();
        match ended {
            Ok(BatchOutcome::Completed | BatchOutcome::Failed { .. }) => {}
            // The link went, which a sub-task that may take it down reports to the plugin
            // instead of ending the host's run - with the sub-task's own result when it arrived
            // before the link went.
            Err(err) if dropping => {
                last = Some(result.unwrap_or_else(|| gone(&err)));
                continue;
            }
            _ => return Err(ended),
        }
        // A batch the agent ended cleanly with no result in it has lost a sub-task's result
        // between two turns. Handed to the plugin as nothing, it would be read as the module's
        // own answer; the item fails naming it instead.
        let Some(result) = result else {
            return Ok(TaskResult::failed_with(format!(
                "the agent ended the '{}' sub-task without its result",
                sub.module
            )));
        };
        // The `timeout` keyword bounds the reference's whole action, and running out of it ends
        // the action there: the item's result is the timeout, never something the plugin made of
        // it.
        if timed_out(&result) {
            return Ok(if limit.is_some() {
                timeout_result()
            } else {
                result
            });
        }
        last = Some(result);
    }
}

/// How long each removal of a directory a plugin left behind may take, the item's time being up.
const SWEEP_WAIT: u64 = 10;

/// Runs what a plugin left on the host when its item ran out of time, whatever each answers. A
/// link that breaks on the way is left for the next batch to find.
#[expect(
    clippy::too_many_arguments,
    reason = "run_plugin_item's link and run state, which each removal goes over"
)]
async fn sweep_abandoned<C: AgentChannel>(
    plugin: &mut dyn crate::action_plugins::Plugin,
    link: &mut C,
    host: &str,
    batch_id: &mut u64,
    task: &PlayTask,
    item: &Item,
    union: Option<&Union>,
    interpreters: &[String],
    asked: Option<&str>,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    logs: &mut Vec<String>,
) {
    for sub in plugin.abandoned() {
        let Ok(mut built) = sub_task(task, item, &sub, union, interpreters, asked) else {
            continue;
        };
        built.timeout = Some(SWEEP_WAIT);
        *batch_id += 1;
        let (_, ended) =
            send_batch(link, host, *batch_id, vec![built], stop, stop_broken, logs).await;
        if let Err(why) = ended {
            logs.push(format!(
                "removing what an action plugin left on the host: {why}"
            ));
            return;
        }
    }
}

/// What starting an action plugin reads besides the item it runs and the warnings it raises.
pub(super) struct PluginStart<'a> {
    pub(super) kind: crate::action_plugins::Kind,
    /// The variables of the host the module runs on - the delegate's when there is one.
    pub(super) running_vars: &'a HostVars,
    pub(super) delegated: bool,
    /// Whether the task escalates: the link its sub-tasks go over is the escalated one.
    pub(super) escalated: bool,
    /// Whether that link runs the agent on the controller itself.
    pub(super) local: bool,
    pub(super) templar: &'a Templar,
    pub(super) origin: &'a crate::compile::Origin,
    pub(super) playbook_dir: &'a std::path::Path,
}

/// One item of a task an action plugin backs, attempt after attempt under `retries`/`until`.
///
/// Each attempt starts the plugin afresh, so the whole sequence of sub-tasks runs again from its
/// first, and a file the plugin stages is read and sent again: the agent consumed the one the
/// last attempt staged. `until` reads the result the sequence ended with, once `changed_when`
/// and `failed_when` have judged it, which is the rule a module's retry follows. Measured on
/// ansible-core 2.19.12: `retries: 3` with no `until` on a `copy` that succeeds is `attempts=1`,
/// and `retries: 2` on one that fails prints `FAILED - RETRYING ... (2 retries left)` then
/// `(1 retries left)` and fails with `"attempts": 2`.
///
/// With no `retry`, one attempt, and the result comes back unjudged, the way the plugin path
/// has always handed it to the reporting loop. `Ok(None)` is a run stopped while it waited
/// between two attempts: nothing more runs, and the item has no result. `lefts` receives each
/// retry line.
#[expect(
    clippy::too_many_arguments,
    reason = "run_plugin_item's arguments, plus the retry plan and the lines it prints"
)]
pub(super) async fn run_plugin_attempts<C: AgentChannel, R: Relink<C>>(
    link: &mut C,
    relink: &mut R,
    host: &str,
    batch_id: &mut u64,
    start: &PluginStart<'_>,
    task: &PlayTask,
    item: &Item,
    retry: Option<&Retry>,
    union: Option<&Union>,
    interpreters: &[String],
    asked: Option<&str>,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    logs: &mut Vec<String>,
    warnings: &mut Vec<String>,
    lefts: &mut Vec<RetryLine>,
) -> Result<Option<TaskResult>, Result<BatchOutcome, String>> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        // The warnings of the attempt whose result is shown, not one copy per attempt.
        warnings.clear();
        let raw = {
            let mut plugin = crate::action_plugins::start(
                start.kind,
                crate::action_plugins::Context {
                    args: &item.args,
                    args_untrusted: &item.args_untrusted,
                    running_vars: start.running_vars,
                    delegated: start.delegated,
                    escalated: start.escalated,
                    local: start.local,
                    item_vars: &item.vars,
                    templar: start.templar,
                    origin: start.origin,
                    playbook_dir: start.playbook_dir,
                    warnings: &mut *warnings,
                    environment: Some(&item.environment),
                    timeout: task.timeout,
                    check_mode: item.check_mode == Some(true),
                    diff: item.diff == Some(true),
                },
            );
            run_plugin_item(
                link,
                relink,
                host,
                batch_id,
                plugin.as_mut(),
                task,
                item,
                union,
                interpreters,
                asked,
                stop,
                stop_broken,
                logs,
            )
            .await?
        };
        let Some(retry) = retry else {
            return Ok(Some(raw));
        };
        match judge_attempt(task, item, raw, attempt, retry, start.templar) {
            Attempt::Done(r) => return Ok(Some(r)),
            Attempt::Again(left, failed) => lefts.push((left, failed)),
        }
        if let Some(failed) = retry.sleep_failure() {
            return Ok(Some(failed));
        }
        if wait_or_stop(retry.delay, stop, stop_broken).await.is_none() {
            return Ok(None);
        }
    }
}

/// Takes off the host the files a sub-task staged and never ran with, the item's deadline having
/// passed during the transfer: a staged file can hold a rendered secret, and would otherwise stay
/// until the link closes. A `stat` stages them under `path`, and the agent removes every staged
/// file once its task ran; the reference's temporary directory goes when its alarm ends the
/// action (`TaskExecutor`'s `cleanup`). Nothing is read of the answer.
#[expect(
    clippy::too_many_arguments,
    reason = "the link, the batch counter and the run's interruption a batch needs"
)]
async fn sweep_staged<C: AgentChannel>(
    link: &mut C,
    host: &str,
    batch_id: &mut u64,
    built: &Task,
    union: Option<&Union>,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    logs: &mut Vec<String>,
) {
    let (Some(ran_under), Some(stat)) = (&built.payload, union.and_then(|u| u.payload("stat")))
    else {
        return;
    };
    if built.files.is_empty() {
        return;
    }
    let args = ["follow", "get_checksum", "get_mime", "get_attributes"]
        .into_iter()
        .map(|key| (key.to_string(), json!(false)))
        .collect();
    let sweep = Task {
        module: "stat".into(),
        args,
        timeout: Some(CANCEL_GRACE.as_secs()),
        force_python: stat.force_python,
        payload: Some(stat.under(&ran_under.interpreter)),
        files: built
            .files
            .iter()
            .map(|file| StagedFile {
                arg: "path".into(),
                blob: file.blob.clone(),
            })
            .collect(),
        ..Task::default()
    };
    *batch_id += 1;
    let _ = send_batch(link, host, *batch_id, vec![sweep], stop, stop_broken, logs).await;
}

/// Waits `delay`, or gives up when the run is interrupted, before it or during it. A zero delay
/// still looks at the stop first, so a retry loop with no delay stops between two attempts
/// rather than starting the next one.
pub(super) async fn wait_or_stop(
    delay: Duration,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
) -> Option<()> {
    // `sleep` rather than a sum with `now`, which would overflow for a delay that large.
    let sleep = tokio::time::sleep(delay);
    tokio::pin!(sleep);
    loop {
        if *stop.borrow() {
            return None;
        }
        if delay.is_zero() {
            return Some(());
        }
        tokio::select! {
            () = &mut sleep => return Some(()),
            res = stop.changed(), if !*stop_broken => {
                if res.is_err() {
                    *stop_broken = true;
                } else {
                    return None;
                }
            }
        }
    }
}

/// One item of one task, as the agent is asked to run it.
///
/// `python` is the module's half of a payload together with the interpreter chosen for the host
/// this item is going to. The two arrive as one because this is where the wire payload is built,
/// and a payload is only ever built where both halves are in hand: the module's half is the same
/// on every host, the interpreter is not, and one sent under an interpreter nobody chose runs
/// something other than what was asked for.
pub(super) fn protocol_task(
    task: &PlayTask,
    item: &Item,
    python: Option<(&ModulePayload, &str)>,
) -> Task {
    let mut args = item.args.clone();
    // The terminal is the controller's to give, never a task's to ask for. `raw` is the one
    // playbook module the reference runs through `ssh -tt`: it sends no stdin.
    args.remove(TTY_KEY);
    if short_name(&task.module) == "raw" {
        args.insert(TTY_KEY.into(), Value::Bool(true));
    }
    // What the reference's `async_status` action plugin adds, whatever the task wrote there.
    if short_name(&task.module) == "async_status" {
        args.insert("_async_dir".into(), Value::String(async_dir(&item.vars)));
    }
    Task {
        module: task.module.clone(),
        args: with_modes(item, args),
        // An item is not the task: a failing item never stops the ones behind it, exactly as in
        // Ansible. The task's own failure is decided later, by `report_task`, from every item's
        // result.
        ignore_errors: item.ignore_errors.unwrap_or_else(|| task.ignores_errors())
            || task.loop_items.is_some(),
        timeout: task.timeout,
        environment: item.environment.clone(),
        // One payload for every item of the task: a loop varies the arguments, never the
        // module, and the arguments travel outside the blob.
        payload: python.map(|(module, interpreter)| module.under(interpreter)),
        files: Vec::new(),
        force_python: python.is_some_and(|(module, _)| module.force_python),
        job: (task.async_val > 0).then(|| volant_protocol::AsyncJob {
            limit: task.async_val,
            dir: async_dir(&item.vars),
        }),
    }
}

/// `args` with what ansible-core's `_update_module_args` adds for the task's check and diff
/// modes, sent only when on. Whatever the playbook wrote under those names goes, as the reference
/// overwrites it: a task cannot talk a module out of check mode through its arguments.
fn with_modes(item: &Item, mut args: Map<String, Value>) -> Map<String, Value> {
    for (key, on) in [(CHECK_MODE_ARG, item.check_mode), (DIFF_ARG, item.diff)] {
        args.remove(key);
        if on == Some(true) {
            args.insert(key.into(), Value::Bool(true));
        }
    }
    args
}

/// The key a started job's id comes back under, in a result rather than among a host's variables.
const JOB_ID: &str = "ansible_job_id";

/// Seconds between two looks at a job when `poll` is not written: the reference's
/// `DEFAULT_POLL_INTERVAL`.
const DEFAULT_POLL: u64 = 15;

/// Whether this task's job is waited for here rather than only started.
pub(super) fn polls(task: &PlayTask) -> bool {
    task.async_val > 0 && task.poll.unwrap_or(DEFAULT_POLL) > 0
}

/// What waiting for a started job came to.
pub(super) enum Polled {
    /// The job's last status, or the failure the reference makes of a job that did not finish.
    Done(TaskResult),
    /// The run was interrupted between two looks.
    Stopped,
    /// A look went out in a batch that ended without its result: the link was lost.
    Ended(Result<BatchOutcome, String>),
}

/// A started job, asked about every `poll` seconds until it says it finished or `async` seconds
/// are spent, then its status file removed: ansible-core 2.19.12's
/// `TaskExecutor._poll_async_result`, one `async_status` batch per look over the task's own link.
/// The `ASYNC POLL`, `ASYNC OK` and `ASYNC FAILED` lines go on the queue as they are decided.
#[expect(
    clippy::too_many_arguments,
    reason = "the job, the link its looks go over, and the queue its lines go on"
)]
#[expect(clippy::too_many_lines)]
pub(super) async fn poll_job<C: AgentChannel>(
    link: &mut C,
    host: &str,
    batch_id: &mut u64,
    task: &PlayTask,
    item: &Item,
    started: TaskResult,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    logs: &mut Vec<String>,
    tx: &tokio::sync::mpsc::Sender<super::coordinator::Event>,
    index: usize,
) -> Polled {
    let truthy =
        |result: &TaskResult, key: &str| result.0.get(key).is_some_and(|v| !python_falsy(v));
    // A start that failed or was skipped is the task's result, with no line: only a started
    // job is polled.
    if truthy(&started, "failed") || truthy(&started, "skipped") {
        return Polled::Done(started);
    }
    let show = |line: String| {
        tx.send(super::coordinator::Event::Display {
            host: host.to_string(),
            index,
            line,
            debug: true,
        })
    };
    let Some(jid) = started
        .0
        .get(JOB_ID)
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        let _ = show(format!("ASYNC FAILED on {host}: jid=None")).await;
        return Polled::Done(TaskResult::failed_with(
            "No job id was returned by the async task",
        ));
    };
    let ask = |mode: &str| {
        let mut args = Map::new();
        args.insert("jid".into(), Value::String(jid.clone()));
        args.insert("mode".into(), Value::String(mode.into()));
        args.insert("_async_dir".into(), Value::String(async_dir(&item.vars)));
        Task {
            module: "async_status".into(),
            args,
            environment: item.environment.clone(),
            ..Task::default()
        }
    };
    let poll = task.poll.unwrap_or(DEFAULT_POLL);
    let mut left = task.async_val;
    let mut last = None;
    while left > 0 {
        if wait_or_stop(Duration::from_secs(poll), stop, stop_broken)
            .await
            .is_none()
        {
            return Polled::Stopped;
        }
        *batch_id += 1;
        let (mut flat, ended) = run_agent_batch(
            link,
            host,
            *batch_id,
            vec![ask("status")],
            None,
            &[],
            stop,
            stop_broken,
            logs,
        )
        .await;
        if !matches!(
            ended,
            Ok(BatchOutcome::Completed | BatchOutcome::Failed { .. })
        ) {
            return Polled::Ended(ended);
        }
        let Some(status) = flat.pop().flatten() else {
            return Polled::Done(TaskResult::failed_with(format!(
                "the agent answered no status for job {jid}"
            )));
        };
        // Every answer the agent gives is a parsed one, so a failure ends the wait as the
        // reference's does for a result it could parse.
        if ["finished", "failed", "skipped"]
            .iter()
            .any(|key| truthy(&status, key))
        {
            last = Some(status);
            break;
        }
        left = left.saturating_sub(poll);
        let _ = show(format!(
            "ASYNC POLL on {host}: jid={} started={} finished={}",
            python_repr(status.0.get(JOB_ID)),
            python_repr(status.0.get("started")),
            python_repr(status.0.get("finished")),
        ))
        .await;
        last = Some(status);
    }
    let result = match last {
        Some(status) if truthy(&status, "finished") => {
            // Removed the way the reference removes it, its answer unread.
            *batch_id += 1;
            let _ = run_agent_batch(
                link,
                host,
                *batch_id,
                vec![ask("cleanup")],
                None,
                &[],
                stop,
                stop_broken,
                logs,
            )
            .await;
            status
        }
        last => {
            let mut failed = TaskResult::failed_with(format!(
                "async task did not complete within the requested time - {}s",
                task.async_val
            ));
            // The status as the action plugin answered it, before the executor's `changed`
            // default the agent adds: measured, the reference's `async_result` has none.
            let mut status = last.map(|status| status.0).unwrap_or_default();
            if status.get("changed") == Some(&Value::Bool(false)) {
                status.remove("changed");
            }
            failed
                .0
                .insert("async_result".into(), Value::Object(status));
            failed.0.insert("changed".into(), Value::Bool(false));
            failed
        }
    };
    let shown = result.0.get(JOB_ID).or_else(|| {
        result
            .0
            .get("async_result")
            .and_then(|inner| inner.get(JOB_ID))
    });
    let verdict = if truthy(&result, "failed") {
        "FAILED"
    } else {
        "OK"
    };
    let _ = show(format!(
        "ASYNC {verdict} on {host}: jid={}",
        python_repr(shown)
    ))
    .await;
    Polled::Done(result)
}

/// A value as Python's `%s` prints it: `True`, `None`, a string bare.
fn python_repr(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".into(),
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::Bool(false)) => "False".into(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

/// The reference's default `async_dir`, which its `sh` shell plugin reads.
const DEFAULT_ASYNC_DIR: &str = "~/.ansible_async";

/// Where a host keeps its jobs' status files: `ansible_async_dir`, then `ANSIBLE_ASYNC_DIR`,
/// then the default, as the shell plugin's option is resolved. Expanded on the host.
pub(super) fn async_dir(vars: &HostVars) -> String {
    let set = |value: Option<String>| value.filter(|dir| !dir.trim().is_empty());
    set(crate::vars::host_setting(vars, "ansible_async_dir")
        .and_then(Value::as_str)
        .map(str::to_string))
    .or_else(|| set(std::env::var("ANSIBLE_ASYNC_DIR").ok()))
    .unwrap_or_else(|| DEFAULT_ASYNC_DIR.to_string())
}

/// What running a batch needs of a link: the memory the link carries, the two directions of the
/// wire, and the way a batch is stopped.
///
/// A trait rather than the link itself so a batch can be run against an agent that answers what
/// a test scripted - a real link is a child process, which is why none of this was testable. The three ways it goes wrong - a payload refused, a payload
/// sent twice, a batch sent for a payload nobody confirmed - are all failures nothing else
/// would show until a host ran the wrong thing.
pub(super) trait AgentChannel {
    fn memory(&mut self) -> &mut BlobMemory;
    async fn ask(&mut self, msg: &ToAgent) -> std::io::Result<()>;
    /// Sends the blob frame a `put_blob` is followed by.
    async fn ask_bytes(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    async fn answer(&mut self) -> std::io::Result<Option<FromAgent>>;
    /// Asks for a batch to stop and waits for the agent to say it has, at most `grace`.
    async fn stop_batch(&mut self, id: u64, grace: Duration) -> bool;
    /// Whether this channel stands in for a host that never answered.
    fn stand_in(&self) -> bool {
        false
    }
    /// The interpreters the agent behind this channel found, empty when it has none to tell.
    fn interpreters(&self) -> Vec<String> {
        Vec::new()
    }
    /// Where the batches over this channel leave their timings, when anything reads them.
    fn timings(&mut self) -> Option<&mut crate::profile::Ledger> {
        None
    }
    /// Where to look for what a stopped agent said; see `AgentLink::stderr_note`.
    fn stderr_note(&self) -> String {
        String::new()
    }
    /// The line a command given a terminal ends its stderr with; see `SshTarget::tty_close`.
    async fn tty_close(&mut self) -> Option<String> {
        None
    }
    /// Gives the link up after a question went unanswered: its answer may still arrive, and the
    /// next task would read it as its own. The link is dead afterwards.
    fn abandon(&mut self) {}
}

/// How a plugin that expects its host to go away gets a link to it again.
pub(super) trait Relink<C> {
    /// Replaces `link` with a fresh one to the same host, under the same user, after dropping
    /// every other link kept for that host: a link the host outlived is of no use to anybody.
    /// `link` is left as it was when no fresh one could be opened. `connect_timeout` bounds the
    /// connection's own setup, as the reference's `connect_timeout` sets the connection plugin's.
    async fn relink(
        &mut self,
        link: &mut C,
        connect_timeout: Option<Duration>,
    ) -> Result<(), String>;

    /// How long to wait before the next try, after `failed` of them: the reference's doubling
    /// from one second, capped at twelve (`reboot.py`, which adds up to a second at random).
    fn pause(&self, failed: u32) -> Duration {
        Duration::from_secs(1 << failed.saturating_sub(1).min(4)).min(Duration::from_secs(12))
    }
}

/// [`Relink`] for the driver's links: `key` is the one the batch runs on, taken out of `links`
/// while the plugin holds it.
pub(super) struct Relinker<'a> {
    pub(super) links: &'a mut HashMap<LinkKey, AgentLink>,
    /// Each link's key, with the host's reboot count when the link was last proved alive.
    pub(super) checked: &'a mut HashMap<LinkKey, u64>,
    pub(super) reboots: &'a Reboots,
    pub(super) key: &'a LinkKey,
    pub(super) escalation: Option<&'a Escalation>,
    pub(super) agents: &'a AgentSource,
    pub(super) defaults: &'a ConnectionDefaults,
}

impl Relink<AgentLink> for Relinker<'_> {
    async fn relink(
        &mut self,
        link: &mut AgentLink,
        connect_timeout: Option<Duration>,
    ) -> Result<(), String> {
        // Before anything else, so that every other driver checks its own link to this host
        // again whether or not this try comes up.
        self.reboots.bump(&self.key.host);
        // Closed off-task, as a replaced escalated link is: their agents died with the host.
        for old in retire_host_links(self.links, self.checked, &self.key.host) {
            tokio::spawn(old.shutdown());
        }
        // Checked again by the next batch if this never comes back up: the link left in place is
        // the one the host outlived.
        self.checked.remove(self.key);
        // The host's shared connection went down with it, perhaps without a word: a master left
        // on a dead TCP connection would take this try's session and hold it past any timeout.
        // Stopped and reopened through the one transport, so both name the same socket.
        let transport = with_connect_timeout(&self.key.transport, connect_timeout);
        transport.stop_shared().await;
        let fresh = connect(&transport, self.agents, self.defaults, self.escalation)
            .await
            .map_err(|e| e.to_string())?;
        let old = std::mem::replace(link, fresh);
        tokio::spawn(old.shutdown());
        Ok(())
    }
}

/// `transport` with ssh's `ConnectTimeout` set to `timeout`, in the whole seconds it takes and at
/// least one; a local transport has no connection to set up.
fn with_connect_timeout(transport: &Transport, timeout: Option<Duration>) -> Transport {
    let mut transport = transport.clone();
    if let (Some(timeout), Transport::Ssh(target)) = (timeout, &mut transport) {
        target.connect_timeout = Duration::from_secs((timeout.as_secs_f64().ceil() as u64).max(1));
    }
    transport
}

impl AgentChannel for AgentLink {
    fn memory(&mut self) -> &mut BlobMemory {
        self.blobs()
    }

    async fn ask(&mut self, msg: &ToAgent) -> std::io::Result<()> {
        self.send(msg).await
    }

    async fn ask_bytes(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.send_bytes(bytes).await
    }

    async fn answer(&mut self) -> std::io::Result<Option<FromAgent>> {
        self.recv().await
    }

    async fn stop_batch(&mut self, id: u64, grace: Duration) -> bool {
        self.cancel(id, grace).await
    }

    fn interpreters(&self) -> Vec<String> {
        AgentLink::interpreters(self).to_vec()
    }

    fn stand_in(&self) -> bool {
        self.is_stand_in()
    }

    fn timings(&mut self) -> Option<&mut crate::profile::Ledger> {
        Some(self.ledger())
    }

    fn stderr_note(&self) -> String {
        AgentLink::stderr_note(self)
    }

    async fn tty_close(&mut self) -> Option<String> {
        AgentLink::tty_close(self).await
    }

    fn abandon(&mut self) {
        AgentLink::abandon(self);
    }
}

/// Makes sure the agent behind this link holds the payload named `hash`, sending it only when
/// it does not.
///
/// Once per link, never once per batch: the agent answers `has_blob` by hashing the file it
/// has, so a yes means the bytes are right, and the 631 KB of a union blob has no business
/// going up again for every batch of a play. A refusal is remembered as a refusal, so the
/// batches behind it fail on what the agent said rather than sending the same rejected payload
/// again.
pub(super) async fn ensure_blob<C: AgentChannel>(
    link: &mut C,
    host: &str,
    hash: &str,
    zip_b64: &str,
    logs: &mut Vec<String>,
) -> Result<(), String> {
    if let Some(seen) = link.memory().seen(hash) {
        return seen;
    }
    // Deflated once per run, when the union cache stored or loaded the entry.
    let frame = crate::agent::frame(hash, zip_b64);
    let state = match place_blob(link, host, hash, frame, false, logs).await {
        Ok(true) => Ok(()),
        // The agent logs why on its way to saying no, and that line is already in `logs`.
        Ok(false) => Err(format!(
            "the agent refused the module payload {hash}; it holds no payload to run this task from"
        )),
        Err(err) => Err(err),
    };
    link.memory().remember(hash, state.clone());
    state
}

/// Asks whether the agent holds `hash`, and sends it when it does not: whether it holds it now.
/// A `staged` file is sent without asking. `frame` is only awaited when the blob goes up.
///
/// Remembers nothing. That is [`ensure_blob`]'s to do for a payload, and is never done for a
/// file a sub-task stages: the agent takes that one out of its cache to hand it to the module,
/// so a link that remembered it would send the next task after a file that is no longer there.
/// Nor is a file asked for: two links to one host account share the agent's cache, and a yes
/// about a file the other link is about to consume is the same stale answer.
async fn place_blob<C: AgentChannel>(
    link: &mut C,
    host: &str,
    hash: &str,
    frame: impl Future<Output = Result<std::sync::Arc<BlobFrame>, String>>,
    staged: bool,
    logs: &mut Vec<String>,
) -> Result<bool, String> {
    let has = ToAgent::HasBlob { hash: hash.into() };
    if !staged && blob_state(link, host, hash, &has, None, logs).await? {
        return Ok(true);
    }
    let frame = frame.await?;
    let put = ToAgent::PutBlob {
        hash: hash.into(),
        len: frame.len,
        encoding: frame.encoding,
        staged,
    };
    blob_state(link, host, hash, &put, Some(&frame.bytes), logs).await
}

/// Puts on the host every file a sub-task stages, whatever the link put there before: the
/// hashes the agent now holds, or why one is not there, in the agent's own words.
async fn stage_files<C: AgentChannel>(
    link: &mut C,
    host: &str,
    files: &[(String, crate::action_plugins::FileBlob)],
    logs: &mut Vec<String>,
) -> Result<BTreeSet<String>, String> {
    let mut placed = BTreeSet::new();
    for (arg, blob) in files {
        let before = logs.len();
        // Framed once for every host that is sent the same bytes.
        let frame = crate::agent::frame(&blob.hash, &blob.b64);
        let placed_now = place_blob(link, host, &blob.hash, frame, true, logs)
            .await
            .map_err(|err| format!("staging the file for '{arg}': {err}"))?;
        if !placed_now {
            let prefix = format!("[{host}] ");
            let why: Vec<&str> = logs[before..]
                .iter()
                .map(|line| line.strip_prefix(&prefix).unwrap_or(line))
                .collect();
            let why = if why.is_empty() {
                "it gave no reason".to_string()
            } else {
                why.join("; ")
            };
            return Err(format!(
                "the agent could not store the file for '{arg}' ({}): {why}",
                blob.hash
            ));
        }
        placed.insert(blob.hash.clone());
    }
    Ok(placed)
}

/// How long the agent has to move a staged file into place. The move is a local copy and a
/// rename; a host that takes longer than this is hung, and the task fails rather than waits.
const PUT_FILE_WAIT: Duration = Duration::from_secs(120);

/// Puts `bytes` at the absolute path `remote` of the host, with `mode` (0600 when `None`):
/// staged first, then moved into place by the agent through a temporary file and a rename.
/// Whether the content differs from what the path held.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the plugins put files with the item's time as the patience"
    )
)]
pub(super) async fn put_file<C: AgentChannel>(
    link: &mut C,
    host: &str,
    bytes: &[u8],
    remote: &str,
    mode: Option<u32>,
    logs: &mut Vec<String>,
) -> Result<bool, String> {
    put_file_within(link, host, bytes, remote, mode, logs, PUT_FILE_WAIT).await
}

/// [`put_file`] with the agent's time to move the file given.
async fn put_file_within<C: AgentChannel>(
    link: &mut C,
    host: &str,
    bytes: &[u8],
    remote: &str,
    mode: Option<u32>,
    logs: &mut Vec<String>,
    patience: Duration,
) -> Result<bool, String> {
    let hash = blake3::hash(bytes).to_hex().to_string();
    let before = logs.len();
    let frame = async { Ok(std::sync::Arc::new(BlobFrame::packed(bytes.to_vec()))) };
    let placed = place_blob(link, host, &hash, frame, true, logs)
        .await
        .map_err(|err| format!("staging the file for '{remote}': {err}"))?;
    if !placed {
        let prefix = format!("[{host}] ");
        let why: Vec<&str> = logs[before..]
            .iter()
            .map(|line| line.strip_prefix(&prefix).unwrap_or(line))
            .collect();
        let why = if why.is_empty() {
            "it gave no reason".to_string()
        } else {
            why.join("; ")
        };
        return Err(format!(
            "the agent could not store the file for '{remote}' ({hash}): {why}"
        ));
    }
    let put = ToAgent::PutFile {
        hash: hash.clone(),
        path: remote.into(),
        mode,
    };
    link.ask(&put)
        .await
        .map_err(|err| format!("asking the agent to put the file at '{remote}': {err}"))?;
    let wait = async {
        loop {
            match link.answer().await {
                Ok(Some(FromAgent::PutFileDone {
                    hash: named,
                    changed,
                    error,
                })) if named == hash => {
                    return match error {
                        None => Ok(changed),
                        Some(why) => Err(why),
                    };
                }
                Ok(Some(FromAgent::Log { message, .. })) => {
                    logs.push(format!("[{host}] {message}"));
                }
                Ok(Some(FromAgent::Ready { .. })) => {}
                Ok(Some(other)) => {
                    return Err(format!(
                        "the agent answered {other:?} while asked to put the file at '{remote}'"
                    ));
                }
                Ok(None) => {
                    return Err(format!(
                        "the agent stopped while putting the file at '{remote}'"
                    ));
                }
                Err(err) => {
                    return Err(format!(
                        "reading the agent's answer about the file at '{remote}': {err}"
                    ));
                }
            }
        }
    };
    if let Ok(done) = tokio::time::timeout(patience, wait).await {
        done
    } else {
        link.abandon();
        Err(format!(
            "the agent did not put the file at '{remote}' within {} seconds",
            patience.as_secs()
        ))
    }
}

/// Asks one question about a blob and reads the one answer to it.
///
/// Anything else the agent says on the way is either a line to show or a message that does not
/// belong to this exchange; an answer about another blob is the streams having desynchronised,
/// which is a hard error rather than something to read as a yes. The words say "blob" because
/// the blob may be a module payload or a file a sub-task stages; each caller says which.
async fn blob_state<C: AgentChannel>(
    link: &mut C,
    host: &str,
    hash: &str,
    msg: &ToAgent,
    frame: Option<&[u8]>,
    logs: &mut Vec<String>,
) -> Result<bool, String> {
    link.ask(msg)
        .await
        .map_err(|err| format!("asking the agent about the blob {hash}: {err}"))?;
    if let Some(frame) = frame {
        link.ask_bytes(frame)
            .await
            .map_err(|err| format!("sending the blob {hash}: {err}"))?;
    }
    loop {
        match link.answer().await {
            Ok(Some(FromAgent::BlobState {
                hash: named,
                present,
            })) if named == hash => {
                return Ok(present);
            }
            Ok(Some(FromAgent::BlobState { hash: named, .. })) => {
                return Err(format!(
                    "the agent answered about the blob {named} while asked about {hash}"
                ));
            }
            Ok(Some(FromAgent::Log { message, .. })) => logs.push(format!("[{host}] {message}")),
            // A frame belonging to a batch, read while a blob question is outstanding, means the
            // two streams are out of step: skipping it would leave the next batch's answers
            // being read as this question's, and a stale `BlobState` behind it would pass for a
            // yes. Latent until something runs two batches over a link whose first did not
            // finish - which is what this pull request adds paths for.
            Ok(Some(FromAgent::TaskResult { batch, index, .. })) => {
                return Err(format!(
                    "the agent sent the result of task {index} of batch {batch} while asked about the blob {hash}"
                ));
            }
            Ok(Some(FromAgent::BatchDone { batch, .. })) => {
                return Err(format!(
                    "the agent ended batch {batch} while asked about the blob {hash}"
                ));
            }
            Ok(Some(FromAgent::Ready { .. })) => {}
            Ok(Some(FromAgent::PutFileDone { .. })) => {
                return Err(format!(
                    "the agent ended a put_file while asked about the blob {hash}"
                ));
            }
            Ok(None) => {
                return Err(format!(
                    "the agent stopped while being asked about the blob {hash}"
                ));
            }
            Err(err) => {
                return Err(format!(
                    "reading the agent's answer about the blob {hash}: {err}"
                ));
            }
        }
    }
}

/// Every payload in a batch names a blob this link confirmed.
///
/// A task naming anything else is a controller that built a payload and never made sure the
/// host had it - a bug here, not a fault of the host - so it fails naming the hash instead of
/// being sent for the agent to fail on, where it would read as the module's own failure.
pub(super) fn payloads_confirmed(tasks: &[Task], memory: &BlobMemory) -> Result<(), String> {
    for task in tasks {
        if let Some(payload) = &task.payload
            && !memory.holds(&payload.blob)
        {
            return Err(format!(
                "task '{}' carries the module payload {} which this link never confirmed",
                task.module, payload.blob
            ));
        }
    }
    Ok(())
}

/// Every file a task stages was put on the host for this batch.
///
/// Checked against `staged`, never against the link's memory: the agent consumes a staged file,
/// so one it held for an earlier task is not there for this one.
pub(super) fn files_staged(tasks: &[Task], staged: &BTreeSet<String>) -> Result<(), String> {
    for task in tasks {
        if let Some(file) = task.files.iter().find(|f| !staged.contains(&f.blob)) {
            return Err(format!(
                "task '{}' stages the file {} under '{}', which was not put on the host for it",
                task.module, file.blob, file.arg
            ));
        }
    }
    Ok(())
}

/// What has to be true of the host before a batch goes out: it holds every payload the batch
/// names.
///
/// Called by [`run_agent_batch`] and written over [`AgentChannel`] rather than over the link, so
/// the call itself can be tested: deleting it, or asking only when *every* task carries a
/// payload instead of when *any* does, is a change that otherwise leaves a green suite and ships
/// a batch the host cannot run.
///
/// A batch that names no payload asks nothing. A batch that names one asks for it once per link,
/// and is refused here rather than by the agent, where a payload nobody sent reads as the
/// module's own failure.
async fn blob_preflight<C: AgentChannel>(
    link: &mut C,
    host: &str,
    tasks: &[Task],
    blob: Option<&Union>,
    files: &[(String, crate::action_plugins::FileBlob)],
    logs: &mut Vec<String>,
) -> Result<(), String> {
    if let Some(union) = blob
        && tasks.iter().any(|task| task.payload.is_some())
    {
        ensure_blob(link, host, &union.hash, &union.zip_b64, logs).await?;
    }
    // The payloads before the files: a batch refused for its payload leaves nothing on the host,
    // and a staged file can hold a rendered secret.
    payloads_confirmed(tasks, link.memory())?;
    let staged = stage_files(link, host, files, logs).await?;
    files_staged(tasks, &staged)
}

/// What a batch reports when the payload it needs could not be put on the host.
///
/// Every task of it fails, carrying the storing error, and the batch ends as a **failure** rather
/// than as a transport error. The difference is the whole point: a transport error makes the
/// driver call the host unreachable and leave the run with no task line at all, which says
/// something false about a host whose native tasks would run perfectly well, and steps over the
/// `rescue:` and the `ignore_errors` an author wrote for exactly this.
fn blob_failure(
    err: String,
    tasks: usize,
) -> (Vec<Option<TaskResult>>, Result<BatchOutcome, String>) {
    (
        vec![Some(TaskResult::failed_with(err)); tasks],
        Ok(BatchOutcome::Failed { at: 0 }),
    )
}

/// Sends one batch to the agent and collects what comes back, by position in `tasks`.
///
/// Anything the agent asked to show on the way lands in `logs`, already prefixed with the host,
/// for the caller to put on the coordinator's queue under this batch's `no_log`.
#[expect(
    clippy::too_many_arguments,
    reason = "the batch, the link it goes over and the blob it needs there first"
)]
pub(super) async fn run_agent_batch<C: AgentChannel>(
    link: &mut C,
    host: &str,
    id: u64,
    tasks: Vec<Task>,
    blob: Option<&Union>,
    files: &[(String, crate::action_plugins::FileBlob)],
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    logs: &mut Vec<String>,
) -> (Vec<Option<TaskResult>>, Result<BatchOutcome, String>) {
    let started = std::time::Instant::now();
    let placed = blob_preflight(link, host, &tasks, blob, files, logs).await;
    if let Some(ledger) = link.timings() {
        ledger.blob_micros += crate::profile::micros(started);
    }
    if let Err(err) = placed {
        return blob_failure(err, tasks.len());
    }
    send_batch(link, host, id, tasks, stop, stop_broken, logs).await
}

/// [`run_agent_batch`] from the point where what the batch needs is on the host.
///
/// A batch with no task in it is not sent: every step of it may have failed on its interpreter
/// already, and an agent answers an empty batch `Completed` with nothing else.
async fn send_batch<C: AgentChannel>(
    link: &mut C,
    host: &str,
    id: u64,
    mut tasks: Vec<Task>,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    logs: &mut Vec<String>,
) -> (Vec<Option<TaskResult>>, Result<BatchOutcome, String>) {
    // Asked for only when a task wants a terminal: working the line out can cost an `ssh -G`.
    let wants = |t: &Task| t.args.get(TTY_KEY) == Some(&Value::Bool(true));
    let close = if tasks.iter().any(wants) {
        link.tty_close().await
    } else {
        None
    };
    give_terminals(&mut tasks, close);
    let mut received: Vec<Option<TaskResult>> = vec![None; tasks.len()];
    if tasks.is_empty() {
        return (received, Ok(BatchOutcome::Completed));
    }
    let started = std::time::Instant::now();
    let modules: Vec<String> = tasks.iter().map(|t| t.module.clone()).collect();
    // The agent's own time, taken off the round trip so what is left is the wire's.
    let mut agent: u64 = 0;
    if let Err(err) = link.ask(&ToAgent::RunBatch { id, tasks }).await {
        return (received, Err(format!("sending batch: {err}")));
    }
    let ended = loop {
        let msg = tokio::select! {
            msg = link.answer() => msg,
            res = stop.changed(), if !*stop_broken => {
                if res.is_err() {
                    *stop_broken = true;
                    continue;
                }
                link.stop_batch(id, CANCEL_GRACE).await;
                break Ok(BatchOutcome::Cancelled { at: 0 });
            }
        };
        match msg {
            // A frame of another batch is a late answer to one this link gave up on, and
            // belongs to nothing awaited here.
            Ok(Some(FromAgent::TaskResult { batch, .. } | FromAgent::BatchDone { batch, .. }))
                if batch != id => {}
            Ok(Some(FromAgent::TaskResult {
                index,
                mut result,
                ran,
                ..
            })) => {
                if let Some(ledger) = link.timings() {
                    // The host's own figure, so added without trusting it to fit.
                    agent = agent.saturating_add(ran.as_ref().map_or(0, |r| r.micros));
                    let module = modules.get(index).cloned().unwrap_or_default();
                    ledger.ran.push((index, module, ran));
                }
                if let Some(slot) = received.get_mut(index) {
                    drop_internal_keys(&mut result);
                    add_output_lines(&mut result);
                    summarise_exception(&mut result);
                    *slot = Some(result);
                }
            }
            // Only `stop_batch` above asks for a cancel, and it consumes the answer. One that
            // arrives here was never asked for: an agent whose stdin reader died reads its own
            // end of input as a cancel.
            Ok(Some(FromAgent::BatchDone {
                outcome: BatchOutcome::Cancelled { .. },
                ..
            })) => break Err("the agent cancelled a batch nobody cancelled".to_string()),
            Ok(Some(FromAgent::BatchDone { outcome, .. })) => break Ok(outcome),
            // Collected rather than printed: a line the agent asked to show belongs on the
            // coordinator's queue with everything else this batch shows, which is the only
            // place the `no_log` policy can see it. `FromAgent::Log` carries no task index, so
            // the caller decides for the whole batch.
            Ok(Some(FromAgent::Log { message, .. })) => logs.push(format!("[{host}] {message}")),
            Ok(Some(
                FromAgent::Ready { .. }
                | FromAgent::BlobState { .. }
                | FromAgent::PutFileDone { .. },
            )) => {}
            Ok(None) => {
                break Err(format!(
                    "agent stopped before the batch finished{}",
                    link.stderr_note()
                ));
            }
            Err(err) => break Err(format!("reading from the agent: {err}")),
        }
    };
    if let Some(ledger) = link.timings() {
        ledger.wire_micros += crate::profile::micros(started).saturating_sub(agent);
    }
    (received, ended)
}

/// The argument that asks the agent to run a command on a terminal. Internal: the controller sets
/// it, a playbook cannot, and the agent never reports it.
const TTY_KEY: &str = "_volant_tty";

/// Settles which tasks of a batch run on a terminal. A task marks itself with `true` when the
/// reference would run it through `ssh -tt`; the marker becomes the line that ends its stderr when
/// this link gives commands a terminal, and is dropped on any other link. Any other value is not
/// the controller's, and is dropped.
fn give_terminals(tasks: &mut [Task], close: Option<String>) {
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
/// [`run_agent_batch`], plugin sub-tasks included.
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

fn add_output_lines(result: &mut TaskResult) {
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
fn summarise_exception(result: &mut TaskResult) {
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
const CAUSE_KEY: &str = "_volant_cause";

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

pub(super) fn classify(result: &TaskResult, ignore_errors: bool, rescuable: bool) -> Outcome {
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

/// The connection for the next batch, under `key`'s target user and over `key`'s transport: the
/// one kept from an earlier play if its agent still answers, a fresh one otherwise. A kept
/// connection gets exactly one liveness check per play, and one more each time a task of this
/// run rebooted its host ([`Reboots`]) - the driver that rebooted it dropped only its own links,
/// and another host delegating to it would otherwise reuse one the reboot killed. A failed check
/// gets exactly one reconnection; a failed reconnection is the host's `UNREACHABLE`.
///
/// The transport comes in on the key rather than being resolved here, so the connection a batch
/// opens and the identity it is filed under can never be built from two different views of the
/// host's variables.
pub(super) async fn reuse_or_connect<'a>(
    links: &'a mut HashMap<LinkKey, AgentLink>,
    checked: &mut HashMap<LinkKey, u64>,
    key: &LinkKey,
    escalation: Option<&Escalation>,
    agents: &AgentSource,
    options: &RunOptions,
) -> Result<&'a mut AgentLink, ConnectError> {
    // Why the kept connection was not reused, kept only in case the reconnection below fails
    // too: on its own it is not a failure (a single reconnection is the designed recovery), but
    // discarding it silently would leave a reconnect failure reporting only its own cause.
    let reboots = options.reboots.of(&key.host);
    let (stale, retired) = check_kept(
        links,
        checked,
        key,
        reboots,
        options.defaults.connect_timeout,
    )
    .await;
    // Closed off-task, as `Relinker::relink` closes them.
    for old in retired {
        tokio::spawn(old.shutdown());
    }
    if !links.contains_key(key) {
        let link = match connect(&key.transport, agents, &options.defaults, escalation).await {
            Ok(link) => link,
            Err(ConnectError::Unreachable(msg)) => {
                return Err(ConnectError::Unreachable(match stale {
                    Some(reason) => format!(
                        "the kept connection failed ({reason}), and reconnecting failed too: {msg}"
                    ),
                    None => msg,
                }));
            }
            // A refused escalation is the host's answer about this user, not a connection that
            // failed, so it travels on untouched: adding the stale note would turn a failed
            // task into something that reads like an unreachable host.
            Err(err) => return Err(err),
        };
        links.insert(key.clone(), link);
    }
    Ok(links.get_mut(key).expect("connected just above"))
}

/// The hosts a step's registered variable and facts are written for: every live host of the
/// batch when one of them runs the step for all of them, and the step's own host otherwise.
///
/// Measured on ansible-core 2.19.12: a `run_once` task's `register` and its `set_fact` are both
/// readable on every host of the batch, not only on the one that ran it.
/// Writes a task's `register` name for every host the task writes for.
///
/// **The one place a task's result becomes a variable**, and it writes untrusted whatever the
/// module was: native, controller-side, or a Python module whose payload the agent ran. A
/// result is a managed host's own words, so a template inside one is text and never a template
/// again - the rule the trust model rests on since the unconditional "a `set_fact` is data"
/// rule was dropped, which leaves nothing under a site that writes a result the other way.
///
/// A task with no `register` writes nothing, which is why the check lives here rather than at
/// each caller: a caller that forgets it writes nothing instead of writing under an empty name.
///
/// `warnings` are the ones [`take_warnings`] took off the results. Measured on ansible-core
/// 2.19.12 with a module calling `module.warn`: the registered result keeps them in `warnings`,
/// and a loop's keeps every item's there, in item order, with none left on the items
/// (`TaskExecutor._run_loop` gathers them, `_execute` puts back what its warning context
/// captured). `pause` warns through `display.warning` instead, and registers no such key.
pub(super) fn record_registered(
    vars: &mut VarStore,
    task: &PlayTask,
    targets: &[String],
    results: &[(Option<Value>, TaskResult)],
    warnings: &[String],
) {
    let Some(reg) = &task.register else {
        return;
    };
    let mut value = registered_value(task, results);
    // The reference registers a module's result without its `invocation` (`_process_pending_results`
    // deletes the top-level key, and only that one: a loop's items keep theirs), so a playbook
    // reading `r.invocation` fails there. `-vvv` still shows it, from the result itself.
    if let Value::Object(map) = &mut value {
        map.remove("invocation");
        if !warnings.is_empty() && short_name(&task.module) != "pause" {
            map.insert("warnings".into(), json!(warnings));
        }
    }
    for target in targets {
        vars.set_untrusted_fact(target, reg, value.clone());
    }
}

/// Writes the `ansible_facts` of a batch's results for every host the batch writes for.
///
/// **The one place a result's facts become variables**, beside [`record_registered`] and for the
/// same reason: a second road in is a road that writes them the trusted way. It is asked of every
/// remote result and not of `setup` alone, because that is where the facts are - `package_facts`,
/// `getent` and a module returning `ansible_facts` of its own all answer in the same field.
///
/// Not asked of a controller-side result. `set_fact` and `include_vars` write their own facts as
/// they run, and they alone know which of them a host wrote and which the playbook did: rewriting
/// them here would give an author's `set_fact` a managed host's trust and lose the one distinction
/// `run_local` exists to keep.
///
/// Returns the restricted names [`VarStore::gather_facts`] took out and has not warned about
/// yet in this run, for the caller to warn about.
///
/// A task that failed writes nothing, as in the reference, which records facts only for a task
/// that did not fail, `ignore_errors` or not. `results` have been through `failed_when`, and a
/// loop is judged whole: one failed item fails the task.
pub(super) fn record_facts(
    vars: &mut VarStore,
    targets: &[String],
    results: &[(Option<Value>, TaskResult)],
) -> Vec<String> {
    let mut removed = Vec::new();
    if results.iter().any(|(_, result)| result.failed()) {
        return removed;
    }
    for (_, result) in results {
        let Some(facts) = result.0.get("ansible_facts").and_then(Value::as_object) else {
            continue;
        };
        for target in targets {
            removed.extend(vars.gather_facts(target, facts));
        }
    }
    removed
}

/// The key `set_stats` answers under: not a host setting, so not read through `host_setting`.
const STATS_KEY: &str = "ansible_stats";

/// Applies what `add_host`, `group_by` and `set_stats` answered for `host`, once the task is
/// judged: the inventory gains the host or the group, the custom stats gain the data.
///
/// Keyed on the action's name and never on the keys alone: a managed host's module result can
/// carry any key, and `add_host` among them is a restricted one that is taken out of its facts.
/// A failed task, or one of its loop items, applies nothing, as in the reference.
pub(super) fn record_inventory_results(
    vars: &mut VarStore,
    task: &PlayTask,
    host: &str,
    results: &[(Option<Value>, TaskResult)],
) {
    let kind = short_name(&task.module);
    if !["add_host", "group_by", "set_stats"].contains(&kind)
        || results.iter().any(|(_, r)| r.failed())
    {
        return;
    }
    let names = |value: Option<&Value>| -> Vec<String> {
        value
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    };
    for (_, result) in results.iter().filter(|(_, r)| !r.skipped()) {
        match kind {
            "add_host" => {
                let Some(added) = result.0.get("add_host").and_then(Value::as_object) else {
                    continue;
                };
                let Some(name) = added.get("host_name").and_then(Value::as_str) else {
                    continue;
                };
                let host_vars = added
                    .get("host_vars")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                vars.add_host(name, &names(added.get("groups")), &host_vars);
            }
            "group_by" => {
                if let Some(group) = result.0.get("add_group").and_then(Value::as_str) {
                    vars.add_to_group(host, group, &names(result.0.get("parent_groups")));
                }
            }
            _ => {
                if let Some(stats) = result.0.get(STATS_KEY) {
                    vars.record_custom_stats(host, stats);
                }
            }
        }
    }
}

/// Writes the facts a controller-side `set_fact` or `include_vars` answered, once `failed_when`
/// has judged the task. A failed task writes none, `ignore_errors` or not, and a loop is judged
/// whole, as [`record_facts`] judges a host's: measured on ansible-core 2.19.12, a `set_fact` or
/// an `include_vars` under `failed_when: true` and `ignore_errors: true` leaves its names
/// undefined at the next task, and so does a `set_fact` loop whose last item failed.
///
/// Only the `set_fact` values whose own render read a managed host are data. Measured on
/// ansible-core 2.19.12: a fact the playbook wrote can still name the variable a `debug: var:`
/// shows, so writing every fact as data would fail a play with no host value anywhere in it.
pub(super) fn record_local_facts(
    vars: &mut VarStore,
    task: &PlayTask,
    items: &[Item],
    hosts: &[String],
    results: &[(Option<Value>, TaskResult)],
    templar: &Templar,
) {
    let set_fact = match short_name(&task.module) {
        "set_fact" => true,
        "include_vars" => false,
        _ => return,
    };
    if results.iter().any(|(_, r)| r.failed()) {
        return;
    }
    let mut written = BTreeSet::new();
    for (item, (_, result)) in items.iter().zip(results) {
        let Some(facts) = result.0.get("ansible_facts").and_then(Value::as_object) else {
            continue;
        };
        let data = set_fact_data(task, item, templar);
        let cacheable = set_fact
            && item.args.get("cacheable").is_some_and(|v| match v {
                Value::Bool(on) => *on,
                Value::String(text) => crate::yaml::bool_from_str(text.trim()) == Some(true),
                _ => false,
            });
        for (key, value) in facts {
            for host in hosts {
                if cacheable {
                    vars.cache_fact(host, key, value);
                    written.insert(host.clone());
                }
                if set_fact && data.contains(key) {
                    vars.set_untrusted_fact(host, key, value.clone());
                } else {
                    vars.set_fact(host, key, value.clone());
                }
            }
        }
    }
    // One file write per host for the whole task, however many names it set.
    for host in written {
        vars.write_cache(&host);
    }
}

pub(super) fn fact_targets(task: &PlayTask, host: &str, live: &[String]) -> Vec<String> {
    if task.runs_once() && !live.is_empty() {
        live.to_vec()
    } else {
        vec![host.to_string()]
    }
}

/// What checking a kept link needs of it, so the check can be driven by a fake in tests.
pub(super) trait KeptLink {
    async fn handshake(&mut self) -> anyhow::Result<()>;
    async fn shutdown(self);
}

impl KeptLink for AgentLink {
    async fn handshake(&mut self) -> anyhow::Result<()> {
        AgentLink::handshake(self).await
    }

    async fn shutdown(self) {
        AgentLink::shutdown(self).await;
    }
}

/// Takes every link kept for `host` out of `links`, and forgets that they were checked, for the
/// caller to close.
fn retire_host_links<L>(
    links: &mut HashMap<LinkKey, L>,
    checked: &mut HashMap<LinkKey, u64>,
    host: &str,
) -> Vec<L> {
    let keys: Vec<LinkKey> = links.keys().filter(|k| k.host == host).cloned().collect();
    keys.iter()
        .filter_map(|k| {
            checked.remove(k);
            links.remove(k)
        })
        .collect()
}

/// Checks the link kept for `key` if the host rebooted since it was last proved alive (or it
/// never was). A link that answers stays in `links`. One that fails, at once or by timing out,
/// is closed, and then the host's shared connection is stopped and every other link kept for the
/// host is retired, as `Relinker::relink` does.
///
/// Both halves matter. The host's plain and escalated links ride one master, and each is
/// checked only when it is next used, so the link that was idle through a reboot may still point
/// at a master long gone while the socket holds a newer one on a connection the host dropped
/// without a word; failing at once on the old one says nothing about the new one, so the socket
/// is stopped on every failure. And a sibling left in `links` would be checked in its turn, fail,
/// and stop the master this reconnection is about to open; retired, it reconnects through that
/// master instead, unchecked.
///
/// Returns why the kept link was not reused, if it failed, and the retired links.
async fn check_kept<L: KeptLink>(
    links: &mut HashMap<LinkKey, L>,
    checked: &mut HashMap<LinkKey, u64>,
    key: &LinkKey,
    reboots: u64,
    timeout: Duration,
) -> (Option<String>, Vec<L>) {
    if checked.insert(key.clone(), reboots) == Some(reboots) {
        return (None, Vec::new());
    }
    let Some(mut link) = links.remove(key) else {
        // No kept link, so nothing to check: this key connects through the socket as it is.
        // That is safe only if the socket's master was proven alive since the host's last
        // reboot, by a sibling on the same transport kept and checked (or connected) at this
        // count; a host this run never rebooted keeps whatever master it has. Otherwise the
        // master may ride a connection the reboot dropped, and it is stopped first, with the
        // host's other links retired so none of them later stops the master this key opens.
        let proven = links
            .keys()
            .any(|k| k.transport == key.transport && checked.get(k) == Some(&reboots));
        if reboots > 0 && key.transport.is_shared() && !proven {
            let retired = retire_host_links(links, checked, &key.host);
            key.transport.stop_shared().await;
            return (None, retired);
        }
        return (None, Vec::new());
    };
    let stale = match tokio::time::timeout(timeout, link.handshake()).await {
        Ok(Ok(())) => {
            links.insert(key.clone(), link);
            return (None, Vec::new());
        }
        Ok(Err(err)) => format!("{err:#}"),
        Err(_) => "no answer from the kept connection".to_string(),
    };
    link.shutdown().await;
    let retired = retire_host_links(links, checked, &key.host);
    key.transport.stop_shared().await;
    (Some(stale), retired)
}

/// Opens one link, escalated when `escalation` is given. Every way this can fail is a host the
/// run cannot reach and comes back as `ConnectError::Unreachable`, except a `sudo` that refused,
/// which comes back as `ConnectError::Become` because the host itself answered.
async fn connect(
    transport: &Transport,
    agents: &AgentSource,
    defaults: &ConnectionDefaults,
    escalation: Option<&Escalation>,
) -> Result<AgentLink, ConnectError> {
    let mut link = transport.connect(agents, escalation).await?;
    let timeout = defaults.connect_timeout;
    tokio::time::timeout(timeout, link.handshake())
        .await
        .map_err(|_| {
            ConnectError::Unreachable(format!(
                "no answer from the agent after {} seconds",
                timeout.as_secs()
            ))
        })?
        .map_err(|e| ConnectError::Unreachable(format!("{e:#}")))?;
    Ok(link)
}

#[cfg(test)]
pub(super) mod test_support;
#[cfg(test)]
mod tests;
