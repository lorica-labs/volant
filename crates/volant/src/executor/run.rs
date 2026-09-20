// SPDX-License-Identifier: GPL-3.0-or-later
//! Running one task and judging its result, on the controller as well as on the agent.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::watch;
use volant_protocol::modules::short_name;
use volant_protocol::{BatchOutcome, FromAgent, Task, TaskResult, ToAgent};

use crate::agent::{AgentLink, AgentSource};
use crate::compile::{Compiled, Step};
use crate::playbook::PlayTask;
use crate::stats::Outcome;
use crate::template::{Templar, TemplateError};
use crate::transport::{ConnectError, ConnectionDefaults, Escalation, Transport};
use crate::vars::{HostVars, VarStore, load_vars_file};

use super::prepare::{Item, display};
use super::{CANCEL_GRACE, LinkKey, RunOptions, as_bool_value, python_type};

/// What a `debug: var:` naming a value that came from a managed host reports. The reference's
/// own sentence, measured on ansible-core 2.19.12 against `var: "{{ r.stdout }}"` and against
/// `var: "{{ item }}"` over a registered list: both fail the task with this `msg`, the second
/// once per item.
const UNTRUSTED_VAR: &str = "Task failed: Error while resolving `var` expression: Encountered untrusted template or expression.";

/// `include_vars`, run where every other controller-side module runs.
///
/// Measured on ansible-core 2.19.12: a file that is there sets each of its keys as a fact and
/// answers `{"ansible_facts": {...}, "ansible_included_var_files": ["<abs>"], "changed": false}`,
/// a `name:` puts the whole mapping under that one key, and a file that is nowhere fails the task
/// with the paths it looked in spelled out one per line.
///
/// A deliberate divergence, recorded rather than hidden: these land as facts, which is precedence
/// 20 here against the reference's own rank 19 for `include_vars`. The two differ only for a name
/// a host variable also carries.
fn run_include_vars(
    item: &Item,
    step: &Step,
    hosts: &[String],
    store: &Mutex<VarStore>,
) -> TaskResult {
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
        Err(err) => return TaskResult::failed_with(format!("Task failed: {err:#}")),
    };
    let facts = match item.args.get("name").and_then(Value::as_str) {
        Some(under) => {
            let mut one = Map::new();
            one.insert(under.to_string(), Value::Object(loaded));
            one
        }
        None => loaded,
    };
    {
        let mut vars = store.lock().expect("vars lock");
        for (key, value) in &facts {
            for host in hosts {
                vars.set_fact(host, key, value.clone());
            }
        }
    }
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

/// `set_fact`, `debug` and `include_vars` never leave the controller.
pub(super) fn run_local(
    task: &PlayTask,
    item: &Item,
    step: &Step,
    fact_hosts: &[String],
    templar: &Templar,
    store: &Mutex<VarStore>,
    verbosity: u8,
) -> TaskResult {
    let mut r = Map::new();
    match short_name(&task.module) {
        "include_vars" => return run_include_vars(item, step, fact_hosts, store),
        "set_fact" => {
            let mut facts = Map::new();
            for (k, v) in &item.args {
                if k == "cacheable" {
                    continue;
                }
                // Only the values whose own render read a managed host are data. Measured on
                // ansible-core 2.19.12: a fact the playbook wrote can still name the variable a
                // `debug: var:` shows, so writing every fact as data would fail a play with no
                // host value anywhere in it.
                let from_host = item.args_untrusted.contains(k);
                let mut vars = store.lock().expect("vars lock");
                for host in fact_hosts {
                    if from_host {
                        vars.set_untrusted_fact(host, k, v.clone());
                    } else {
                        vars.set_fact(host, k, v.clone());
                    }
                }
                drop(vars);
                facts.insert(k.clone(), v.clone());
            }
            r.insert("ansible_facts".into(), Value::Object(facts));
            r.insert("changed".into(), json!(false));
            r.insert("failed".into(), json!(false));
        }
        "debug" => {
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
                    return TaskResult::failed_with(UNTRUSTED_VAR);
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
        }
        "validate_argument_spec" => return validate_argument_spec(item),
        other => {
            return TaskResult::failed_with(format!("{other} is not a controller-side module"));
        }
    }
    TaskResult(r)
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
    match apply_conditions(task, item, result, templar) {
        Ok(r) => r,
        Err(e) => TaskResult::failed_with(e.0),
    }
}

/// Applies `changed_when` and `failed_when` to one result, with `result` bound to it.
fn apply_conditions(
    task: &PlayTask,
    item: &Item,
    mut result: TaskResult,
    templar: &Templar,
) -> Result<TaskResult, TemplateError> {
    if task.changed_when.is_empty() && task.failed_when.is_empty() {
        return Ok(result);
    }
    let mut vars = item.vars.clone();
    if let Some(reg) = &task.register {
        vars.insert_untrusted(reg.clone(), Value::Object(result.0.clone()));
    }
    vars.insert_untrusted("result".into(), Value::Object(result.0.clone()));
    if !task.changed_when.is_empty() {
        let changed = all_hold(&task.changed_when, &vars, templar)?;
        result.0.insert("changed".into(), json!(changed));
    }
    if !task.failed_when.is_empty() {
        let failed = all_hold(&task.failed_when, &vars, templar)?;
        result.0.insert("failed_when_result".into(), json!(failed));
        if failed {
            result.0.insert("failed".into(), json!(true));
        } else {
            result.0.remove("failed");
            // A non-zero rc would still count as failed: the condition has spoken.
            if result.failed() {
                result.0.insert("failed".into(), json!(false));
            }
        }
    }
    Ok(result)
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

/// What `register` stores: the single result, or Ansible's loop aggregate.
pub(super) fn registered_value(task: &PlayTask, results: &[(Option<Value>, TaskResult)]) -> Value {
    if task.loop_items.is_none() {
        return results
            .first()
            .map_or(Value::Null, |(_, r)| Value::Object(r.0.clone()));
    }
    if results.is_empty() {
        let mut agg = empty_loop_result().0;
        agg.insert("results".into(), Value::Array(Vec::new()));
        return Value::Object(agg);
    }
    let mut list = Vec::new();
    for (element, r) in results {
        let mut m = r.0.clone();
        if let Some(el) = element {
            m.insert(task.loop_var.clone(), el.clone());
            m.insert("ansible_loop_var".into(), json!(task.loop_var));
        }
        list.push(Value::Object(m));
    }
    let mut agg = Map::new();
    agg.insert("results".into(), Value::Array(list));
    agg.insert(
        "changed".into(),
        json!(results.iter().any(|(_, r)| r.changed())),
    );
    if results.iter().any(|(_, r)| r.failed()) {
        agg.insert("failed".into(), json!(true));
    }
    agg.insert(
        "skipped".into(),
        json!(results.iter().all(|(_, r)| r.skipped())),
    );
    agg.insert("msg".into(), json!("All items completed"));
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
/// `ansible_failed_task.connection` is better told nothing than told `ssh` by an engine that
/// never looked at the keyword.
///
/// `become_method` is not one of those: this release honours it. It reads `null` here anyway
/// because its resolved value lives with the host's variables, not on `PlayTask`, and this
/// function only takes the task - not because nothing looked at the keyword.
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
            "changed_when" => strings(&task.changed_when),
            "failed_when" => strings(&task.failed_when),
            "ignore_errors" => json!(task.ignore_errors),
            "loop" => task.loop_items.clone().unwrap_or(Value::Null),
            "loop_control" => json!({"loop_var": task.loop_var, "label": task.loop_label}),
            "name" => json!(task.name),
            "register" => json!(task.register),
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

/// Files the handlers one finished task asked for, if it changed anything.
///
/// Measured on ansible-core 2.19.12 and each half worth stating: a task that came back `ok`
/// notifies nothing, a name notified twice runs its handler once, and for a loop it is the
/// **aggregate** that decides - one changed item is a changed task. Notifications live in the
/// driver and nowhere else: they are one host's business, and the flush the coordinator opens is
/// the same flush whether this host has anything to run in it or not.
pub(super) fn notify(
    compiled: &Compiled,
    task: &PlayTask,
    results: &[(Option<Value>, TaskResult)],
    notified: &mut Vec<usize>,
) {
    if task.notify.is_empty() || !results.iter().any(|(_, r)| r.changed()) {
        return;
    }
    for name in &task.notify {
        // Not deduplicated here, deliberately: what makes a handler run once is the driver
        // taking **every** copy of its index off this list when it runs it, and a guard here
        // that cannot fail because of that would read as if it were what did the work. Proved
        // by deletion - removing this guard reddened nothing, removing that one reddens the
        // order-and-count test.
        notified.extend(crate::compile::resolve_notify(compiled, name));
    }
}

/// What `until`, `retries` and `delay` ask of one task, rendered against its own variables.
///
/// Measured on ansible-core 2.19.12, and every line of it is a measurement: `until` with no
/// `retries` gives three attempts; `retries` with no `until` retries while the result is failed;
/// `retries` under 1 (0 and -1 were both measured) turns the whole machinery off, so the task
/// runs once, prints no retry line, carries no `attempts` and is not failed by a condition that
/// never held; the default `delay` is five seconds; and the sleep happens after **every** failed
/// attempt, the last one included - measured by timing, 2 attempts cost 10 seconds and 3 cost 15.
#[derive(Clone)]
pub(super) struct Retry {
    /// Attempts in all, never zero.
    pub(super) attempts: u32,
    pub(super) delay: Duration,
    /// The conditions that end the loop. Empty means "the result did not fail".
    until: Vec<String>,
}

/// `item` is the task's first one, whose variables `retries` and `delay` are rendered against.
/// It is an `Option` because nothing here promises a task has items; one that has none is
/// `Prepared::Skipped` and never reaches this.
pub(super) fn retry_plan(
    task: &PlayTask,
    item: Option<&Item>,
    templar: &Templar,
) -> Result<Option<Retry>, TemplateError> {
    let empty = HostVars::default();
    let vars = item.map_or(&empty, |i| &i.vars);
    let number = |raw: &Value, keyword: &str| -> Result<f64, TemplateError> {
        let rendered = templar.render_value(raw, vars)?;
        match &rendered {
            Value::Number(n) => n.as_f64().ok_or_else(|| TemplateError(String::new())),
            Value::String(s) => s.trim().parse::<f64>().map_err(|_| TemplateError(String::new())),
            _ => Err(TemplateError(String::new())),
        }
        .map_err(|_| {
            TemplateError(format!(
                "Error processing keyword '{keyword}': The value {rendered} could not be converted to 'int'."
            ))
        })
    };
    let attempts = match &task.retries {
        Some(raw) => {
            let n = number(raw, "retries")?;
            if n < 1.0 {
                return Ok(None);
            }
            n as u32
        }
        None if task.until.is_empty() => return Ok(None),
        None => 3,
    };
    let delay = match &task.delay {
        Some(raw) => Duration::from_secs_f64(number(raw, "delay")?.max(0.0)),
        None => Duration::from_secs(5),
    };
    Ok(Some(Retry {
        attempts,
        delay,
        until: task.until.clone(),
    }))
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
    let mut vars = item.vars.clone();
    if let Some(reg) = &task.register {
        vars.insert_untrusted(reg.clone(), Value::Object(result.0.clone()));
    }
    vars.insert_untrusted("result".into(), Value::Object(result.0.clone()));
    all_hold(&retry.until, &vars, templar)
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

/// One item of one task, as the agent is asked to run it.
pub(super) fn protocol_task(task: &PlayTask, item: &Item) -> Task {
    Task {
        module: task.module.clone(),
        args: item.args.clone(),
        // An item is not the task: a failing item never stops the ones behind it, exactly as in
        // Ansible. The task's own failure is decided later, by `report_task`, from every item's
        // result.
        ignore_errors: task.ignores_errors() || task.loop_items.is_some(),
        timeout: task.timeout,
        environment: item.environment.clone(),
        payload: None,
    }
}

/// Sends one batch to the agent and collects what comes back, by position in `tasks`.
///
/// Anything the agent asked to show on the way lands in `logs`, already prefixed with the host,
/// for the caller to put on the coordinator's queue under this batch's `no_log`.
pub(super) async fn run_agent_batch(
    link: &mut AgentLink,
    host: &str,
    id: u64,
    tasks: Vec<Task>,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    logs: &mut Vec<String>,
) -> (Vec<Option<TaskResult>>, Result<BatchOutcome, String>) {
    let mut received: Vec<Option<TaskResult>> = vec![None; tasks.len()];
    if let Err(err) = link.send(&ToAgent::RunBatch { id, tasks }).await {
        return (received, Err(format!("sending batch: {err}")));
    }
    let ended = loop {
        let msg = tokio::select! {
            msg = link.recv() => msg,
            res = stop.changed(), if !*stop_broken => {
                if res.is_err() {
                    *stop_broken = true;
                    continue;
                }
                link.cancel(id, CANCEL_GRACE).await;
                break Ok(BatchOutcome::Cancelled { at: 0 });
            }
        };
        match msg {
            Ok(Some(FromAgent::TaskResult { index, result, .. })) => {
                if let Some(slot) = received.get_mut(index) {
                    *slot = Some(result);
                }
            }
            Ok(Some(FromAgent::BatchDone { outcome, .. })) => break Ok(outcome),
            // Collected rather than printed: a line the agent asked to show belongs on the
            // coordinator's queue with everything else this batch shows, which is the only
            // place the `no_log` policy can see it. `FromAgent::Log` carries no task index, so
            // the caller decides for the whole batch.
            Ok(Some(FromAgent::Log { message, .. })) => logs.push(format!("[{host}] {message}")),
            Ok(Some(FromAgent::Ready { .. } | FromAgent::BlobState { .. })) => {}
            Ok(None) => break Err("agent stopped before the batch finished".to_string()),
            Err(err) => break Err(format!("reading from the agent: {err}")),
        }
    };
    (received, ended)
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
/// connection gets exactly one liveness check per play, and a failed check exactly one
/// reconnection; a failed reconnection is the host's `UNREACHABLE`.
///
/// The transport comes in on the key rather than being resolved here, so the connection a batch
/// opens and the identity it is filed under can never be built from two different views of the
/// host's variables.
pub(super) async fn reuse_or_connect<'a>(
    links: &'a mut HashMap<LinkKey, AgentLink>,
    checked: &mut HashSet<LinkKey>,
    key: &LinkKey,
    escalation: Option<&Escalation>,
    agents: &AgentSource,
    options: &RunOptions,
) -> Result<&'a mut AgentLink, ConnectError> {
    // Why the kept connection was not reused, kept only in case the reconnection below fails
    // too: on its own it is not a failure (a single reconnection is the designed recovery), but
    // discarding it silently would leave a reconnect failure reporting only its own cause.
    let mut stale: Option<String> = None;
    if checked.insert(key.clone())
        && let Some(mut link) = links.remove(key)
    {
        let alive = tokio::time::timeout(options.defaults.connect_timeout, link.handshake()).await;
        match alive {
            Ok(Ok(())) => {
                links.insert(key.clone(), link);
            }
            Ok(Err(err)) => {
                stale = Some(format!("{err:#}"));
                link.shutdown().await;
            }
            Err(_) => {
                stale = Some("no answer from the kept connection".to_string());
                link.shutdown().await;
            }
        }
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
pub(super) fn fact_targets(task: &PlayTask, host: &str, live: &[String]) -> Vec<String> {
    if task.runs_once() && !live.is_empty() {
        live.to_vec()
    } else {
        vec![host.to_string()]
    }
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
mod tests {
    use super::super::testing::{hvars, task, vars};
    use super::*;
    use crate::compile::StepKind;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    /// Every row of `LOCAL_MODULES` has an arm in `run_local` behind it.
    ///
    /// Nothing but a name joins the two. The table is what the pre-flight consults, what the
    /// documentation page is generated from and what decides a task runs on the controller
    /// rather than on the host; the `match` in `run_local` is what then runs. A row added to the
    /// table without an arm passes the loader, passes the pre-flight, shows its banner, and only
    /// then fails on the host that reached it, telling the operator that a module the engine
    /// advertises is "not a controller-side module". That is this project's most expensive bug
    /// family - accepted, then silently not done - and it is the same totality property the
    /// keyword tables carry, one level over.
    ///
    /// What would make this red: a name in `LOCAL_MODULES` that falls through to the catch-all
    /// arm.
    #[test]
    fn every_local_module_has_an_arm_in_run_local() {
        let inventory = crate::inventory::Inventory::parse_ini("h1\n").expect("an inventory");
        let store = Mutex::new(
            VarStore::new(&inventory, None, Path::new("."), Map::new()).expect("a var store"),
        );
        let templar = Templar::new(PathBuf::from("."));
        let step = Step {
            kind: StepKind::Task,
            task: PlayTask::empty(),
            block: None,
            section: crate::compile::Section::Body,
            role: None,
            origin: Arc::new(crate::compile::Origin::default()),
            include_params: None,
            hosts: None,
        };
        for spec in volant_protocol::modules::LOCAL_MODULES {
            let item = Item {
                element: None,
                label: None,
                args: Map::new(),
                args_untrusted: std::collections::BTreeSet::new(),
                vars: HostVars::default(),
                environment: BTreeMap::new(),
                skipped: None,
            };
            // Arguments are deliberately empty: what is asserted is that the name is known, not
            // that the module does its work. A module that needs an argument says so in its own
            // words, which is not the catch-all's words.
            let result = run_local(
                &task(spec.name),
                &item,
                &step,
                std::slice::from_ref(&"h1".to_string()),
                &templar,
                &store,
                0,
            );
            assert_ne!(
                result.0.get("msg").and_then(Value::as_str),
                Some(format!("{} is not a controller-side module", spec.name).as_str()),
                "{} is in LOCAL_MODULES with no arm in run_local",
                spec.name
            );
        }
    }

    /// Each complaint the argument check can make, in the reference's own words and in the order
    /// it makes them.
    ///
    /// Measured on ansible-core 2.19.12 by asking for all four at once: missing required
    /// arguments (sorted by name, whatever order the spec lists them in), then a type that
    /// cannot be converted, then a value outside `choices`, then an argument the spec does not
    /// have. A value the entry never named is still found when the host has a variable of that
    /// name, which is what lets `-e needed=given` pass a check the role entry said nothing
    /// about; a name the spec does not have is only refused when the entry wrote it, since a
    /// host carries hundreds of variables the spec has never heard of.
    ///
    /// What would make this red: a check that passes a permissive conversion the reference
    /// refuses, or refuses one it accepts - either way a role runs, or fails to run, on
    /// arguments the reference judges differently.
    #[test]
    fn the_argument_check_speaks_the_reference_s_sentences() {
        let check = |provided: Value, host: Value| {
            let item = Item {
                element: None,
                label: None,
                args: vars(json!({
                    "argument_spec": {
                        "zeta": {"type": "str", "required": true},
                        "alpha_req": {"type": "str", "required": true},
                        "count": {"type": "int"},
                        "flag": {"type": "bool"},
                        "pick": {"type": "str", "choices": ["alpha", "beta"]},
                    },
                    "provided_arguments": provided,
                    "validate_args_context": {"argument_spec_name": "main", "name": "types", "type": "role"},
                })),
                args_untrusted: std::collections::BTreeSet::new(),
                vars: hvars(host),
                environment: BTreeMap::new(),
                skipped: None,
            };
            validate_argument_spec(&item)
        };

        let all_wrong = check(
            json!({"count": "notanint", "pick": "gamma", "unknown_arg": "whatever"}),
            json!({}),
        );
        assert!(all_wrong.failed());
        assert_eq!(
            all_wrong.0["argument_errors"],
            json!([
                "missing required arguments: alpha_req, zeta",
                "argument 'count' is of type str and we were unable to convert to int: \"'notanint'\" cannot be converted to an int",
                "value of pick must be one of: alpha, beta, got: gamma",
                "unknown_arg. Supported parameters include: alpha_req, count, flag, pick, zeta.",
            ])
        );
        assert_eq!(
            all_wrong.0["msg"],
            json!(
                "Validation of arguments failed:\nmissing required arguments: alpha_req, zeta\nargument 'count' is of type str and we were unable to convert to int: \"'notanint'\" cannot be converted to an int\nvalue of pick must be one of: alpha, beta, got: gamma\nunknown_arg. Supported parameters include: alpha_req, count, flag, pick, zeta."
            )
        );
        assert_eq!(
            all_wrong.0["argument_spec_data"]["zeta"]["required"],
            json!(true)
        );

        // Measured: `"3"` is an `int` and `"yes"` is a `bool`, and nothing is written back - the
        // role's own tasks read `count=3 flag=yes`, the strings they were handed.
        let permissive = check(
            json!({"count": "3", "flag": "yes", "pick": "alpha", "zeta": "z", "alpha_req": "a"}),
            json!({}),
        );
        assert!(!permissive.failed(), "{:?}", permissive.0);
        assert_eq!(permissive.0["msg"], json!("The arg spec validation passed"));
        assert_eq!(
            permissive.0["validate_args_context"]["name"],
            json!("types")
        );

        // Nothing on the entry, everything on the host: the shape `-e zeta=z ...` makes.
        let from_host = check(
            json!({}),
            json!({"zeta": "z", "alpha_req": "a", "count": 2, "flag": false, "pick": "beta"}),
        );
        assert!(!from_host.failed(), "{:?}", from_host.0);
        // A host variable the spec never heard of is not an unknown argument.
        let noise = check(
            json!({}),
            json!({"zeta": "z", "alpha_req": "a", "ansible_forks": 5}),
        );
        assert!(!noise.failed(), "{:?}", noise.0);
    }

    fn result(v: Value) -> TaskResult {
        TaskResult(v.as_object().unwrap().clone())
    }

    /// `ansible_failed_task` carries the keys the reference carries, and the ones this engine
    /// models carry the task's own values.
    ///
    /// Measured on ansible-core 2.19.12: the dictionary has 46 keys, six of them the engine's
    /// own bookkeeping (`uuid`, `finalized`, `squashed`, `_resolved_action`, `async_val`,
    /// `loop_with`); of the forty left, `local_action` and the `with_*` lookups are absent
    /// because the reference has already folded them into `action` and `loop`.
    ///
    /// What would make this red: a key dropped, which turns a rescue reading it into a failure
    /// of its own; or `action` carrying the fully qualified module name, which the reference
    /// keeps in a private key and which every playbook comparing `.action` to a short name
    /// would then miss.
    #[test]
    fn the_failed_task_carries_the_reference_s_own_keys() {
        let mut task = task("ansible.builtin.command");
        task.name = "boom".to_string();
        task.register = Some("b".to_string());
        task.when = vec!["true".to_string()];
        task.tags = vec!["t1".to_string()];
        task.vars = vars(json!({"v": 1}));
        let value = failed_task_value(&task);
        let object = value.as_object().expect("an object");
        assert_eq!(object.len(), 40, "{value}");
        for key in [
            "action",
            "args",
            "become",
            "connection",
            "loop",
            "loop_control",
            "name",
            "register",
            "tags",
            "vars",
            "when",
        ] {
            assert!(object.contains_key(key), "{key} is missing from {value}");
        }
        assert!(!object.contains_key("local_action"), "{value}");
        assert!(!object.contains_key("with_items"), "{value}");
        assert_eq!(object["action"], json!("command"));
        assert_eq!(object["name"], json!("boom"));
        assert_eq!(object["register"], json!("b"));
        assert_eq!(object["when"], json!(["true"]));
        assert_eq!(object["tags"], json!(["t1"]));
        assert_eq!(object["vars"], json!({"v": 1}));
        assert_eq!(object["loop"], Value::Null);
        assert_eq!(
            object["loop_control"],
            json!({"loop_var": "item", "label": null})
        );
        assert_eq!(
            object["connection"],
            Value::Null,
            "a keyword nothing here honours says nothing rather than the reference's default"
        );
    }

    #[test]
    fn a_loop_aggregate_follows_ansible_shape() {
        let mut t = task("command");
        t.loop_items = Some(json!([1, 2]));
        let results = vec![
            (Some(json!(1)), result(json!({"changed": true, "rc": 0}))),
            (
                Some(json!(2)),
                result(json!({"changed": false, "rc": 1, "failed": true})),
            ),
        ];
        let agg = registered_value(&t, &results);
        assert_eq!(agg["changed"], json!(true));
        assert_eq!(agg["failed"], json!(true));
        assert_eq!(agg["skipped"], json!(false));
        assert_eq!(agg["msg"], json!("All items completed"));
        assert_eq!(agg["results"][1]["item"], json!(2));
        assert_eq!(agg["results"][1]["ansible_loop_var"], json!("item"));
    }

    #[test]
    fn failed_when_false_rescues_a_non_zero_rc() {
        let templar = Templar::new(std::env::temp_dir());
        let mut t = task("command");
        t.failed_when = vec!["result.rc == 99".into()];
        let item = Item {
            element: None,
            label: None,
            args: Map::new(),
            args_untrusted: std::collections::BTreeSet::new(),
            vars: HostVars::default(),
            environment: BTreeMap::new(),
            skipped: None,
        };
        let r = apply_conditions(
            &t,
            &item,
            result(json!({"rc": 1, "changed": true})),
            &templar,
        )
        .unwrap();
        assert!(!r.failed(), "{:?}", r.0);
        assert_eq!(r.0["failed_when_result"], json!(false));
        let r = apply_conditions(
            &t,
            &item,
            result(json!({"rc": 99, "changed": true})),
            &templar,
        )
        .unwrap();
        assert!(r.failed());
    }

    /// A `run_once` step's registered variable and facts are written for every live host of the
    /// batch; every other step writes for its own host alone.
    ///
    /// What would make this red: the broadcast dropped, which leaves the hosts that did not run
    /// the task with an undefined variable behind it.
    #[test]
    fn a_run_once_step_writes_its_result_for_the_whole_batch() {
        let live = ["h1".to_string(), "h2".to_string()];
        let mut t = task("command");
        assert_eq!(fact_targets(&t, "h1", &live), ["h1"]);
        t.run_once = Some(true);
        assert_eq!(fact_targets(&t, "h1", &live), live);
    }
}
