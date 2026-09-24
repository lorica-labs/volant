// SPDX-License-Identifier: GPL-3.0-or-later
//! Running one task and judging its result, on the controller as well as on the agent.

use std::collections::{BTreeSet, HashMap};
use std::io::IsTerminal as _;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::watch;
use volant_protocol::modules::{ASSERT, FAIL, ModuleSpec, PAUSE, short_name};
use volant_protocol::{BatchOutcome, FromAgent, Task, TaskResult, ToAgent};

use crate::agent::{AgentLink, AgentSource, BlobMemory};
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

/// The modules in `LOCAL_MODULES` never leave the controller. `stop` is the run's interruption,
/// which a `pause` waits on beside its timer: `None` is a pause the run's stop ended, which the
/// driver answers the way it answers every other wait the stop ends, with no task line and no
/// count.
#[expect(
    clippy::too_many_arguments,
    reason = "the task, the host's view of it, and the run's interruption for a pause"
)]
pub(super) async fn run_local(
    task: &PlayTask,
    item: &Item,
    step: &Step,
    fact_hosts: &[String],
    templar: &Templar,
    store: &Mutex<VarStore>,
    verbosity: u8,
    stop: &mut watch::Receiver<bool>,
) -> Option<TaskResult> {
    let mut result = if short_name(&task.module) == "pause" {
        pause(item, std::io::stdin().is_terminal(), stop).await?
    } else {
        local_result(task, item, step, fact_hosts, templar, store, verbosity)
    };
    // The reference's `maybe_raise_on_result` normalises every task's final result, wherever it
    // ran, the way `run_agent_batch` normalises an agent's.
    summarise_exception(&mut result);
    Some(result)
}

/// Every controller-side module but `pause`, none of which waits.
fn local_result(
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
        "assert" => return assert_module(task, item, templar),
        "fail" => {
            let unknown = unknown_args(&FAIL, &item.args);
            if !unknown.is_empty() {
                return TaskResult::failed_with(format!(
                    "Invalid options for ansible.builtin.fail: {}",
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
fn unsupported(spec: &ModuleSpec, aliases: &[&str], args: &Map<String, Value>) -> Option<String> {
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
        "Unsupported parameters for (ansible_collections.ansible.builtin.plugins.action.{}) module: {}. Supported parameters include: {}{aliases}.",
        spec.name,
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
    if let Some(refusal) = unsupported(&ASSERT, &["msg"], &item.args) {
        return TaskResult::failed_with(refusal);
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

/// `pause`, measured on ansible-core 2.19.12: a duration under one second waits one, `delta` is
/// the whole seconds waited, and `stdout` gives the time waited to two decimals, in minutes
/// unless `seconds` was the argument. A pause that asks for an answer, through `prompt` or by
/// having no duration at all, warns and goes on at once when standard input is not a terminal.
///
/// On a terminal it is refused: nothing here reads the answer, and showing the prompt and going
/// on would accept the task and not do it. `interactive` is passed in so that both sides of that
/// can be tested. `None` is the run's interruption, which ends the wait early.
async fn pause(
    item: &Item,
    interactive: bool,
    stop: &mut watch::Receiver<bool>,
) -> Option<TaskResult> {
    if let Some(refusal) = unsupported(&PAUSE, &[], &item.args) {
        return Some(TaskResult::failed_with(refusal));
    }
    let whole = |v: &Value| match v {
        Value::Number(n) => n.as_f64().map(|f| f.trunc() as i64),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    };
    let asked = match (item.args.get("minutes"), item.args.get("seconds")) {
        (Some(_), Some(_)) => {
            return Some(TaskResult::failed_with(
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
            return Some(TaskResult::failed_with(format!(
                "non-integer value given for prompt duration: {v}"
            )));
        }
    };
    // Python's clock counts nanoseconds in a signed 64-bit integer, and the reference fails a
    // pause it cannot hold rather than wait it out. Measured on ansible-core 2.19.12.
    if asked.is_some_and(|(seconds, _)| seconds > i64::MAX / 1_000_000_000) {
        return Some(TaskResult::failed_with(
            "Task failed: timestamp out of range for C PyTime_t",
        ));
    }
    let prompting = asked.is_none() || item.args.contains_key("prompt");
    if prompting && interactive {
        return Some(TaskResult::failed_with(PROMPT_REFUSED));
    }
    let mut r = Map::new();
    r.insert("start".into(), json!(timestamp()));
    let began = std::time::Instant::now();
    match asked {
        Some((seconds, _)) => {
            let wait = Duration::from_secs(seconds.max(1).unsigned_abs());
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = interrupted(stop) => return None,
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
    r.insert(
        "echo".into(),
        json!(
            item.args
                .get("echo")
                .and_then(volant_protocol::modules::arg_bool)
                .unwrap_or(true)
        ),
    );
    r.insert("rc".into(), json!(0));
    r.insert("stderr".into(), json!(""));
    r.insert("stdout".into(), json!(format!("Paused for {shown} {unit}")));
    r.insert("stop".into(), json!(timestamp()));
    r.insert("user_input".into(), json!(""));
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
    // judged the result, so `failed_when: false` cannot make a pass of it.
    if timed_out(&result) {
        return result;
    }
    match apply_conditions(task, item, result, templar) {
        Ok(r) => r,
        Err(e) => TaskResult::failed_with(e.0),
    }
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
) -> Result<TaskResult, TemplateError> {
    result.settle_failed();
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
        // Over a non-zero rc too: the condition has spoken.
        result.0.insert("failed".into(), json!(failed));
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
            "ignore_errors" => match &task.ignore_errors {
                Some(crate::playbook::Flag::Fixed(b)) => json!(b),
                Some(crate::playbook::Flag::Template(t)) => json!(t),
                None => Value::Null,
            },
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

/// Takes the `warnings` off every result of a task, for the driver to show as `[WARNING]:` lines
/// under it. Measured on ansible-core 2.19.12 for `pause`: the warning is shown once and the
/// registered result has no such key. Applied to a module's result the same way, unmeasured.
pub(super) fn take_warnings(results: &mut [(Option<Value>, TaskResult)]) -> Vec<String> {
    let mut out = Vec::new();
    for (_, result) in results.iter_mut() {
        if let Some(Value::Array(list)) = result.0.remove("warnings") {
            out.extend(list.into_iter().map(|w| match w {
                Value::String(s) => s,
                other => other.to_string(),
            }));
        }
    }
    out
}

/// The reason to end the run when a task that changed notifies a handler the play does not
/// have, in the sentence the pre-flight uses for the same name in the play itself.
///
/// Only a task spliced in by a dynamic include can get here: the pre-flight checks every other
/// `notify` before the first connection, and the handlers an included file may name are the
/// play's, which are only in hand here. The reference raises it from its strategy
/// (`ERROR_ON_MISSING_HANDLER`, on by default), so it ends the run rather than failing the task:
/// no `ignore_errors` and no `rescue` can take it. A task that changed nothing notifies nothing.
pub(super) fn unresolved_notify(
    compiled: &Compiled,
    task: &PlayTask,
    results: &[(Option<Value>, TaskResult)],
) -> Option<String> {
    // Only a result that changed and did not fail notifies: the reference looks for the handler
    // in the ok branch of `_process_pending_results` alone, so a failed task - `ignore_errors`
    // or not, a loop whose aggregate failed as well - never reaches the lookup. These are the
    // verdicts `failed_when` has already had its say on.
    if results.iter().any(|(_, r)| r.failed()) || !results.iter().any(|(_, r)| r.changed()) {
        return None;
    }
    let name = task
        .notify
        .iter()
        .find(|name| crate::compile::resolve_notify(compiled, name).is_empty())?;
    Some(format!(
        "The requested handler '{name}' was not found in either the main handlers list nor in the listening handlers list"
    ))
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
    let retries = match &task.retries {
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
        // `if delay < 0: delay = 1` in ansible-core 2.19.12 `task_executor.py`.
        Some(raw) => match number(raw, "delay")? {
            d if d < 0.0 => Duration::from_secs(1),
            d => Duration::from_secs_f64(d),
        },
        None => Duration::from_secs(5),
    };
    Ok(Some(Retry {
        retries,
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

/// What one run of a retried task decides.
pub(super) enum Attempt {
    /// The loop ends with this result.
    Done(TaskResult),
    /// Another run follows, after a retry line showing this count and the `delay`.
    Again(u32),
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
    let mut r = finish(task, item, raw, templar);
    r.0.insert("attempts".into(), json!(attempt));
    match until_holds(task, item, &r, retry, templar) {
        Err(e) => return Attempt::Done(TaskResult::failed_with(conditional_error(&e))),
        Ok(true) => return Attempt::Done(r),
        Ok(false) => {}
    }
    if attempt > retry.retries {
        r.0.insert("attempts".into(), json!(retry.retries));
        r.0.insert("failed".into(), json!(true));
        return Attempt::Done(r);
    }
    Attempt::Again(retry.retries + 1 - attempt)
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
    for (ii, item) in items.iter().enumerate() {
        // A skipped item already has its result and never had a task.
        if item.skipped.is_some() {
            continue;
        }
        match python {
            Ok(python) => out.push((ii, protocol_task(task, item, *python))),
            Err(failure) => results[ii] = Some(failure.clone()),
        }
    }
    out
}

/// The variables of the host a task's module actually runs on: the delegate's when the task has
/// one, the task's own host's otherwise.
///
/// One expression, used for the connection and for the interpreter both, because they have to be
/// the same host: a link opened to the delegate and an interpreter read off the delegating host
/// runs a module on one machine under the Python of another, which is a failure only a path that
/// does not exist on both would show.
pub(super) fn running_host_vars<'a>(
    delegate: Option<&'a (String, Map<String, Value>)>,
    items: &'a [Item],
) -> &'a Map<String, Value> {
    delegate.map_or(&items[0].vars.map, |(_, vars)| vars)
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
pub(super) fn requested_interpreter(vars: &Map<String, Value>) -> Option<String> {
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
            args: sub.args.clone(),
            ..protocol_task(task, item, None)
        });
    }
    let Some(payload) = union.and_then(|union| union.payload(sub.module)) else {
        return Err(no_payload(sub.module));
    };
    let interpreter = chosen_interpreter(asked, interpreters)?;
    Ok(Task {
        module: sub.module.to_string(),
        args: sub.args.clone(),
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
            .map(|(arg, blob)| volant_protocol::StagedFile {
                arg: arg.clone(),
                blob: blob.hash.clone(),
            })
            .collect(),
        force_python: payload.force_python,
    })
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
    let ends = limit.map(|t| Instant::now() + Duration::from_secs(t));
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
            return Ok(timeout_result());
        }
        let (sub, dropping) = match plugin.next(last.take()) {
            Step::Done(result) => return Ok(result),
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
                if failed > 0
                    && wait_or_stop(capped(relink.pause(failed).min(timeout)), stop, stop_broken)
                        .await
                        .is_none()
                {
                    return Err(Ok(BatchOutcome::Cancelled { at: 0 }));
                }
                if expired() {
                    return Ok(timeout_result());
                }
                let built = match sub_task(task, item, &probe, union, interpreters, asked) {
                    Ok(built) => bounded(built),
                    Err(failure) => return Ok(failure),
                };
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
                let once = async {
                    relink.relink(link, attempt).await?;
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
                    Ok(Err(why)) => gone(&why),
                    Err(_) => gone(&format!("no answer after {} seconds", left.as_secs_f64())),
                });
                continue;
            }
        };
        tries = None;
        let built = match sub_task(task, item, &sub, union, interpreters, asked) {
            Ok(built) => bounded(built),
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

/// What starting an action plugin reads besides the item it runs and the warnings it raises.
pub(super) struct PluginStart<'a> {
    pub(super) kind: crate::action_plugins::Kind,
    /// The variables of the host the module runs on - the delegate's when there is one.
    pub(super) running_vars: &'a Map<String, Value>,
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
/// between two attempts: nothing more runs, and the item has no result. `lefts` receives the
/// count each retry line shows.
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
    lefts: &mut Vec<u32>,
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
            Attempt::Again(left) => lefts.push(left),
        }
        if wait_or_stop(retry.delay, stop, stop_broken).await.is_none() {
            return Ok(None);
        }
    }
}

/// Waits `delay`, or gives up when the run is interrupted, before it or during it. A zero delay
/// still looks at the stop first, so a retry loop with no delay stops between two attempts
/// rather than starting the next one.
pub(super) async fn wait_or_stop(
    delay: Duration,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
) -> Option<()> {
    let deadline = tokio::time::Instant::now() + delay;
    loop {
        if *stop.borrow() {
            return None;
        }
        if delay.is_zero() {
            return Some(());
        }
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => return Some(()),
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
    Task {
        module: task.module.clone(),
        args: item.args.clone(),
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
    }
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
    async fn answer(&mut self) -> std::io::Result<Option<FromAgent>>;
    /// Asks for a batch to stop and waits for the agent to say it has, at most `grace`.
    async fn stop_batch(&mut self, id: u64, grace: Duration) -> bool;
    /// Where the batches over this channel leave their timings, when anything reads them.
    fn timings(&mut self) -> Option<&mut crate::profile::Ledger> {
        None
    }
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
        let stale: Vec<LinkKey> = self
            .links
            .keys()
            .filter(|k| k.host == self.key.host)
            .cloned()
            .collect();
        for key in stale {
            self.checked.remove(&key);
            if let Some(old) = self.links.remove(&key) {
                tokio::spawn(old.shutdown());
            }
        }
        // Checked again by the next batch if this never comes back up: the link left in place is
        // the one the host outlived.
        self.checked.remove(self.key);
        let fresh = connect(
            &with_connect_timeout(&self.key.transport, connect_timeout),
            self.agents,
            self.defaults,
            self.escalation,
        )
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

    async fn answer(&mut self) -> std::io::Result<Option<FromAgent>> {
        self.recv().await
    }

    async fn stop_batch(&mut self, id: u64, grace: Duration) -> bool {
        self.cancel(id, grace).await
    }

    fn timings(&mut self) -> Option<&mut crate::profile::Ledger> {
        Some(self.ledger())
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
    let state = match place_blob(link, host, hash, zip_b64, false, logs).await {
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
/// A `staged` file is sent without asking.
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
    b64: &str,
    staged: bool,
    logs: &mut Vec<String>,
) -> Result<bool, String> {
    let has = ToAgent::HasBlob { hash: hash.into() };
    if !staged && blob_state(link, host, hash, &has, logs).await? {
        return Ok(true);
    }
    let put = ToAgent::PutBlob {
        hash: hash.into(),
        zip_b64: b64.into(),
        staged,
    };
    blob_state(link, host, hash, &put, logs).await
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
        let placed_now = place_blob(link, host, &blob.hash, &blob.b64, true, logs)
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
    logs: &mut Vec<String>,
) -> Result<bool, String> {
    link.ask(msg)
        .await
        .map_err(|err| format!("asking the agent about the blob {hash}: {err}"))?;
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
    tasks: Vec<Task>,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    logs: &mut Vec<String>,
) -> (Vec<Option<TaskResult>>, Result<BatchOutcome, String>) {
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
                    add_output_lines(&mut result);
                    summarise_exception(&mut result);
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
    if let Some(ledger) = link.timings() {
        ledger.wire_micros += crate::profile::micros(started).saturating_sub(agent);
    }
    (received, ended)
}

/// `stdout_lines` and `stderr_lines`, for a result that carries `stdout` or `stderr` and not the
/// list beside it. The reference's `_execute_module` adds both to every
/// module result that lacks them, so a Python module's result has them there; here the agent
/// writes them for `command` and `raw` alone, and every result an agent sends passes through
/// [`run_agent_batch`], plugin sub-tasks included.
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
    if failed || sent.is_some_and(|e| !python_falsy(&e)) {
        result
            .0
            .insert("exception".into(), Value::from("(traceback unavailable)"));
    }
}

/// Whether Python's `bool()` is false for the value a module sent: `None`, `False`, a zero, or
/// an empty string, list or dict.
fn python_falsy(value: &Value) -> bool {
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
    let mut stale: Option<String> = None;
    let reboots = options.reboots.of(&key.host);
    if checked.insert(key.clone(), reboots) != Some(reboots)
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
pub(super) fn record_registered(
    vars: &mut VarStore,
    task: &PlayTask,
    targets: &[String],
    results: &[(Option<Value>, TaskResult)],
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
    #[tokio::test]
    async fn every_local_module_has_an_arm_in_run_local() {
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
                args_untrusted: BTreeSet::new(),
                vars: HostVars::default(),
                environment: BTreeMap::new(),
                ignore_errors: None,
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
                &mut watch::channel(false).1,
            )
            .await
            .expect("nothing stops this run");
            assert_ne!(
                result.0.get("msg").and_then(Value::as_str),
                Some(format!("{} is not a controller-side module", spec.name).as_str()),
                "{} is in LOCAL_MODULES with no arm in run_local",
                spec.name
            );
        }
    }

    fn local_item(args: Value) -> Item {
        Item {
            element: None,
            label: None,
            args: vars(args),
            args_untrusted: BTreeSet::new(),
            vars: HostVars::default(),
            environment: BTreeMap::new(),
            ignore_errors: None,
            skipped: None,
        }
    }

    /// One controller-side module run the way the driver runs it, with nothing interrupting.
    async fn local(module: &str, args: Value) -> TaskResult {
        local_with(module, args, HostVars::default()).await
    }

    /// `local` against the host variables given, the task carrying the arguments as written and
    /// the item carrying them as `prepare` renders them: per argument, with the names whose
    /// render read a managed host set apart.
    async fn local_with(module: &str, args: Value, host_vars: HostVars) -> TaskResult {
        let templar = Templar::new(PathBuf::from("."));
        let mut written = task(module);
        written.args = vars(args);
        let (rendered, untrusted) = templar
            .render_map_tainted(&written.args, &host_vars)
            .expect("the arguments render");
        let item = Item {
            args: rendered,
            args_untrusted: untrusted,
            vars: host_vars,
            ..local_item(json!({}))
        };
        let inventory = crate::inventory::Inventory::parse_ini("h1\n").expect("an inventory");
        let store = Mutex::new(
            VarStore::new(&inventory, None, Path::new("."), Map::new()).expect("a var store"),
        );
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
        run_local(
            &written,
            &item,
            &step,
            std::slice::from_ref(&"h1".to_string()),
            &templar,
            &store,
            0,
            &mut watch::channel(false).1,
        )
        .await
        .expect("nothing stops this run")
    }

    /// Measured on ansible-core 2.19.12: every condition true gives `All assertions passed`, and a
    /// `success_msg` takes its place.
    ///
    /// What would make this red: a passing assert reported failed, or `success_msg` dropped.
    #[tokio::test]
    async fn an_assert_that_holds_passes() {
        let r = local("assert", json!({"that": ["1 == 1", "2 > 1"]})).await;
        assert_eq!(
            Value::Object(r.0),
            json!({"changed": false, "msg": "All assertions passed"})
        );
        let r = local("assert", json!({"that": "1 == 1", "success_msg": "fine"})).await;
        assert_eq!(Value::Object(r.0), json!({"changed": false, "msg": "fine"}));
    }

    /// Measured on ansible-core 2.19.12: the failing element comes back as written, a string for
    /// `that: 1 == 2` and a boolean for `that: false`, and in a list it is the first false one.
    ///
    /// What would make this red: the rendered value or the last false element reported instead
    /// of the first one as written, or `evaluated_to` missing.
    #[tokio::test]
    async fn a_failed_assert_names_the_first_false_element_as_written() {
        let r = local("assert", json!({"that": "1 == 2"})).await;
        assert_eq!(
            Value::Object(r.0),
            json!({"assertion": "1 == 2", "changed": false, "evaluated_to": false, "exception": "(traceback unavailable)", "failed": true, "msg": "Assertion failed"})
        );
        let r = local("assert", json!({"that": false})).await;
        assert_eq!(r.0["assertion"], json!(false));
        let r = local("assert", json!({"that": ["1 == 1", "2 == 3", "4 == 5"]})).await;
        assert_eq!(r.0["assertion"], json!("2 == 3"));
        assert!(r.failed());
    }

    /// Measured on ansible-core 2.19.12: `fail_msg`, or its alias `msg`, replaces `msg` and leaves
    /// `assertion` and `evaluated_to` in place. `quiet` changes the display only, which
    /// `an_assert_shows_its_result_unless_quiet` pins.
    ///
    /// What would make this red: the alias refused or ignored, or `quiet` changing the result.
    #[tokio::test]
    async fn fail_msg_and_its_alias_replace_the_message_and_quiet_leaves_the_result_alone() {
        for key in ["fail_msg", "msg"] {
            let r = local("assert", json!({"that": "1 == 2", key: "custom"})).await;
            assert_eq!(
                Value::Object(r.0),
                json!({"assertion": "1 == 2", "changed": false, "evaluated_to": false, "exception": "(traceback unavailable)", "failed": true, "msg": "custom"}),
                "{key}"
            );
        }
        let loud = local("assert", json!({"that": "1 == 2"})).await;
        let quiet = local("assert", json!({"that": "1 == 2", "quiet": true})).await;
        assert_eq!(loud.0, quiet.0);
    }

    /// The three refusals measured on ansible-core 2.19.12, the first two in its own words. The
    /// third fails the way every other conditional in this engine fails.
    ///
    /// What would make this red: an assert with no `that` passing, an unknown argument accepted,
    /// the supported list or its alias spelled differently, or an undefined name panicking or
    /// reading as false.
    #[tokio::test]
    async fn assert_refuses_what_the_reference_refuses() {
        let r = local("assert", json!({})).await;
        assert!(r.failed());
        assert_eq!(r.0["msg"], json!("missing required arguments: that"));
        let r = local("assert", json!({"that": "1 == 1", "nosucharg": 1})).await;
        assert!(r.failed());
        assert_eq!(
            r.0["msg"],
            json!(
                "Unsupported parameters for (ansible_collections.ansible.builtin.plugins.action.assert) module: nosucharg. Supported parameters include: fail_msg, quiet, success_msg, that (msg)."
            )
        );
        // The reference ends this one with `'some_undefined_thing' is undefined`. The prefix is
        // the one every conditional here shares; the sentence behind it is the templar's.
        let r = local("assert", json!({"that": "some_undefined_thing"})).await;
        assert!(r.failed());
        assert!(
            r.0["msg"]
                .as_str()
                .is_some_and(|m| m.starts_with("Task failed: Error while evaluating conditional: ")),
            "{:?}",
            r.0
        );
    }

    /// A host's variables where `r` is a registered result, as `record_registered` leaves it.
    fn with_registered(extra: Value) -> HostVars {
        let mut v = hvars(extra);
        v.insert_untrusted("r".into(), json!({"rc": 0, "stdout": "1 == 2"}));
        v
    }

    /// Measured on ansible-core 2.19.12, after `register: r` on `command: echo "1 == 2"`: the
    /// three forms below pass, the first two with the deprecation warning for the braces.
    ///
    /// What would make this red: `that` rendered along with the other arguments and refused
    /// because that render read a registered value, although the render left no text to compile.
    #[tokio::test]
    async fn a_that_reading_a_registered_value_reads_it_as_data() {
        for that in [
            json!("{{ r.rc == 0 }}"),
            json!(["1 == 1", "{{ r.rc == 0 }}"]),
            json!("r.stdout == '1 == 2'"),
        ] {
            let r = local_with("assert", json!({"that": that}), with_registered(json!({}))).await;
            assert_eq!(
                Value::Object(r.0),
                json!({"changed": false, "msg": "All assertions passed"}),
                "{that}"
            );
        }
    }

    /// `that` is evaluated by the code that evaluates `when`, element by element, from the text
    /// the playbook wrote. Asserted against `when` rather than against the reference because
    /// the two agree in the reference as well: measured on ansible-core 2.19.12, every case
    /// here gives an assert and a `when` the same outcome. Both evaluate again the untainted
    /// string a `{{ }}` conditional renders to, as the reference does, and refuse a tainted one.
    ///
    /// What would make this red: `that` evaluated by code of its own, so that the two part ways
    /// on any case here; or a `that` whose render read a host value refused in words of its own
    /// rather than `when`'s. It cannot catch a regression of the second evaluation itself, which
    /// moves both together; `Templar::condition`'s own tests hold that.
    #[tokio::test]
    async fn a_that_is_evaluated_the_way_when_is() {
        let host_vars = with_registered(json!({"healthy": "true"}));
        let templar = Templar::new(PathBuf::from("."));
        for that in [
            "{{ healthy }}",
            "healthy",
            "{{ r.stdout }}",
            "r.stdout",
            "{{ 1 == 2 }}",
            "{{ r.rc == 0 }}",
        ] {
            let asserted = local_with("assert", json!({"that": that}), host_vars.clone()).await;
            match templar.condition(that, &host_vars) {
                Ok(held) => assert_eq!(asserted.failed(), !held, "{that}: {:?}", asserted.0),
                Err(e) => assert_eq!(
                    asserted.0["msg"],
                    json!(conditional_error(&e)),
                    "{that}: {:?}",
                    asserted.0
                ),
            }
        }
    }

    /// Measured on ansible-core 2.19.12: a `that` that is one template naming a list is that
    /// list, each element a condition, and the one that fails is reported as the list held it.
    /// A list a managed host wrote is data, and its strings are not compiled.
    ///
    /// What would make this red: the list itself taken as the condition, or a host's strings
    /// evaluated on the controller.
    #[tokio::test]
    async fn a_that_naming_a_list_is_that_list() {
        let host_vars = hvars(json!({"good": ["1 == 1", "2 == 2"], "bad": ["1 == 1", "1 == 2"]}));
        let r = local_with("assert", json!({"that": "{{ good }}"}), host_vars.clone()).await;
        assert!(!r.failed(), "{:?}", r.0);
        let r = local_with("assert", json!({"that": "{{ bad }}"}), host_vars).await;
        assert_eq!(r.0["assertion"], json!("1 == 2"), "{:?}", r.0);
        let mut host_vars = HostVars::default();
        host_vars.insert_untrusted("remote".into(), json!(["1 == 1"]));
        let r = local_with("assert", json!({"that": "{{ remote }}"}), host_vars).await;
        assert_eq!(
            r.0["msg"],
            json!(
                "Task failed: Error while evaluating conditional: Encountered untrusted template or expression."
            ),
            "{:?}",
            r.0
        );
    }

    /// A failed controller-side task registers `exception: "(traceback unavailable)"`, as an
    /// agent's does: the reference's `TaskExecutor._execute` normalises every task's final
    /// result with `maybe_raise_on_result`, wherever it ran. One that succeeds carries none.
    ///
    /// What would make this red: the normalisation left to the agent's results alone, which
    /// registers no `exception` for a failed `fail` or `assert`.
    #[tokio::test]
    async fn a_failed_controller_side_task_registers_an_exception() {
        for (module, args) in [("fail", json!({})), ("assert", json!({"that": "1 == 2"}))] {
            let r = local(module, args).await;
            assert_eq!(
                r.0["exception"],
                json!("(traceback unavailable)"),
                "{module}"
            );
        }
        let r = local("debug", json!({"msg": "hi"})).await;
        assert!(!r.0.contains_key("exception"), "{:?}", r.0);
    }

    /// Measured on ansible-core 2.19.12: the default message, a custom one, and a list that stays
    /// a list. An unknown argument is refused in a sentence of its own, unlike assert and pause.
    ///
    /// What would make this red: `msg` turned into a string, the default reworded, or the
    /// refusal written like the other two.
    #[tokio::test]
    async fn fail_fails_with_its_message_as_given() {
        let r = local("fail", json!({})).await;
        assert_eq!(
            Value::Object(r.0),
            json!({"changed": false, "exception": "(traceback unavailable)", "failed": true,
                   "msg": "Failed as requested from task"})
        );
        let r = local("fail", json!({"msg": "custom failure"})).await;
        assert_eq!(r.0["msg"], json!("custom failure"));
        let r = local("fail", json!({"msg": ["one", "two"]})).await;
        assert_eq!(r.0["msg"], json!(["one", "two"]));
        assert!(r.failed());
        let r = local("fail", json!({"nosucharg": 1})).await;
        assert_eq!(
            r.0["msg"],
            json!("Invalid options for ansible.builtin.fail: nosucharg")
        );
    }

    /// Measured on ansible-core 2.19.12: `seconds: 0` still waits one second and says so.
    ///
    /// What would make this red: a zero pause that does not wait, or a result whose keys or
    /// wording differ from the reference's.
    #[tokio::test]
    async fn a_pause_of_zero_seconds_waits_one() {
        let began = std::time::Instant::now();
        let mut r = local("pause", json!({"seconds": 0})).await;
        assert!(
            began.elapsed() >= Duration::from_secs(1),
            "{:?}",
            began.elapsed()
        );
        for stamp in ["start", "stop"] {
            let value = r.0.remove(stamp).expect(stamp);
            assert_eq!(value.as_str().map(str::len), Some(26), "{value}");
        }
        assert_eq!(
            Value::Object(r.0),
            json!({"changed": false, "delta": 1, "echo": true, "rc": 0, "stderr": "", "stdout": "Paused for 1.0 seconds", "user_input": ""})
        );
    }

    /// Measured on ansible-core 2.19.12: `minutes: 153722867280912931` fails at once, because
    /// Python's clock cannot hold that many nanoseconds. The bound is that clock's, a signed
    /// 64-bit count of nanoseconds, so `seconds` has it too.
    ///
    /// What would make this red: the duration multiplied without a check, which panics in a
    /// debug build and in a release build wraps to a pause of the wrong length that reports
    /// `ok`.
    #[tokio::test]
    async fn a_pause_too_long_for_the_clock_fails_at_once() {
        for args in [
            json!({"minutes": 153_722_867_280_912_931_i64}),
            json!({"seconds": 9_223_372_037_i64}),
        ] {
            let r = tokio::time::timeout(Duration::from_secs(5), local("pause", args.clone()))
                .await
                .expect("the pause fails rather than waits");
            assert_eq!(
                Value::Object(r.0),
                json!({"exception": "(traceback unavailable)", "failed": true, "msg": "Task failed: timestamp out of range for C PyTime_t"}),
                "{args}"
            );
        }
    }

    /// Measured on ansible-core 2.19.12, the two refusals of a pause.
    ///
    /// What would make this red: both durations accepted, one of them silently winning, or an
    /// unknown argument accepted.
    #[tokio::test]
    async fn pause_refuses_what_the_reference_refuses() {
        let r = local("pause", json!({"minutes": 1, "seconds": 1})).await;
        assert!(r.failed());
        assert_eq!(
            r.0["msg"],
            json!("parameters are mutually exclusive: minutes|seconds")
        );
        let r = local("pause", json!({"seconds": 1, "nosucharg": 1})).await;
        assert!(r.failed());
        assert_eq!(
            r.0["msg"],
            json!(
                "Unsupported parameters for (ansible_collections.ansible.builtin.plugins.action.pause) module: nosucharg. Supported parameters include: echo, minutes, prompt, seconds."
            )
        );
    }

    /// Measured on ansible-core 2.19.12 with standard input not a terminal: a prompt warns, does
    /// not wait, and reports in minutes.
    ///
    /// What would make this red: a prompt that waits for an answer nobody can type, or a result
    /// that loses the warning.
    #[tokio::test]
    async fn a_prompt_without_a_terminal_warns_and_goes_on() {
        let item = local_item(json!({"prompt": "Continue?"}));
        let r = pause(&item, false, &mut watch::channel(false).1)
            .await
            .expect("nothing stops this run");
        assert!(!r.failed(), "{:?}", r.0);
        assert_eq!(r.0["delta"], json!(0));
        assert_eq!(r.0["stdout"], json!("Paused for 0.0 minutes"));
        assert_eq!(r.0["user_input"], json!(""));
        assert_eq!(
            r.0["warnings"],
            json!(["Not waiting for response to prompt as stdin is not interactive"])
        );
    }

    /// On a terminal a prompt is a person waiting to answer, and this release cannot read the
    /// answer. Showing the prompt and going on would accept the task and not do it, so it is
    /// refused, and so is a pause with no duration, which waits for Enter in the reference.
    ///
    /// What would make this red: a prompt on a terminal that returns `ok` without waiting.
    #[tokio::test]
    async fn a_prompt_on_a_terminal_is_refused() {
        for args in [
            json!({"prompt": "Continue?"}),
            json!({}),
            json!({"prompt": "Go?", "seconds": 1}),
        ] {
            let r = pause(
                &local_item(args.clone()),
                true,
                &mut watch::channel(false).1,
            )
            .await
            .expect("nothing stops this run");
            assert_eq!(
                Value::Object(r.0),
                json!({"failed": true, "msg": PROMPT_REFUSED}),
                "{args}"
            );
        }
    }

    /// An interrupted run does not sit out the rest of a pause, and the pause reports no result:
    /// the driver ends the host's run on it, as it does on every other wait the stop ends, with
    /// no task line and no count.
    ///
    /// What would make this red: a pause that waits on its timer alone, which holds a cancelled
    /// run for as long as the playbook asked to wait (the five-second bound makes that red in
    /// five seconds rather than ten minutes); or the interruption reported as a failed task,
    /// `user requested abort!`, which counts `failed=1` for a run the operator stopped.
    #[tokio::test]
    async fn an_interrupted_pause_ends_at_once() {
        let (stop_tx, mut stop) = watch::channel(false);
        let item = local_item(json!({"minutes": 10}));
        let (r, ()) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(5), pause(&item, false, &mut stop)),
            async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                stop_tx.send(true).expect("the receiver is alive");
            }
        );
        let r = r.expect("the interruption ends the pause");
        assert!(r.is_none(), "{r:?}");
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
                args_untrusted: BTreeSet::new(),
                vars: hvars(host),
                environment: BTreeMap::new(),
                ignore_errors: None,
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

    /// Every result that ran, module or controller, comes out with `failed` set, the way
    /// ansible-core's `TaskExecutor._execute` sets it before `register`: kept when the result
    /// says it, otherwise `true` for a non-zero `rc` and `false` for anything else. Measured on
    /// 2.19.12, registered values read `failed=False` for `ping`, `command: "true"` and `stat`,
    /// `failed=True` for a `command: "false"` and a `file` that failed, and `ABSENT` for a task
    /// `when` skipped, which never gets here.
    ///
    /// What would make this red: `failed` left absent, so `r.failed` is undefined on a
    /// registered `ping`; or written `false` over a failure, which would be far worse - a task
    /// that failed and reads as a success.
    #[test]
    fn a_result_that_ran_says_whether_it_failed() {
        let templar = Templar::new(std::env::temp_dir());
        let t = task("command");
        let item = Item {
            element: None,
            label: None,
            args: Map::new(),
            args_untrusted: BTreeSet::new(),
            vars: HostVars::default(),
            environment: BTreeMap::new(),
            ignore_errors: None,
            skipped: None,
        };
        let failed = |r: Value| {
            let r = apply_conditions(&t, &item, result(r), &templar).unwrap();
            r.0.get("failed").cloned()
        };
        assert_eq!(failed(json!({"ping": "pong"})), Some(json!(false)));
        assert_eq!(failed(json!({"rc": 0})), Some(json!(false)));
        assert_eq!(failed(json!({"rc": 1})), Some(json!(true)));
        assert_eq!(
            failed(json!({"failed": true, "msg": "missing"})),
            Some(json!(true))
        );
        assert_eq!(
            failed(json!({"rc": 1, "failed": false})),
            Some(json!(false))
        );
        // Measured on 2.19.12 through a module printing each shape: a failure registers `True`
        // whatever made it one, and a falsy `failed` is kept as written.
        assert_eq!(failed(json!({"rc": "2"})), Some(json!(true)));
        assert_eq!(failed(json!({"rc": null})), Some(json!(true)));
        assert_eq!(failed(json!({"failed": "yes"})), Some(json!(true)));
        assert_eq!(failed(json!({"failed": 1})), Some(json!(true)));
        assert_eq!(failed(json!({"failed": 0})), Some(json!(0)));
        assert_eq!(failed(json!({"failed": null})), Some(json!(null)));
        let mut t = task("command");
        t.failed_when = vec!["false".into()];
        let r = apply_conditions(&t, &item, result(json!({"rc": 0})), &templar).unwrap();
        assert_eq!(r.0.get("failed"), Some(&json!(false)), "{:?}", r.0);
    }

    /// The recap the reference printed for these five tasks on 2.19.12, measured with a module
    /// printing each result: two ignored failures reporting `changed: true` and `changed: "yes"`,
    /// a `changed: "yes"` under `changed_when: false`, a rescued failure reporting
    /// `changed: true`, and the rescue's own `debug` - `ok=4 changed=2 rescued=1 ignored=2`
    /// (the play's last `debug` made the reference's `ok=5`).
    ///
    /// What would make this red: `changed: "yes"` read as unchanged; `changed_when: false` losing
    /// to it; or a failure that stopped the host counted as a change.
    #[test]
    fn the_recap_counts_changed_the_way_the_reference_does() {
        let templar = Templar::new(std::env::temp_dir());
        let item = Item {
            element: None,
            label: None,
            args: Map::new(),
            args_untrusted: BTreeSet::new(),
            vars: HostVars::default(),
            environment: BTreeMap::new(),
            ignore_errors: None,
            skipped: None,
        };
        let plain = task("command");
        let mut quiet = task("command");
        quiet.changed_when = vec!["false".into()];
        let tasks = [
            (
                &plain,
                json!({"changed": true, "failed": true}),
                true,
                false,
            ),
            (
                &plain,
                json!({"changed": "yes", "failed": true}),
                true,
                false,
            ),
            (&quiet, json!({"changed": "yes"}), false, false),
            (
                &plain,
                json!({"changed": true, "failed": true}),
                false,
                true,
            ),
            (&plain, json!({"changed": false}), false, false),
        ];
        let mut stats = crate::stats::Stats::default();
        for (t, shape, ignore_errors, rescuable) in tasks {
            let r = apply_conditions(t, &item, result(shape), &templar).unwrap();
            stats.record("h", classify(&r, ignore_errors, rescuable), r.changed());
        }
        let h = stats.host("h");
        assert_eq!(
            (h.ok, h.changed, h.rescued, h.ignored, h.failed),
            (4, 2, 1, 2, 0)
        );
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
            args_untrusted: BTreeSet::new(),
            vars: HostVars::default(),
            environment: BTreeMap::new(),
            ignore_errors: None,
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

    /// A plugin that reconnects, where the test expects none to: every try fails.
    struct NoRelink;

    impl<C> Relink<C> for NoRelink {
        async fn relink(&mut self, _link: &mut C, _: Option<Duration>) -> Result<(), String> {
            Err("this test opens no connection".into())
        }
    }

    /// An agent that answers what the test scripted, and remembers what a link remembers.
    struct FakeAgent {
        sent: Vec<ToAgent>,
        answers: std::collections::VecDeque<FromAgent>,
        memory: BlobMemory,
        /// Whether a fake that has run out of answers holds the line open instead of closing it,
        /// which is what a real agent busy with a batch does.
        hangs_when_empty: bool,
        ledger: crate::profile::Ledger,
    }

    impl FakeAgent {
        fn answering(answers: Vec<FromAgent>) -> Self {
            FakeAgent {
                sent: Vec::new(),
                answers: answers.into(),
                memory: BlobMemory::default(),
                hangs_when_empty: false,
                ledger: crate::profile::Ledger::default(),
            }
        }

        fn puts(&self) -> usize {
            self.sent
                .iter()
                .filter(|m| matches!(m, ToAgent::PutBlob { .. }))
                .count()
        }
    }

    impl AgentChannel for FakeAgent {
        fn memory(&mut self) -> &mut BlobMemory {
            &mut self.memory
        }

        async fn ask(&mut self, msg: &ToAgent) -> std::io::Result<()> {
            self.sent.push(msg.clone());
            Ok(())
        }

        async fn answer(&mut self) -> std::io::Result<Option<FromAgent>> {
            match self.answers.pop_front() {
                Some(answer) => Ok(Some(answer)),
                None if self.hangs_when_empty => std::future::pending().await,
                None => Ok(None),
            }
        }

        async fn stop_batch(&mut self, id: u64, _grace: Duration) -> bool {
            self.sent.push(ToAgent::Cancel { id });
            true
        }

        fn timings(&mut self) -> Option<&mut crate::profile::Ledger> {
            Some(&mut self.ledger)
        }
    }

    fn state(hash: &str, present: bool) -> FromAgent {
        FromAgent::BlobState {
            hash: hash.to_string(),
            present,
        }
    }

    /// A payload the agent already holds is not sent again, and a payload it does not hold is
    /// sent once for the whole link rather than once per batch.
    ///
    /// What would make this red: the `has_blob` question dropped, or its answer ignored, either
    /// of which puts 631 KB on the wire before every batch of every run.
    #[tokio::test]
    async fn a_payload_travels_once_per_link() {
        let mut logs = Vec::new();
        let mut agent = FakeAgent::answering(vec![state("ab", false), state("ab", true)]);
        ensure_blob(&mut agent, "h1", "ab", "UEsD", &mut logs)
            .await
            .expect("the agent stored it");
        assert_eq!(agent.puts(), 1);
        ensure_blob(&mut agent, "h1", "ab", "UEsD", &mut logs)
            .await
            .expect("it is already there");
        assert_eq!(agent.puts(), 1, "{:?}", agent.sent);

        let mut held = FakeAgent::answering(vec![state("cd", true)]);
        ensure_blob(&mut held, "h1", "cd", "UEsD", &mut logs)
            .await
            .expect("the agent already holds it");
        assert_eq!(held.puts(), 0, "{:?}", held.sent);
    }

    /// A payload the agent refuses fails the batch, and the next batch of that host does not
    /// send it again blindly.
    ///
    /// What would make this red: the refusal forgotten, which re-uploads the same rejected
    /// payload before every batch for as long as the play lasts, each time failing the same
    /// way; or the refusal read as success, which runs the batch against a host that holds no
    /// payload and blames the module for it.
    #[tokio::test]
    async fn a_refused_payload_fails_the_batch_and_is_not_sent_again() {
        let mut logs = Vec::new();
        let mut agent = FakeAgent::answering(vec![state("ab", false), state("ab", false)]);
        let err = ensure_blob(&mut agent, "h1", "ab", "UEsD", &mut logs)
            .await
            .expect_err("the agent refused it");
        assert!(err.contains("ab"), "{err}");
        let again = ensure_blob(&mut agent, "h1", "ab", "UEsD", &mut logs)
            .await
            .expect_err("the refusal is remembered");
        assert_eq!(again, err);
        assert_eq!(agent.puts(), 1, "{:?}", agent.sent);
    }

    /// An item that cannot be sent still reports, so the step gets a task line.
    ///
    /// What would make this red: the failure dropped instead of written into the item's slot.
    /// The driver then has no result for that item, the reporting loop breaks on the first
    /// `None`, the step is reported **not at all**, the empty batch comes back `Completed`, and
    /// the play finishes `ok` having run nothing - no line, no failure, nothing in the recap.
    /// That is the failure this whole engine is written against.
    #[test]
    fn an_item_that_cannot_be_sent_still_reports() {
        let items = [bare_item(), bare_item()];
        let refused: Result<Option<(&ModulePayload, &str)>, TaskResult> = Err(
            TaskResult::failed_with("The module interpreter was not found."),
        );
        let mut results = vec![None, None];
        let sent = step_tasks(&task("ping"), &items, &refused, &mut results);
        assert!(sent.is_empty(), "nothing can be sent: {sent:?}");
        for slot in &results {
            let result = slot.as_ref().expect("every item of the step reports");
            assert!(result.failed(), "{result:?}");
        }
    }

    /// A step that can be sent leaves the results alone and carries every item that is not
    /// skipped, keyed by its own index.
    ///
    /// What would make this red: a skipped item counted in, which sends a task the `when` left
    /// out, or the index lost, which files an item's result under another item.
    #[test]
    fn a_step_that_travels_carries_the_items_that_are_not_skipped() {
        let mut items = [bare_item(), bare_item()];
        items[0].skipped = Some(TaskResult::default());
        let mut results = vec![None, None];
        let sent = step_tasks(&task("command"), &items, &Ok(None), &mut results);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, 1, "the item's own index travels with it");
        assert!(results.iter().all(Option::is_none), "{results:?}");
    }

    /// A module that needs a payload and has none is refused by name rather than sent.
    ///
    /// What would make this red: the task sent as if it were native, which the agent answers
    /// with its unknown-module sentence - read by an operator as a typo in a module name that is
    /// spelled perfectly. A dynamic include names its file while the play runs, so a module only
    /// it mentions is not in the blob this run built before connecting.
    #[test]
    fn a_module_with_no_payload_built_for_it_is_refused_by_name() {
        let failure = python_for("lineinfile", None, None)
            .expect_err("nothing was built for a module the union never saw");
        let msg = failure.0.get("msg").and_then(Value::as_str).unwrap_or("");
        assert!(msg.contains("lineinfile"), "{msg}");
        assert!(msg.contains("python payload"), "{msg}");
        assert!(
            python_for("command", None, None)
                .expect("a native module needs none")
                .is_none()
        );
    }

    /// The module runs where the link goes, so its interpreter is read off that host: the
    /// delegate's variables when the task is delegated.
    ///
    /// What would make this red: the delegating host's map read instead, which is what the
    /// expression looks like it could be simplified to. A task `delegate_to: build-host`, where
    /// `build-host` carries its own `ansible_python_interpreter`, would then run its module on
    /// `build-host` under the delegating host's Python - rc 127 if that path is not there, and
    /// silently the wrong Python if it is.
    #[test]
    fn the_interpreter_comes_from_the_host_the_module_runs_on() {
        let mut item = bare_item();
        item.vars = hvars(json!({"ansible_python_interpreter": "/usr/bin/python3"}));
        let items = [item];
        assert_eq!(
            requested_interpreter(running_host_vars(None, &items)),
            Some("/usr/bin/python3".to_string())
        );
        let delegate = (
            "build-host".to_string(),
            vars(json!({"ansible_python_interpreter": "/opt/py311/bin/python"})),
        );
        assert_eq!(
            requested_interpreter(running_host_vars(Some(&delegate), &items)),
            Some("/opt/py311/bin/python".to_string())
        );
    }

    /// The four discovery modes are not interpreters, and neither is a blank value.
    ///
    /// What would make this red: `auto_silent` sent as a path. It is the usual way an inventory
    /// silences the discovery warning, and `auto` is ansible-core's own default, so a group that
    /// sets one would have every Python task on every host in it fail at rc 127 naming
    /// `auto_silent` as a file, where the reference discovers an interpreter and runs. A blank
    /// value is the same shape - `"{{ py_override | default('') }}"` asked for nothing - and is
    /// filtered the way a blank `ansible_remote_tmp` already is.
    #[test]
    fn the_discovery_modes_and_a_blank_value_mean_nobody_named_one() {
        for mode in ["auto", "auto_legacy", "auto_silent", "auto_legacy_silent"] {
            assert_eq!(
                requested_interpreter(&vars(json!({ "ansible_python_interpreter": mode }))),
                None,
                "{mode} is a discovery mode, not a path"
            );
        }
        assert_eq!(
            requested_interpreter(&vars(json!({"ansible_python_interpreter": "   "}))),
            None
        );
        assert_eq!(
            requested_interpreter(&vars(
                json!({"ansible_python_interpreter": " /usr/bin/python3 "})
            )),
            Some("/usr/bin/python3".to_string())
        );
    }

    /// A run told to stop asks the agent to stop the batch it is running.
    ///
    /// What would make this red: the cancel never sent - an operator's interruption then leaves
    /// the agent running the batch to completion on the host while the controller walks away, so
    /// the run reports as stopped and the host keeps changing.
    #[tokio::test]
    async fn a_stopped_run_asks_the_agent_to_stop_the_batch() {
        let (stop_tx, mut stop) = watch::channel(false);
        let mut stop_broken = false;
        let mut logs = Vec::new();
        let mut agent = FakeAgent::answering(Vec::new());
        agent.hangs_when_empty = true;
        stop_tx.send(true).expect("the receiver is alive");
        let tasks = vec![protocol_task(&task("command"), &bare_item(), None)];
        let (_, ended) = run_agent_batch(
            &mut agent,
            "h1",
            3,
            tasks,
            None,
            &[],
            &mut stop,
            &mut stop_broken,
            &mut logs,
        )
        .await;
        assert!(
            matches!(ended, Ok(BatchOutcome::Cancelled { .. })),
            "{ended:?}"
        );
        assert!(
            agent
                .sent
                .iter()
                .any(|msg| matches!(msg, ToAgent::Cancel { id: 3 })),
            "{:?}",
            agent.sent
        );
    }

    /// A failed module's `exception` is the sentence the reference registers, whatever the module
    /// sent, and a result that neither failed nor sent one has none.
    ///
    /// Measured on ansible-core 2.19.12, `connection: local`: `copy` with a `validate` naming a
    /// program that is not there sends an error summary (`fail_json` given the `OSError`), a
    /// `lineinfile` on a missing file sends no `exception` at all, and a `command: false` fails
    /// too; each one registers `exception: "(traceback unavailable)"`, a `str`, and a `stat` that
    /// succeeds registers no `exception`. Volant registered the first as the module's serialized
    /// `{"__ansible_type": "ErrorSummary", ...}` and the other two without the key.
    ///
    /// What would make this red: the normalisation in `run_agent_batch` removed, which leaves the
    /// serialized summary in the first result and nothing in the second; or an `exception` put on
    /// a result that succeeded.
    #[tokio::test]
    async fn a_failed_module_registers_the_error_summary_the_reference_does() {
        let summary = json!({
            "__ansible_type": "ErrorSummary",
            "event": {"msg": "Error executing command."}
        });
        let answers = [
            json!({"failed": true, "msg": "Error executing command.", "exception": summary}),
            json!({"failed": true, "msg": "Destination /x does not exist !", "rc": 257}),
            json!({"changed": false, "stat": {"exists": true}, "exception": ""}),
        ];
        for (answer, want) in answers.into_iter().zip([
            Some("(traceback unavailable)"),
            Some("(traceback unavailable)"),
            None,
        ]) {
            let mut stop = watch::channel(false).1;
            let mut stop_broken = false;
            let mut logs = Vec::new();
            let mut agent = FakeAgent::answering(one_result(4, answer.clone()).to_vec());
            let tasks = vec![protocol_task(&task("copy"), &bare_item(), None)];
            let (received, _) = run_agent_batch(
                &mut agent,
                "h1",
                4,
                tasks,
                None,
                &[],
                &mut stop,
                &mut stop_broken,
                &mut logs,
            )
            .await;
            let result = received[0].clone().expect("a result").0;
            assert_eq!(
                result.get("exception").and_then(Value::as_str),
                want,
                "{answer} gave {result:?}"
            );
            if want.is_none() {
                assert!(!result.contains_key("exception"), "{result:?}");
            }
        }
    }

    /// The agent's times are the host's words: two tasks reporting `u64::MAX` cannot overflow
    /// the sum the wire time is taken from.
    ///
    /// What would make this red: a plain `+=`, which panics in a debug build.
    #[tokio::test]
    async fn a_host_s_times_cannot_overflow_the_wire_time() {
        let ran = volant_protocol::Ran {
            path: volant_protocol::ExecPath::Native,
            reason: None,
            micros: u64::MAX,
            fork_micros: None,
            import_micros: None,
            module_micros: None,
        };
        let answer = |index| FromAgent::TaskResult {
            batch: 4,
            index,
            result: TaskResult(vars(json!({"changed": false}))),
            ran: Some(ran.clone()),
        };
        let mut agent = FakeAgent::answering(vec![
            answer(0),
            answer(1),
            FromAgent::BatchDone {
                batch: 4,
                outcome: BatchOutcome::Completed,
            },
        ]);
        let tasks = vec![
            protocol_task(&task("stat"), &bare_item(), None),
            protocol_task(&task("stat"), &bare_item(), None),
        ];
        let (received, _) = run_agent_batch(
            &mut agent,
            "h1",
            4,
            tasks,
            None,
            &[],
            &mut watch::channel(false).1,
            &mut false,
            &mut Vec::new(),
        )
        .await;
        assert!(received.iter().all(Option::is_some));
        assert_eq!(agent.ledger.ran.len(), 2);
        assert_eq!(agent.ledger.wire_micros, 0);
    }

    /// What the agent says about how it ran each task is kept with the module that was asked
    /// for, by the task's position in the batch, for the driver to file in the profile.
    ///
    /// What would make this red: `ran` dropped where the result is read, or filed under another
    /// task's module.
    #[tokio::test]
    async fn a_batch_keeps_how_the_agent_ran_each_task() {
        let ran = volant_protocol::Ran {
            path: volant_protocol::ExecPath::Fallback,
            reason: Some("validate".into()),
            micros: 40,
            fork_micros: None,
            import_micros: None,
            module_micros: None,
        };
        let mut agent = FakeAgent::answering(vec![
            FromAgent::TaskResult {
                batch: 4,
                index: 1,
                result: TaskResult(vars(json!({"changed": false}))),
                ran: Some(ran.clone()),
            },
            FromAgent::TaskResult {
                batch: 4,
                index: 0,
                result: TaskResult(vars(json!({"changed": false}))),
                ran: None,
            },
            FromAgent::BatchDone {
                batch: 4,
                outcome: BatchOutcome::Completed,
            },
        ]);
        let tasks = vec![
            protocol_task(&task("command"), &bare_item(), None),
            protocol_task(&task("stat"), &bare_item(), None),
        ];
        let (received, _) = run_agent_batch(
            &mut agent,
            "h1",
            4,
            tasks,
            None,
            &[],
            &mut watch::channel(false).1,
            &mut false,
            &mut Vec::new(),
        )
        .await;
        assert!(received.iter().all(Option::is_some));
        assert_eq!(
            agent.ledger.ran,
            [
                (1, "stat".to_string(), Some(ran)),
                (0, "command".to_string(), None)
            ]
        );
    }

    /// A result carrying `stdout` or `stderr` comes back with the `_lines` of each, split the way
    /// Python's `str.splitlines()` splits, whatever module produced it. The reference's
    /// `_execute_module` adds them to every module result that lacks them, so a Python module's
    /// `register` reads `r.stderr_lines` there; the agent adds them for `command` and `raw` only.
    ///
    /// What would make this red: the addition in `run_agent_batch` removed, which leaves
    /// `failed_when: "'error' in r.stderr_lines"` on a `pip` task reading an undefined name; or
    /// a list the module wrote itself replaced, or one built from a value that is not a string.
    #[tokio::test]
    async fn a_module_result_gains_the_lines_of_its_output() {
        let mut stop = watch::channel(false).1;
        let mut stop_broken = false;
        let mut logs = Vec::new();
        let mut agent = FakeAgent::answering(
            one_result(
                4,
                json!({
                    "stdout": "a\nb\r\nc\rd\u{2028}e\x0bf\x1cg\u{85}h\u{2029}i\n",
                    "stderr": "",
                    "rc": 0
                }),
            )
            .to_vec(),
        );
        let tasks = vec![protocol_task(&task("pip"), &bare_item(), None)];
        let (received, _) = run_agent_batch(
            &mut agent,
            "h1",
            4,
            tasks,
            None,
            &[],
            &mut stop,
            &mut stop_broken,
            &mut logs,
        )
        .await;
        let result = received[0].clone().expect("a result").0;
        assert_eq!(
            result["stdout_lines"],
            json!(["a", "b", "c", "d", "e", "f", "g", "h", "i"])
        );
        assert_eq!(result["stderr_lines"], json!([]));

        let mut kept = TaskResult(vars(
            json!({"stdout": "one", "stdout_lines": ["kept"], "stderr": 3}),
        ));
        add_output_lines(&mut kept);
        assert_eq!(kept.0["stdout_lines"], json!(["kept"]));
        assert!(!kept.0.contains_key("stderr_lines"), "{:?}", kept.0);

        // `data.get('stdout') or ''`: a value Python reads as false gives an empty list.
        for falsy in [json!(null), json!(false), json!(0)] {
            let mut empty = TaskResult(vars(json!({"stdout": falsy, "stderr": falsy})));
            add_output_lines(&mut empty);
            assert_eq!(empty.0["stdout_lines"], json!([]), "{falsy}");
            assert_eq!(empty.0["stderr_lines"], json!([]), "{falsy}");
        }
    }

    /// A union holding every module a plugin may run, each keyed by its own short name.
    fn union_of(hash: &str, modules: &[&str]) -> Union {
        Union {
            modules: modules
                .iter()
                .map(|m| {
                    let facts = crate::python::ModuleFacts {
                        module_fqn: format!("ansible.modules.{m}"),
                        ..module_payload(hash).facts
                    };
                    ((*m).to_string(), facts)
                })
                .collect(),
            ..union_named(hash)
        }
    }

    /// What the agent answers for a batch of one task that ran to its end.
    fn one_result(batch: u64, result: Value) -> [FromAgent; 2] {
        [
            FromAgent::TaskResult {
                batch,
                index: 0,
                result: TaskResult(vars(result)),
                ran: None,
            },
            FromAgent::BatchDone {
                batch,
                outcome: BatchOutcome::Completed,
            },
        ]
    }

    /// Every `RunBatch` the fake was sent, as the tasks each one carried.
    fn batches_sent(agent: &FakeAgent) -> Vec<&[Task]> {
        agent
            .sent
            .iter()
            .filter_map(|m| match m {
                ToAgent::RunBatch { tasks, .. } => Some(tasks.as_slice()),
                _ => None,
            })
            .collect()
    }

    /// One item of a task backed by the plugin `kind`, run the way the driver runs it, against
    /// the scripted `agent`: its arguments as `prepare` rendered them, the variables of the host
    /// the module runs on, the interpreters that host's agent reported. The item's result, or the
    /// outcome that ended the host's batch, and the warnings the plugin raised.
    async fn plugin_item(
        kind: crate::action_plugins::Kind,
        args: Value,
        running: Value,
        agent: &mut FakeAgent,
        interpreters: &[String],
        stop: &mut watch::Receiver<bool>,
    ) -> (
        Result<TaskResult, Result<BatchOutcome, String>>,
        Vec<String>,
    ) {
        plugin_item_on(
            kind,
            args,
            running,
            agent,
            &mut NoRelink,
            false,
            None,
            interpreters,
            stop,
        )
        .await
    }

    /// [`plugin_item`], with a way to reconnect and a choice of whether the link is local.
    #[expect(
        clippy::too_many_arguments,
        reason = "plugin_item's, plus the two it fixes"
    )]
    async fn plugin_item_on<R: Relink<FakeAgent>>(
        kind: crate::action_plugins::Kind,
        args: Value,
        running: Value,
        agent: &mut FakeAgent,
        relink: &mut R,
        local: bool,
        timeout: Option<u64>,
        interpreters: &[String],
        stop: &mut watch::Receiver<bool>,
    ) -> (
        Result<TaskResult, Result<BatchOutcome, String>>,
        Vec<String>,
    ) {
        let t = task(match kind {
            crate::action_plugins::Kind::Copy => "copy",
            crate::action_plugins::Kind::Dnf => "dnf",
            crate::action_plugins::Kind::Fetch => "fetch",
            crate::action_plugins::Kind::Package => "package",
            crate::action_plugins::Kind::Reboot => "reboot",
            crate::action_plugins::Kind::Service => "service",
            crate::action_plugins::Kind::Template => "template",
            crate::action_plugins::Kind::Unarchive => "unarchive",
        });
        let t = PlayTask { timeout, ..t };
        let item = Item {
            args: vars(args),
            ..bare_item()
        };
        let running = vars(running);
        let templar = Templar::new(PathBuf::from("."));
        let origin = crate::compile::Origin::default();
        let union = union_of("ab", crate::action_plugins::modules_for(kind));
        let mut warnings = Vec::new();
        let mut stop_broken = false;
        let mut logs = Vec::new();
        let mut batch_id = 0;
        let ran = {
            let mut plugin = crate::action_plugins::start(
                kind,
                crate::action_plugins::Context {
                    args: &item.args,
                    args_untrusted: &item.args_untrusted,
                    running_vars: &running,
                    delegated: false,
                    escalated: false,
                    local,
                    item_vars: &item.vars,
                    templar: &templar,
                    origin: &origin,
                    playbook_dir: Path::new("."),
                    warnings: &mut warnings,
                },
            );
            run_plugin_item(
                agent,
                relink,
                "h1",
                &mut batch_id,
                plugin.as_mut(),
                &t,
                &item,
                Some(&union),
                interpreters,
                None,
                stop,
                &mut stop_broken,
                &mut logs,
            )
            .await
        };
        (ran, warnings)
    }

    fn python3() -> Vec<String> {
        vec!["/usr/bin/python3".to_string()]
    }

    /// A `package` task reaches the host as the module its package manager names, and nothing
    /// else: with the facts gathered it asks nothing first; without them it asks `setup` for
    /// `ansible_pkg_mgr` alone, then runs that module.
    ///
    /// What would make this red: `package` still refused, or sent as the `package` module itself -
    /// which the reference never runs, since its plugin is where the choice lives - or the filtered
    /// `setup` result written into the host's facts, which the reference does not keep: measured
    /// on ansible-core 2.19.12, it runs that `setup` again at every such task.
    #[tokio::test]
    async fn a_package_task_runs_the_module_its_manager_names() {
        use crate::action_plugins::Kind;
        let mut stop = watch::channel(false).1;
        // Measured on ansible-core 2.19.12 with facts gathered: `Running ansible.legacy.apt` and no
        // `AnsiballZ_setup.py`.
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(1, json!({"cache_updated": false, "changed": false})).to_vec(),
            ]
            .concat(),
        );
        let (ran, _) = plugin_item(
            Kind::Package,
            json!({"name": "bash", "use": "auto"}),
            json!({"ansible_facts": {"pkg_mgr": "apt"}}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        let sent = batches_sent(&agent);
        assert_eq!(sent.len(), 1, "one frame and no setup: {:?}", agent.sent);
        assert_eq!(sent[0].len(), 1, "{:?}", sent[0]);
        assert_eq!(sent[0][0].module, "apt");
        assert_eq!(
            Value::Object(sent[0][0].args.clone()),
            json!({"name": "bash"}),
            "`use` is the plugin's and never reaches the module"
        );
        assert_eq!(
            sent[0][0].payload.as_ref().map(|p| p.module_fqn.as_str()),
            Some("ansible.modules.apt"),
            "the payload is the union's entry for the module the plugin chose"
        );
        assert!(!ran.expect("the item finished").failed());

        // Measured without facts: `AnsiballZ_setup.py`, then `Running ansible.legacy.apt`.
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(
                    1,
                    json!({"ansible_facts": {"ansible_pkg_mgr": "apt"}, "changed": false}),
                )
                .to_vec(),
                one_result(2, json!({"cache_updated": false, "changed": false})).to_vec(),
            ]
            .concat(),
        );
        let (ran, _) = plugin_item(
            Kind::Package,
            json!({"name": "bash"}),
            json!({}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        let sent = batches_sent(&agent);
        let modules: Vec<&str> = sent.iter().map(|b| b[0].module.as_str()).collect();
        assert_eq!(modules, ["setup", "apt"], "{:?}", agent.sent);
        assert_eq!(
            Value::Object(sent[0][0].args.clone()),
            json!({"filter": ["ansible_pkg_mgr"], "gather_subset": ["!all"]})
        );
        let result = ran.expect("the item finished");

        // The result goes where every remote result goes, and the `setup` it took to choose the
        // module leaves nothing there.
        let mut store = one_host_store();
        record_facts(&mut store, &["h1".to_string()], &[(None, result.clone())]);
        let host = store.for_host("h1", &crate::vars::Scope::default());
        assert!(!host.contains_key("ansible_pkg_mgr"), "{host:?}");
        assert!(
            host.get("ansible_facts")
                .and_then(|f| f.get("pkg_mgr"))
                .is_none(),
            "{host:?}"
        );
        // Measured: the task registers the module's result as it is.
        assert_eq!(
            Value::Object(result.0),
            json!({"cache_updated": false, "changed": false})
        );
    }

    fn msg(result: &TaskResult) -> &str {
        result.0.get("msg").and_then(Value::as_str).unwrap_or("")
    }

    /// The order the package manager is picked in, and the reference's sentences when it
    /// cannot be, measured on ansible-core 2.19.12 (`use: nosuchmgr` fails with the first sentence)
    /// and read off its `plugins/action/package.py`.
    ///
    /// What would make this red: a name no module of the union carries sent anyway - a
    /// collection's manager, or a value a host put in its facts - or `ansible_package_use`
    /// passed over for the facts, or a `setup` failure or a missing fact reported in words of
    /// this engine's own, which a playbook testing the reference's `msg` would not recognise.
    #[tokio::test]
    async fn the_package_manager_is_picked_in_the_reference_s_order_and_words() {
        use crate::action_plugins::Kind;
        let mut stop = watch::channel(false).1;
        for (args, running, name) in [
            (
                json!({"name": "bash", "use": "nosuchmgr"}),
                json!({}),
                "nosuchmgr",
            ),
            (
                json!({"name": "bash"}),
                json!({"ansible_facts": {"pkg_mgr": "pacman"}}),
                "pacman",
            ),
        ] {
            let mut agent = FakeAgent::answering(Vec::new());
            let (ran, _) = plugin_item(
                Kind::Package,
                args,
                running,
                &mut agent,
                &python3(),
                &mut stop,
            )
            .await;
            let result = ran.expect("the item finished");
            assert!(result.failed(), "{result:?}");
            assert_eq!(
                msg(&result),
                format!("Could not find a matching action for the \"{name}\" package manager.")
            );
            assert!(agent.sent.is_empty(), "nothing is sent: {:?}", agent.sent);
        }

        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(1, json!({"changed": false})).to_vec(),
            ]
            .concat(),
        );
        let (ran, _) = plugin_item(
            Kind::Package,
            json!({"name": "bash"}),
            json!({"ansible_package_use": "dnf", "ansible_facts": {"pkg_mgr": "apt"}}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        assert!(!ran.expect("the item finished").failed());
        let sent = batches_sent(&agent);
        assert_eq!(sent.len(), 1, "{:?}", agent.sent);
        assert_eq!(sent[0][0].module, "dnf", "the variable beats the facts");

        let setup_failed = json!({
            "failed": true,
            "msg": "boom",
            "ansible_facts": {"ansible_pkg_mgr": "apt"},
        });
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(1, setup_failed).to_vec(),
            ]
            .concat(),
        );
        let (ran, _) = plugin_item(
            Kind::Package,
            json!({"name": "bash"}),
            json!({}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        let result = ran.expect("the item finished");
        assert!(result.failed(), "{result:?}");
        assert_eq!(
            msg(&result),
            "Failed to fetch ansible_pkg_mgr to determine the package action backend: boom"
        );
        assert!(
            !result.0.contains_key("ansible_facts"),
            "a filtered setup is never kept, failed or not: {result:?}"
        );
        assert_eq!(batches_sent(&agent).len(), 1, "{:?}", agent.sent);

        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(1, json!({"ansible_facts": {}, "changed": false})).to_vec(),
            ]
            .concat(),
        );
        let (ran, _) = plugin_item(
            Kind::Package,
            json!({"name": "bash"}),
            json!({}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        let result = ran.expect("the item finished");
        assert_eq!(
            msg(&result),
            "Could not detect a package manager. Try using the \"use\" option."
        );
        assert_eq!(batches_sent(&agent).len(), 1, "{:?}", agent.sent);
    }

    /// Without gathered facts and without `use_backend`, a `dnf` task asks `setup` for
    /// `ansible_pkg_mgr` alone, then runs `dnf` for `dnf`, `dnf4`, `yum` and `yum4` and `dnf5`
    /// for `dnf5`, and its result carries the answer as `ansible_facts.pkg_mgr`, which enters the
    /// host's facts as the host's own words. Measured on ansible-core 2.19.12 on a host that runs
    /// `apt`: the `setup` goes out filtered and `gather_subset: ["!all"]`; the rest is read off
    /// its `plugins/action/dnf.py`, since no host measured runs `dnf`.
    ///
    /// What would make this red: `dnf` still refused, or sent as a module of its own before the
    /// question is asked; `yum` or a `4` name sent as a module the union does not hold; the
    /// answer dropped from the result, where the reference keeps it - unlike `package`, whose
    /// `setup` leaves nothing; or the fact recorded as trusted, which lets a host's answer be
    /// rendered as a template.
    #[tokio::test]
    async fn a_dnf_task_asks_setup_then_runs_the_backend_it_names() {
        use crate::action_plugins::Kind;
        let mut stop = watch::channel(false).1;
        for (answer, module) in [
            ("dnf", "dnf"),
            ("dnf4", "dnf"),
            ("yum", "dnf"),
            ("yum4", "dnf"),
            ("dnf5", "dnf5"),
        ] {
            let mut agent = FakeAgent::answering(
                [
                    vec![state("ab", true)],
                    one_result(
                        1,
                        json!({"ansible_facts": {"ansible_pkg_mgr": answer}, "changed": false}),
                    )
                    .to_vec(),
                    one_result(2, json!({"changed": false, "results": []})).to_vec(),
                ]
                .concat(),
            );
            let (ran, _) = plugin_item(
                Kind::Dnf,
                json!({"name": "bash"}),
                json!({}),
                &mut agent,
                &python3(),
                &mut stop,
            )
            .await;
            let sent = batches_sent(&agent);
            let modules: Vec<&str> = sent.iter().map(|b| b[0].module.as_str()).collect();
            assert_eq!(modules, ["setup", module], "{answer}: {:?}", agent.sent);
            assert_eq!(
                Value::Object(sent[0][0].args.clone()),
                json!({"filter": ["ansible_pkg_mgr"], "gather_subset": ["!all"]})
            );
            assert_eq!(
                Value::Object(sent[1][0].args.clone()),
                json!({"name": "bash"})
            );
            assert_eq!(
                sent[1][0].payload.as_ref().map(|p| p.module_fqn.as_str()),
                Some(format!("ansible.modules.{module}").as_str())
            );
            let result = ran.expect("the item finished");
            assert_eq!(
                Value::Object(result.0.clone()),
                json!({"ansible_facts": {"pkg_mgr": answer}, "changed": false, "results": []}),
                "{answer}"
            );
            let mut store = one_host_store();
            record_facts(&mut store, &["h1".to_string()], &[(None, result)]);
            let scope = crate::vars::Scope::default();
            assert_eq!(
                store.for_host("h1", &scope)["ansible_facts"]["pkg_mgr"],
                json!(answer)
            );
            assert!(store.untrusted_of("h1", &scope).contains("ansible_facts"));
        }

        // An empty `use_backend`, as `"{{ backend | default('') }}"` renders, asks the host
        // rather than the facts, and installs with what it answers.
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(
                    1,
                    json!({"ansible_facts": {"ansible_pkg_mgr": "dnf"}, "changed": false}),
                )
                .to_vec(),
                one_result(2, json!({"changed": true})).to_vec(),
            ]
            .concat(),
        );
        let (ran, _) = plugin_item(
            Kind::Dnf,
            json!({"name": "bash", "use_backend": ""}),
            json!({"ansible_facts": {"pkg_mgr": "apt"}}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        let modules: Vec<&str> = batches_sent(&agent)
            .iter()
            .map(|b| b[0].module.as_str())
            .collect();
        assert_eq!(modules, ["setup", "dnf"], "{:?}", agent.sent);
        assert_eq!(
            Value::Object(batches_sent(&agent)[1][0].args.clone()),
            json!({"name": "bash"})
        );
        assert_eq!(
            Value::Object(ran.expect("the item finished").0),
            json!({"ansible_facts": {"pkg_mgr": "dnf"}, "changed": true})
        );

        // Gathered facts that name a backend ask nothing, and the result carries nothing more
        // than the module's; `use_backend` wins over the facts and never reaches the module
        // (measured: `Running ansible.legacy.dnf5 as the backend for the dnf action plugin`).
        for (args, running, module) in [
            (
                json!({"name": "bash"}),
                json!({"ansible_facts": {"pkg_mgr": "dnf5"}}),
                "dnf5",
            ),
            (
                json!({"name": "bash", "use_backend": "dnf5"}),
                json!({"ansible_facts": {"pkg_mgr": "apt"}}),
                "dnf5",
            ),
            (json!({"name": "bash", "use": "dnf4"}), json!({}), "dnf"),
        ] {
            let mut agent = FakeAgent::answering(
                [
                    vec![state("ab", true)],
                    one_result(1, json!({"changed": false})).to_vec(),
                ]
                .concat(),
            );
            let (ran, _) =
                plugin_item(Kind::Dnf, args, running, &mut agent, &python3(), &mut stop).await;
            let sent = batches_sent(&agent);
            assert_eq!(sent.len(), 1, "{:?}", agent.sent);
            assert_eq!(sent[0][0].module, module);
            assert_eq!(
                Value::Object(sent[0][0].args.clone()),
                json!({"name": "bash"})
            );
            assert_eq!(
                Value::Object(ran.expect("the item finished").0),
                json!({"changed": false})
            );
        }
    }

    /// A host whose `setup` names no `dnf` fails the task with the reference's two sentences, and
    /// the `pkg_mgr` its result carries stays out of the host's facts. Measured on ansible-core
    /// 2.19.12 on a host that runs `apt`: `AnsiballZ_setup.py` answers `apt`, the task fails with
    /// a `msg` that is a list of two sentences - the stray `}` is in the reference's source - and
    /// `ansible_facts.pkg_mgr` is undefined at the next task.
    ///
    /// What would make this red: a sentence of this engine's own, or one string where the
    /// reference has two; `apt` sent after all; the failed result's facts recorded, which the
    /// reference never does for a failed task; `use` and `use_backend` both taken; a
    /// `use_backend` naming no backend failed without asking the host; or a failed `setup`
    /// reported as the two sentences, which drops its cause.
    #[tokio::test]
    async fn a_dnf_task_on_a_host_without_dnf_fails_in_the_reference_s_words() {
        use crate::action_plugins::Kind;
        let undetected = json!([
            "Could not detect which major revision of dnf is in use, which is required to determine module backend.",
            "You should manually specify use_backend to tell the module whether to use the dnf4 or dnf5 backend})",
        ]);
        let mut stop = watch::channel(false).1;
        // A `use_backend` naming no backend asks the host too, as the reference does: its test
        // of `VALID_BACKENDS` that runs the `setup` is not under the `auto`/`yum` one.
        for args in [
            json!({"name": "bash"}),
            json!({"name": "bash", "use_backend": "apt"}),
        ] {
            let mut agent = FakeAgent::answering(
                [
                    vec![state("ab", true)],
                    one_result(
                        1,
                        json!({"ansible_facts": {"ansible_pkg_mgr": "apt"}, "changed": false}),
                    )
                    .to_vec(),
                ]
                .concat(),
            );
            let (ran, _) = plugin_item(
                Kind::Dnf,
                args.clone(),
                json!({}),
                &mut agent,
                &python3(),
                &mut stop,
            )
            .await;
            let result = ran.expect("the item finished");
            assert!(result.failed(), "{result:?}");
            assert_eq!(result.0["msg"], undetected);
            let modules: Vec<&str> = batches_sent(&agent)
                .iter()
                .map(|b| b[0].module.as_str())
                .collect();
            assert_eq!(modules, ["setup"], "{args}: {:?}", agent.sent);
            // The reference puts the answer in the result before it fails, so it is
            // registered...
            assert_eq!(result.0["ansible_facts"], json!({"pkg_mgr": "apt"}));
            // ...and not kept as a fact, because the task failed.
            let mut store = one_host_store();
            record_facts(&mut store, &["h1".to_string()], &[(None, result)]);
            let host = store.for_host("h1", &crate::vars::Scope::default());
            assert!(
                host.get("ansible_facts")
                    .and_then(|f| f.get("pkg_mgr"))
                    .is_none(),
                "{host:?}"
            );
        }

        let mut agent = FakeAgent::answering(Vec::new());
        let (ran, _) = plugin_item(
            Kind::Dnf,
            json!({"name": "bash", "use": "dnf", "use_backend": "dnf5"}),
            json!({}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        let result = ran.expect("the item finished");
        assert!(result.failed(), "{result:?}");
        assert_eq!(
            result.0["msg"],
            json!("parameters are mutually exclusive: ('use', 'use_backend')")
        );
        assert!(agent.sent.is_empty(), "nothing is sent: {:?}", agent.sent);

        // A `setup` that failed fails the task with its cause under the reference's sentence,
        // read off `plugins/action/dnf.py`, and without the facts it carried.
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(
                    1,
                    json!({"failed": true, "msg": "boom", "ansible_facts": {"ansible_pkg_mgr": "dnf"}}),
                )
                .to_vec(),
            ]
            .concat(),
        );
        let (ran, _) = plugin_item(
            Kind::Dnf,
            json!({"name": "bash"}),
            json!({}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        let result = ran.expect("the item finished");
        assert!(result.failed(), "{result:?}");
        assert_eq!(
            result.0["msg"],
            json!("Failed to fetch ansible_pkg_mgr to determine the dnf action backend: boom")
        );
        assert!(!result.0.contains_key("ansible_facts"), "{result:?}");
        assert_eq!(batches_sent(&agent).len(), 1, "{:?}", agent.sent);
    }

    /// `service` runs the init system the host names, drops what `systemd` does not take with the
    /// reference's warning, and falls back to `service` for a name no module carries: each measured
    /// on ansible-core 2.19.12.
    ///
    /// What would make this red: `sleep` sent to `systemd`, the warning lost or worded otherwise,
    /// `use:` read without lowering it, or an unknown name failing where the reference runs
    /// `service`.
    #[tokio::test]
    async fn a_service_task_runs_the_init_system_the_host_names() {
        use crate::action_plugins::Kind;
        let mut stop = watch::channel(false).1;
        // Measured without facts and with `sleep:` given: `AnsiballZ_setup.py`, then `Running
        // ansible.legacy.systemd` and the warning.
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(
                    1,
                    json!({"ansible_facts": {"ansible_service_mgr": "systemd"}, "changed": false}),
                )
                .to_vec(),
                one_result(2, json!({"changed": false})).to_vec(),
            ]
            .concat(),
        );
        let (ran, warnings) = plugin_item(
            Kind::Service,
            json!({"name": "cron", "state": "started", "sleep": 2, "use": "auto"}),
            json!({}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        assert!(!ran.expect("the item finished").failed());
        let sent = batches_sent(&agent);
        let modules: Vec<&str> = sent.iter().map(|b| b[0].module.as_str()).collect();
        assert_eq!(modules, ["setup", "systemd"], "{:?}", agent.sent);
        assert_eq!(
            Value::Object(sent[0][0].args.clone()),
            json!({"filter": ["ansible_service_mgr"], "gather_subset": ["!all"]})
        );
        assert_eq!(
            Value::Object(sent[1][0].args.clone()),
            json!({"name": "cron", "state": "started"})
        );
        assert_eq!(
            warnings,
            ["Ignoring \"sleep\" as it is not used in \"systemd\""]
        );

        // An unknown name runs `service`, as measured; a `use:` in capitals is lowered, as the
        // reference's plugin does. No `setup` either way.
        for (asked, module) in [("nosuchmgr", "service"), ("SystemD", "systemd")] {
            let mut agent = FakeAgent::answering(
                [
                    vec![state("ab", true)],
                    one_result(1, json!({"changed": false})).to_vec(),
                ]
                .concat(),
            );
            let (ran, _) = plugin_item(
                Kind::Service,
                json!({"name": "cron", "use": asked}),
                json!({}),
                &mut agent,
                &python3(),
                &mut stop,
            )
            .await;
            assert!(!ran.expect("the item finished").failed());
            let sent = batches_sent(&agent);
            assert_eq!(sent.len(), 1, "{:?}", agent.sent);
            assert_eq!(sent[0][0].module, module);
            assert_eq!(
                Value::Object(sent[0][0].args.clone()),
                json!({"name": "cron"})
            );
        }
    }

    /// What ends a plugin's item other than its own result: the link lost between the `setup` and
    /// the module, an interruption during the `setup`, a host with no interpreter, a result the
    /// agent never sent.
    ///
    /// What would make this red: a lost link or an interruption turned into a result, which the
    /// driver would record and report for a task that never ran its module; the module sent after
    /// all; or a batch that ended with no result read as a finished item, which reports `ok` for
    /// a sub-task nobody saw end.
    #[tokio::test]
    async fn a_plugin_item_ends_the_way_its_link_does() {
        use crate::action_plugins::Kind;
        let setup = || {
            one_result(
                1,
                json!({"ansible_facts": {"ansible_pkg_mgr": "apt"}, "changed": false}),
            )
            .to_vec()
        };
        let mut stop = watch::channel(false).1;

        // The agent goes away once the `setup` is in.
        let mut agent = FakeAgent::answering([vec![state("ab", true)], setup()].concat());
        let (ran, _) = plugin_item(
            Kind::Package,
            json!({"name": "bash"}),
            json!({}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        assert!(matches!(ran, Err(Err(_))), "{ran:?}");
        assert_eq!(batches_sent(&agent).len(), 2, "{:?}", agent.sent);

        // Interrupted while the `setup` runs: the agent is asked to stop it, and nothing follows.
        let (stop_tx, mut stopping) = watch::channel(false);
        let mut agent = FakeAgent::answering(vec![state("ab", true)]);
        agent.hangs_when_empty = true;
        stop_tx.send(true).expect("the receiver is alive");
        let (ran, _) = plugin_item(
            Kind::Package,
            json!({"name": "bash"}),
            json!({}),
            &mut agent,
            &python3(),
            &mut stopping,
        )
        .await;
        assert!(
            matches!(ran, Err(Ok(BatchOutcome::Cancelled { .. }))),
            "{ran:?}"
        );
        let modules: Vec<&str> = batches_sent(&agent)
            .iter()
            .map(|b| b[0].module.as_str())
            .collect();
        assert_eq!(modules, ["setup"]);
        assert!(
            agent
                .sent
                .iter()
                .any(|m| matches!(m, ToAgent::Cancel { .. })),
            "{:?}",
            agent.sent
        );

        // No interpreter: the item fails, per task and rescuable, and nothing is sent.
        let mut agent = FakeAgent::answering(Vec::new());
        let (ran, _) = plugin_item(
            Kind::Package,
            json!({"name": "bash"}),
            json!({"ansible_facts": {"pkg_mgr": "apt"}}),
            &mut agent,
            &[],
            &mut stop,
        )
        .await;
        let result = ran.expect("a task failure, not the host's end");
        assert!(msg(&result).contains("no python interpreter"), "{result:?}");
        assert!(agent.sent.is_empty(), "{:?}", agent.sent);

        // The batch ended cleanly with no result in it.
        let mut agent = FakeAgent::answering(vec![
            state("ab", true),
            FromAgent::BatchDone {
                batch: 1,
                outcome: BatchOutcome::Completed,
            },
        ]);
        let (ran, _) = plugin_item(
            Kind::Package,
            json!({"name": "bash"}),
            json!({}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        let result = ran.expect("the item reports");
        assert!(result.failed(), "{result:?}");
        assert!(msg(&result).contains("'setup'"), "{result:?}");
    }

    /// A module that failed ends its item with that failure and not the task's batch, so the
    /// items of a loop behind it still run and `report_task` judges the task from all of them.
    ///
    /// What would make this red: `BatchOutcome::Failed` read as the end of the batch, which stops
    /// a three-item loop at its second item where the reference runs all three.
    #[tokio::test]
    async fn a_failed_module_is_its_item_s_result_and_the_loop_goes_on() {
        use crate::action_plugins::Kind;
        let mut stop = watch::channel(false).1;
        let mut agent = FakeAgent::answering(vec![
            state("ab", true),
            FromAgent::TaskResult {
                batch: 1,
                index: 0,
                result: TaskResult(vars(json!({"failed": true, "msg": "No package matching"}))),
                ran: None,
            },
            FromAgent::BatchDone {
                batch: 1,
                outcome: BatchOutcome::Failed { at: 0 },
            },
        ]);
        let (ran, _) = plugin_item(
            Kind::Package,
            json!({"name": "nosuchpackage"}),
            json!({"ansible_facts": {"pkg_mgr": "apt"}}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        let result = ran.expect("the item finished");
        assert!(result.failed(), "{result:?}");
        assert_eq!(msg(&result), "No package matching");
    }

    /// A sub-task that stages a file names it on the wire, under its argument and by its hash
    /// alone, and a module the union lacks is never sent as native.
    ///
    /// What would make this red: the sub-task sent without its file, which runs `copy` against a
    /// source that is not there - or, worse, against whatever the argument names on the host.
    #[test]
    fn a_sub_task_names_the_files_it_stages() {
        let sub = crate::action_plugins::Sub {
            module: "copy",
            args: Map::new(),
            files: vec![(
                "src".to_string(),
                crate::action_plugins::FileBlob {
                    hash: "cd".into(),
                    b64: "eA==".into(),
                },
            )],
        };
        let union = union_of("ab", &["copy"]);
        let built = sub_task(
            &task("copy"),
            &bare_item(),
            &sub,
            Some(&union),
            &python3(),
            None,
        )
        .expect("a sub-task with files travels");
        assert_eq!(built.module, "copy");
        assert_eq!(
            built.files,
            [volant_protocol::StagedFile {
                arg: "src".into(),
                blob: "cd".into(),
            }]
        );
        assert!(!built.force_python);
        // A plugin's sub-task follows the run's switch like a task the playbook wrote.
        let mut off = union.clone();
        off.natives.enabled = false;
        let built = sub_task(
            &task("copy"),
            &bare_item(),
            &sub,
            Some(&off),
            &python3(),
            None,
        )
        .expect("a sub-task with files travels");
        assert!(built.force_python, "{built:?}");
        let failure = sub_task(&task("copy"), &bare_item(), &sub, None, &python3(), None)
            .expect_err("a module the union lacks is never sent as native");
        assert!(msg(&failure).contains("python payload"), "{failure:?}");
    }

    /// What the playbook asked for is read under the name the reference uses, off the map of
    /// the host the module will run on.
    ///
    /// What would make this red: another key, which silently ignores every
    /// `ansible_python_interpreter` a playbook sets and runs modules under the agent's first
    /// candidate instead.
    #[test]
    fn the_asked_for_interpreter_is_read_under_the_reference_name() {
        assert_eq!(requested_interpreter(&vars(json!({}))), None);
        assert_eq!(
            requested_interpreter(&vars(json!({"ansible_python_interpreter": "/opt/py"}))),
            Some("/opt/py".to_string())
        );
    }

    /// The interpreter a Python task runs under: the playbook's own choice when it made one,
    /// and otherwise the best of what the agent found on the host.
    ///
    /// What would make this red: the reported list preferred over the variable, which runs a
    /// module under an interpreter the playbook explicitly did not ask for.
    #[test]
    fn the_playbook_interpreter_wins_over_what_the_agent_found() {
        let found = [
            "/usr/bin/python3.12".to_string(),
            "/usr/bin/python3".to_string(),
        ];
        assert_eq!(
            chosen_interpreter(None, &found).expect("the best of the two"),
            "/usr/bin/python3.12"
        );
        assert_eq!(
            chosen_interpreter(Some("/usr/bin/python3"), &found)
                .expect("what the playbook asked for"),
            "/usr/bin/python3"
        );
    }

    /// An interpreter the playbook named is used as given, found by the agent or not.
    ///
    /// What would make this red: the reported list read as an allow-list. It is the
    /// **auto-discovery** list - eight names looked for on `PATH` - so a real interpreter
    /// somewhere it never looks, `/opt/python3.12/bin/python`, is not in it and would be refused
    /// although the reference runs it happily. The measured rc 127 and
    /// `The module interpreter ... was not found.` belong to an interpreter that is not there at
    /// all, which is a thing only the host can find out, by trying to start it.
    #[test]
    fn an_explicit_interpreter_is_used_as_given() {
        assert_eq!(
            chosen_interpreter(
                Some("/opt/python3.12/bin/python"),
                &["/usr/bin/python3".to_string()]
            )
            .expect("the playbook named it, so it travels"),
            "/opt/python3.12/bin/python"
        );
    }

    /// A host whose agent reported no interpreter fails its Python tasks saying that the agent
    /// reported none.
    ///
    /// What would make this red: a sentence claiming the host has no Python. An agent older than
    /// the interpreter list reports none exactly as a host with none does - the controller
    /// uploads the agent it shipped with, so the two are the same bytes on the wire - and an
    /// operator sent to look at the host's Python would find it there and be none the wiser.
    #[test]
    fn a_host_whose_agent_reported_no_interpreter_says_so_that_way() {
        let failure = chosen_interpreter(None, &[]).expect_err("nothing to run a module under");
        let msg = failure.0.get("msg").and_then(Value::as_str).unwrap_or("");
        assert!(
            msg.contains("the agent on this host reported no python"),
            "{msg}"
        );
        assert!(failure.failed(), "{failure:?}");
    }

    /// The two halves of a payload meet here or the step does not travel.
    ///
    /// What would make this red: a step sent with a payload and no interpreter chosen, which
    /// asks the agent to run a module under nothing, or a step whose interpreter could not be
    /// chosen sent anyway - both of which report the agent's confusion instead of the reason.
    #[test]
    fn a_step_travels_with_both_halves_of_its_payload_or_not_at_all() {
        let module = module_payload("ab");
        let chosen = Ok("/usr/bin/python3".to_string());
        let (payload, interpreter) = python_for("ping", Some(&module), Some(&chosen))
            .expect("both halves are in hand")
            .expect("a payload to send");
        assert_eq!(payload.blob, "ab");
        assert_eq!(interpreter, "/usr/bin/python3");

        assert!(
            python_for("command", None, None)
                .expect("a native step")
                .is_none(),
            "a step with no payload travels as it always did"
        );

        let refused: Result<String, TaskResult> = Err(TaskResult::failed_with(
            "The module interpreter was not found.",
        ));
        let failure = python_for("ping", Some(&module), Some(&refused))
            .expect_err("the interpreter could not be chosen");
        assert!(failure.failed(), "{failure:?}");

        let bug =
            python_for("ping", Some(&module), None).expect_err("a payload with no choice made");
        let msg = bug.0.get("msg").and_then(Value::as_str).unwrap_or("");
        assert!(msg.contains("ab"), "{msg}");
    }

    /// A payload that could not be stored fails every task of the batch and leaves the host
    /// where it was.
    ///
    /// Driven through `run_agent_batch` itself, because the shape of the answer is the whole
    /// finding: what would make this red is the storing error handed back as a batch error, which
    /// the driver reads as the host being unreachable - no task line at all, no `rescue:`, no
    /// `ignore_errors`, and a recap counting `unreachable` for a host whose native tasks would
    /// have run perfectly well. The batch must also not go out: a host that could not be given
    /// the payload has nothing to run.
    #[tokio::test]
    async fn a_payload_that_could_not_be_stored_fails_the_tasks_and_not_the_host() {
        let (_stop_tx, mut stop) = watch::channel(false);
        let mut stop_broken = false;
        let mut logs = Vec::new();
        let mut agent = FakeAgent::answering(vec![state("ab", false), state("ab", false)]);
        let tasks = vec![
            protocol_task(
                &task("ping"),
                &bare_item(),
                Some((&module_payload("ab"), "/usr/bin/python3")),
            ),
            protocol_task(&task("command"), &bare_item(), None),
        ];
        let (received, ended) = run_agent_batch(
            &mut agent,
            "h1",
            1,
            tasks,
            Some(&union_named("ab")),
            &[],
            &mut stop,
            &mut stop_broken,
            &mut logs,
        )
        .await;
        assert_eq!(received.len(), 2);
        for slot in &received {
            let result = slot.as_ref().expect("every task of the batch reports");
            assert!(result.failed(), "{result:?}");
            let msg = result.0.get("msg").and_then(Value::as_str).unwrap_or("");
            assert!(msg.contains("ab"), "{msg}");
        }
        assert!(
            matches!(ended, Ok(BatchOutcome::Failed { .. })),
            "the host stays reachable: {ended:?}"
        );
        assert!(
            !agent
                .sent
                .iter()
                .any(|msg| matches!(msg, ToAgent::RunBatch { .. })),
            "a batch went out for a payload the host has not got: {:?}",
            agent.sent
        );
    }

    /// A frame belonging to a batch, read while a blob question is outstanding, stops the
    /// exchange rather than being skipped.
    ///
    /// What would make this red: skipping it, which reads the next frame as the answer to this
    /// question - here a `present: true` that belongs to nothing - and runs a batch against a
    /// host that holds no payload.
    #[tokio::test]
    async fn a_batch_frame_during_a_blob_question_is_the_stream_out_of_step() {
        let mut logs = Vec::new();
        let mut agent = FakeAgent::answering(vec![
            FromAgent::BatchDone {
                batch: 7,
                outcome: BatchOutcome::Completed,
            },
            state("ab", true),
        ]);
        let err = ensure_blob(&mut agent, "h1", "ab", "UEsD", &mut logs)
            .await
            .expect_err("the streams are out of step");
        assert!(err.contains('7'), "{err}");
    }

    fn union_named(hash: &str) -> Union {
        Union {
            hash: hash.to_string(),
            zip_b64: "UEsDBA==".to_string(),
            modules: BTreeMap::new(),
            refused: BTreeMap::new(),
            natives: crate::python::Natives {
                enabled: true,
                facts: crate::python::Facts::Auto,
            },
        }
    }

    /// A batch holding one Python task among native ones gets the blob before any of it goes
    /// out, and gets it once.
    ///
    /// What would make this red: the pre-flight asking only when every task of the batch carries
    /// a payload - a batch is a run of tasks with the same connection, not the same module, so
    /// one Python task among natives is the ordinary case and the one that would go out with
    /// nothing on the host.
    #[tokio::test]
    async fn a_batch_carrying_a_payload_gets_the_blob_before_it_goes_out() {
        let mut logs = Vec::new();
        let mut agent = FakeAgent::answering(vec![state("ab", false), state("ab", true)]);
        let tasks = vec![
            protocol_task(&task("command"), &bare_item(), None),
            protocol_task(
                &task("ping"),
                &bare_item(),
                Some((&module_payload("ab"), "/usr/bin/python3")),
            ),
        ];
        blob_preflight(
            &mut agent,
            "h1",
            &tasks,
            Some(&union_named("ab")),
            &[],
            &mut logs,
        )
        .await
        .expect("the blob went first");
        assert_eq!(agent.puts(), 1, "{:?}", agent.sent);
    }

    /// A batch naming a payload the link never confirmed does not go out at all.
    ///
    /// What would make this red: the confirmation dropped from the pre-flight, which sends the
    /// batch and lets the agent fail on a payload it was never given - a failure the operator
    /// reads as the module's own.
    #[tokio::test]
    async fn a_batch_whose_payload_the_link_never_confirmed_never_goes_out() {
        let mut logs = Vec::new();
        let mut agent = FakeAgent::answering(Vec::new());
        let tasks = vec![protocol_task(
            &task("ping"),
            &bare_item(),
            Some((&module_payload("ab"), "/usr/bin/python3")),
        )];
        let err = blob_preflight(&mut agent, "h1", &tasks, None, &[], &mut logs)
            .await
            .expect_err("no link confirmed this payload");
        assert!(err.contains("ab"), "{err}");
        assert!(agent.sent.is_empty(), "{:?}", agent.sent);
    }

    /// A batch of native tasks asks the agent nothing about blobs, whatever the run has built.
    #[tokio::test]
    async fn a_batch_without_a_payload_asks_the_agent_nothing() {
        let mut logs = Vec::new();
        let mut agent = FakeAgent::answering(Vec::new());
        let tasks = vec![protocol_task(&task("command"), &bare_item(), None)];
        blob_preflight(
            &mut agent,
            "h1",
            &tasks,
            Some(&union_named("ab")),
            &[],
            &mut logs,
        )
        .await
        .expect("nothing to ask for");
        assert!(agent.sent.is_empty(), "{:?}", agent.sent);
    }

    /// A task whose payload names a blob this link never confirmed is a controller bug, and it
    /// fails naming the hash rather than being sent for the host to fail on.
    ///
    /// What would make this red: the batch sent anyway, which asks the agent to run a module
    /// out of a payload it was never given - a failure that reads as the module's.
    #[test]
    fn a_payload_naming_an_unsent_blob_fails_before_the_batch() {
        let mut memory = BlobMemory::default();
        let tasks = vec![protocol_task(
            &task("ping"),
            &bare_item(),
            Some((&module_payload("ab"), "/usr/bin/python3")),
        )];
        let err = payloads_confirmed(&tasks, &memory).expect_err("nothing was confirmed");
        assert!(err.contains("ab"), "{err}");
        memory.remember("ab", Ok(()));
        payloads_confirmed(&tasks, &memory).expect("the link confirmed it");

        // A staged file counts only when it went up for this batch, and the check has no memory
        // to consult: the agent consumed the one it had for the task before.
        let mut staging = tasks;
        staging[0].files = vec![volant_protocol::StagedFile {
            arg: "src".into(),
            blob: "cd".into(),
        }];
        let err = files_staged(&staging, &BTreeSet::new())
            .expect_err("the file was not put on the host for this batch");
        assert!(err.contains("cd") && err.contains("'src'"), "{err}");
        files_staged(&staging, &["cd".to_string()].into()).expect("the file went up for it");
    }

    /// A batch refused for its payload puts none of its files on the host.
    ///
    /// What would make this red: the files staged before the payload is checked, which leaves a
    /// staged file, a rendered template's secret among them, on a host that never runs the
    /// module that would have consumed it.
    #[tokio::test]
    async fn a_batch_refused_for_its_payload_stages_nothing() {
        let blob = hello_blob();
        let mut staging = protocol_task(
            &task("copy"),
            &bare_item(),
            Some((&module_payload("zz"), "/usr/bin/python3")),
        );
        staging.files = vec![volant_protocol::StagedFile {
            arg: "src".into(),
            blob: blob.hash.clone(),
        }];
        let mut agent = FakeAgent::answering(vec![state(&blob.hash, true)]);
        let mut stop = watch::channel(false).1;
        let (results, ended) = run_agent_batch(
            &mut agent,
            "h1",
            1,
            vec![staging],
            None,
            &[("src".into(), blob)],
            &mut stop,
            &mut false,
            &mut Vec::new(),
        )
        .await;
        assert!(
            matches!(ended, Ok(BatchOutcome::Failed { .. })),
            "{ended:?}"
        );
        let failed = results[0].as_ref().expect("the task's failure");
        assert!(msg(failed).contains("never confirmed"), "{failed:?}");
        assert!(agent.sent.is_empty(), "{:?}", agent.sent);
    }

    /// A link lost while a file is staged says it was the file, not a module payload.
    ///
    /// What would make this red: the staging error left in the words of the payload exchange,
    /// which sends an operator looking for a module payload that was never involved.
    #[tokio::test]
    async fn a_link_lost_while_staging_names_the_file() {
        let blob = hello_blob();
        let mut agent = FakeAgent::answering(Vec::new());
        let err = stage_files(&mut agent, "h1", &[("src".into(), blob)], &mut Vec::new())
            .await
            .expect_err("the agent stopped");
        assert!(
            err.starts_with("staging the file for 'src': ") && !err.contains("module payload"),
            "{err}"
        );
    }

    /// A file the agent refuses to store fails the item with the agent's reason, and nothing the
    /// plugin would add to a module's result: the module never ran, as the reference's action
    /// fails on a transfer that raised, before any `diff` or `checksum` is put under a result.
    ///
    /// What would make this red: the storing error handed to the plugin as the `copy` sub-task's
    /// result, which `Copy::finish` dresses with `diff: []` and the local checksum.
    #[tokio::test]
    async fn a_file_the_agent_cannot_store_fails_the_item_undressed() {
        use crate::action_plugins::Kind;
        let blob = hello_blob();
        let mut stop = watch::channel(false).1;
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(1, json!({"stat": {"exists": false}})).to_vec(),
                vec![
                    FromAgent::Log {
                        level: volant_protocol::LogLevel::Error,
                        message: "no space left".into(),
                    },
                    state(&blob.hash, false),
                ],
            ]
            .concat(),
        );
        let (ran, _) = plugin_item(
            Kind::Copy,
            copy_args("/tmp/v/full"),
            json!({}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        let result = ran.expect("the item finished");
        assert!(result.failed(), "{result:?}");
        assert!(
            msg(&result).starts_with("the agent could not store the file for 'src'"),
            "{result:?}"
        );
        assert!(
            !result.0.contains_key("diff") && !result.0.contains_key("checksum"),
            "{result:?}"
        );
        assert_eq!(
            batches_sent(&agent).len(),
            1,
            "only the stat ran: {:?}",
            agent.sent
        );
    }

    /// A batch with no task in it is not sent: there is nothing for the agent to answer, and the
    /// batch ends the way an agent ends an empty one.
    ///
    /// What would make this red: a `RunBatch` with no tasks put on the wire, which a host whose
    /// agent reported no Python pays a round trip for, batch after batch.
    #[tokio::test]
    async fn an_empty_batch_is_not_sent() {
        let mut agent = FakeAgent::answering(Vec::new());
        let mut stop = watch::channel(false).1;
        let (results, ended) = run_agent_batch(
            &mut agent,
            "h1",
            1,
            Vec::new(),
            None,
            &[],
            &mut stop,
            &mut false,
            &mut Vec::new(),
        )
        .await;
        assert!(results.is_empty());
        assert!(matches!(ended, Ok(BatchOutcome::Completed)), "{ended:?}");
        assert!(agent.sent.is_empty(), "{:?}", agent.sent);
    }

    /// The file blob of `hello\n`, as the plugin stages it.
    fn hello_blob() -> crate::action_plugins::FileBlob {
        crate::action_plugins::files::blob_of("hello.txt", b"hello\n").expect("it fits")
    }

    /// A `hello\n` on the controller, copied to `dest`: the arguments `prepare` hands the plugin.
    fn copy_args(dest: &str) -> Value {
        let dir = std::env::temp_dir().join(format!("volant-run-copy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let src = dir.join("hello.txt");
        std::fs::write(&src, "hello\n").expect("the source");
        json!({"src": src.display().to_string(), "dest": dest})
    }

    /// Two items copying the same bytes each put them on the host, as a staged file and without
    /// asking first: the first `copy` consumed the blob it staged, and nothing the link or the
    /// agent's shared cache holds stands in for the second.
    ///
    /// What would make this red: the file blob kept in the link's memory, so the second item
    /// sends nothing and its `copy` runs against a `src` the agent no longer holds; or a
    /// `has_blob` asked first, which another link to the same host account can answer yes to
    /// for a file it is about to consume - either way the answers scripted for the staging land
    /// in the wrong exchange.
    #[tokio::test]
    async fn the_same_content_is_staged_for_every_item() {
        use crate::action_plugins::Kind;
        let blob = hello_blob();
        let mut stop = watch::channel(false).1;
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(1, json!({"stat": {"exists": false}})).to_vec(),
                vec![state(&blob.hash, true)],
                one_result(2, json!({"changed": true})).to_vec(),
                one_result(1, json!({"stat": {"exists": false}})).to_vec(),
                vec![state(&blob.hash, true)],
                one_result(2, json!({"changed": true})).to_vec(),
            ]
            .concat(),
        );
        for dest in ["/tmp/v/one", "/tmp/v/two"] {
            let (ran, _) = plugin_item(
                Kind::Copy,
                copy_args(dest),
                json!({}),
                &mut agent,
                &python3(),
                &mut stop,
            )
            .await;
            let result = ran.expect("the item finished");
            assert!(!result.failed(), "{dest}: {result:?}");
        }
        let staged = agent
            .sent
            .iter()
            .filter(
                |m| matches!(m, ToAgent::PutBlob { hash, staged: true, .. } if *hash == blob.hash),
            )
            .count();
        assert_eq!(staged, 2, "{:?}", agent.sent);
        assert!(
            !agent
                .sent
                .iter()
                .any(|m| matches!(m, ToAgent::HasBlob { hash } if *hash == blob.hash)),
            "a file is never asked for: {:?}",
            agent.sent
        );
        let copies: Vec<&Task> = batches_sent(&agent)
            .into_iter()
            .flatten()
            .filter(|t| t.module == "copy")
            .collect();
        assert_eq!(copies.len(), 2, "{:?}", agent.sent);
        for copy in copies {
            assert_eq!(
                copy.files,
                [volant_protocol::StagedFile {
                    arg: "src".into(),
                    blob: blob.hash.clone(),
                }]
            );
        }
    }

    /// A scripted agent that raises the run's stop as it hands over the `BatchDone` of batch
    /// `after`, which is the moment between two attempts of a retried plugin item.
    struct StopsAfter {
        agent: FakeAgent,
        stop: watch::Sender<bool>,
        after: u64,
    }

    impl AgentChannel for StopsAfter {
        fn memory(&mut self) -> &mut BlobMemory {
            self.agent.memory()
        }

        async fn ask(&mut self, msg: &ToAgent) -> std::io::Result<()> {
            self.agent.ask(msg).await
        }

        async fn answer(&mut self) -> std::io::Result<Option<FromAgent>> {
            let answer = self.agent.answer().await?;
            if let Some(FromAgent::BatchDone { batch, .. }) = &answer
                && *batch == self.after
            {
                let _ = self.stop.send(true);
            }
            Ok(answer)
        }

        async fn stop_batch(&mut self, id: u64, grace: Duration) -> bool {
            self.agent.stop_batch(id, grace).await
        }
    }

    /// One item of a `copy` task `t` run the way the driver runs it under `t`'s own `retries`,
    /// `until` and `delay`: the item's result - `None` when the stop ended it between two
    /// attempts - and the count each retry line would show.
    async fn copy_attempts<C: AgentChannel>(
        t: &PlayTask,
        args: Value,
        agent: &mut C,
        stop: &mut watch::Receiver<bool>,
    ) -> (
        Result<Option<TaskResult>, Result<BatchOutcome, String>>,
        Vec<u32>,
    ) {
        let kind = crate::action_plugins::Kind::Copy;
        let item = Item {
            args: vars(args),
            ..bare_item()
        };
        let templar = Templar::new(PathBuf::from("."));
        let retry = retry_plan(t, Some(&item), &templar).expect("the retry plan renders");
        let origin = crate::compile::Origin::default();
        let running = Map::new();
        let union = union_of("ab", crate::action_plugins::modules_for(kind));
        let start = PluginStart {
            kind,
            running_vars: &running,
            delegated: false,
            escalated: false,
            local: false,
            templar: &templar,
            origin: &origin,
            playbook_dir: Path::new("."),
        };
        let mut lefts = Vec::new();
        let ran = run_plugin_attempts(
            agent,
            &mut NoRelink,
            "h1",
            &mut 0,
            &start,
            t,
            &item,
            retry.as_ref(),
            Some(&union),
            &python3(),
            None,
            stop,
            &mut false,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut lefts,
        )
        .await;
        (ran, lefts)
    }

    fn retried_copy(retries: u64) -> PlayTask {
        let mut t = task("copy");
        t.retries = Some(json!(retries));
        t.delay = Some(json!(0));
        t
    }

    fn modules_sent(agent: &FakeAgent) -> Vec<&str> {
        batches_sent(agent)
            .iter()
            .map(|b| b[0].module.as_str())
            .collect()
    }

    /// `k3s_server`'s `Copy k3s.yaml to second file`: a `remote_src` copy with `retries: 3` and
    /// no `until`, which succeeds the first time. Measured on ansible-core 2.19.12, the same
    /// shape on `localhost`: `attempts=1`, and no retry line.
    ///
    /// What would make this red: the retry loop left out of the plugin path, which gives a
    /// result with no `attempts`; or a passing attempt retried, which sends the copy again.
    #[tokio::test]
    async fn a_retried_copy_that_succeeds_runs_once_and_says_so() {
        let mut stop = watch::channel(false).1;
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(1, json!({"changed": true})).to_vec(),
            ]
            .concat(),
        );
        let (ran, lefts) = copy_attempts(
            &retried_copy(3),
            json!({"src": "/etc/rancher/k3s/k3s.yaml", "dest": "/etc/rancher/k3s/k3s-copy.yaml", "mode": "0600", "remote_src": true}),
            &mut agent,
            &mut stop,
        )
        .await;
        let result = ran.expect("the item ran").expect("nothing stopped it");
        assert!(!result.failed(), "{result:?}");
        assert_eq!(result.0["attempts"], json!(1));
        assert_eq!(lefts, Vec::<u32>::new());
        assert_eq!(modules_sent(&agent), ["copy"]);
    }

    /// A `copy` into a directory that does not exist, under `retries: 2`: each attempt replays
    /// the whole sequence, `stat` then `copy`, and stages the source again, because the agent
    /// consumed the file the first attempt staged. Measured on ansible-core 2.19.12, the same
    /// task on `localhost`: `FAILED - RETRYING ... (2 retries left)`, then `(1 retries left)`,
    /// then `fatal` with `"attempts": 2` - after **three** runs, the first and two retries, as
    /// `task_executor.py` runs them.
    ///
    /// What would make this red: the plugin kept across attempts, which resumes after its last
    /// sub-task instead of starting again; the staged file reused from the first attempt, which
    /// sends a `copy` naming a blob the agent no longer holds; one run fewer than the reference;
    /// or the retry counts wrong.
    #[tokio::test]
    async fn a_retried_copy_replays_its_whole_sequence_and_stages_its_file_again() {
        let blob = hello_blob();
        let mut stop = watch::channel(false).1;
        let missing = json!({"failed": true, "msg": "Destination directory /tmp/volant-nosuch does not exist"});
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(1, json!({"stat": {"exists": false}})).to_vec(),
                vec![state(&blob.hash, true)],
                one_result(2, missing.clone()).to_vec(),
                one_result(3, json!({"stat": {"exists": false}})).to_vec(),
                vec![state(&blob.hash, true)],
                one_result(4, missing.clone()).to_vec(),
                one_result(5, json!({"stat": {"exists": false}})).to_vec(),
                vec![state(&blob.hash, true)],
                one_result(6, missing).to_vec(),
            ]
            .concat(),
        );
        let (ran, lefts) = copy_attempts(
            &retried_copy(2),
            copy_args("/tmp/volant-nosuch/hello.txt"),
            &mut agent,
            &mut stop,
        )
        .await;
        let result = ran.expect("the item ran").expect("nothing stopped it");
        assert!(result.failed(), "{result:?}");
        assert_eq!(result.0["attempts"], json!(2));
        assert_eq!(lefts, [2, 1], "`(2 retries left)` then `(1 retries left)`");
        assert_eq!(
            modules_sent(&agent),
            ["stat", "copy", "stat", "copy", "stat", "copy"]
        );
        let staged = agent
            .sent
            .iter()
            .filter(
                |m| matches!(m, ToAgent::PutBlob { hash, staged: true, .. } if *hash == blob.hash),
            )
            .count();
        assert_eq!(staged, 3, "{:?}", agent.sent);
    }

    /// The attempt rule on its own, for `retries: 2` and a run that keeps failing: a retry after
    /// runs 1 and 2 showing `(2 retries left)` and `(1 retries left)`, a third run, and then the
    /// failure reporting `attempts: 2`. Read from ansible-core 2.19.12 `task_executor.py`
    /// (`retries = 1 + R`, `attempts = retries - 1` on running out) and measured there with a
    /// `shell` appending a line: three lines, `"attempts": 2`.
    ///
    /// What would make this red: the loop ending after R runs, which is one run fewer than the
    /// reference; `attempts` reporting R + 1; or a retry line after the last run.
    #[test]
    fn retries_r_is_r_plus_one_runs_reporting_r_attempts() {
        let mut t = task("command");
        t.retries = Some(json!(2));
        t.delay = Some(json!(0));
        let item = bare_item();
        let templar = Templar::new(PathBuf::from("."));
        let retry = retry_plan(&t, Some(&item), &templar)
            .unwrap()
            .expect("a retry plan");
        let failing = || TaskResult(vars(json!({"failed": true, "rc": 1})));
        let mut lefts = Vec::new();
        let mut attempt = 0;
        let last = loop {
            attempt += 1;
            match judge_attempt(&t, &item, failing(), attempt, &retry, &templar) {
                Attempt::Done(r) => break r,
                Attempt::Again(left) => lefts.push(left),
            }
        };
        assert_eq!(attempt, 3, "three runs");
        assert_eq!(lefts, [2, 1]);
        assert_eq!(last.0["attempts"], json!(2));
        assert!(last.failed());

        let passing = TaskResult(vars(json!({"changed": false, "rc": 0})));
        let Attempt::Done(r) = judge_attempt(&t, &item, passing, 1, &retry, &templar) else {
            panic!("a passing run ends the loop");
        };
        assert_eq!(r.0["attempts"], json!(1));
    }

    /// `attempts` is on the result before `changed_when` and `failed_when` read it, and still on
    /// it when a condition could not be evaluated. ansible-core 2.19.12 `task_executor.py` sets
    /// it right after `failed` ("Make attempts and retries available early to allow their use in
    /// changed/failed_when"), then binds the registered name, then applies the conditions.
    ///
    /// What would make this red: `attempts` set only after the conditions, which leaves
    /// `r.attempts` undefined under `changed_when` and fails a task whose command passed on its
    /// second run; or the failure a broken condition reports losing the count.
    #[test]
    fn changed_when_reads_the_attempt_it_judges() {
        let mut t = task("command");
        t.register = Some("r".into());
        t.retries = Some(json!(3));
        t.delay = Some(json!(0));
        t.until = vec!["r.rc == 0".into()];
        t.changed_when = vec!["r.attempts > 1".into()];
        let item = bare_item();
        let templar = Templar::new(PathBuf::from("."));
        let retry = retry_plan(&t, Some(&item), &templar)
            .unwrap()
            .expect("a retry plan");
        let rc = |rc: i64| TaskResult(vars(json!({"rc": rc, "changed": true})));
        assert!(matches!(
            judge_attempt(&t, &item, rc(1), 1, &retry, &templar),
            Attempt::Again(3)
        ));
        let Attempt::Done(r) = judge_attempt(&t, &item, rc(0), 2, &retry, &templar) else {
            panic!("rc 0 ends the loop");
        };
        assert!(!r.failed(), "{r:?}");
        assert_eq!(r.0["changed"], json!(true), "{r:?}");
        assert_eq!(r.0["attempts"], json!(2));

        // An `until` the failure satisfies, so that the loop ends on the result the broken
        // condition handed back, and not on one an `until` reading a missing `rc` would raise.
        t.changed_when = vec!["r.nosuchkey".into()];
        t.until = vec!["r.failed".into()];
        let retry = retry_plan(&t, Some(&item), &templar)
            .unwrap()
            .expect("a retry plan");
        let Attempt::Done(r) = judge_attempt(&t, &item, rc(0), 1, &retry, &templar) else {
            panic!("the failure satisfies `until`");
        };
        assert!(r.failed(), "{r:?}");
        assert!(msg(&r).contains("undefined"), "{r:?}");
        assert_eq!(r.0["attempts"], json!(1), "{r:?}");
    }

    /// A run that hit the `timeout` keyword ends the task as it stands: no second run, no
    /// `attempts`, and `failed_when` never consulted. In ansible-core 2.19.12 the timeout is an
    /// exception raised out of the whole attempt loop (`_task_timeout.TaskTimeoutError`, a
    /// `BaseException`), so nothing after the handler's run executes.
    ///
    /// What would make this red: a timed-out run judged like any failure, which retries a task
    /// the reference gives up on at once, or lets `failed_when: false` turn a timeout into a pass.
    #[test]
    fn a_timed_out_run_is_never_retried_nor_judged() {
        let mut t = task("command");
        t.retries = Some(json!(3));
        t.delay = Some(json!(0));
        t.failed_when = vec!["false".into()];
        let item = bare_item();
        let templar = Templar::new(PathBuf::from("."));
        let retry = retry_plan(&t, Some(&item), &templar)
            .unwrap()
            .expect("a retry plan");
        let Attempt::Done(r) =
            judge_attempt(&t, &item, TaskResult::timed_out(1), 1, &retry, &templar)
        else {
            panic!("a timeout ends the loop");
        };
        assert_eq!(r, TaskResult::timed_out(1));
        assert_eq!(
            finish(&t, &item, TaskResult::timed_out(1), &templar),
            TaskResult::timed_out(1)
        );
    }

    /// A negative `delay` waits one second, as `task_executor.py` has it (`if delay < 0: delay =
    /// 1`), and zero waits nothing.
    ///
    /// What would make this red: a negative delay clamped to zero, which retries a flapping
    /// service in a tight loop where the reference paces it.
    #[test]
    fn a_negative_delay_waits_one_second() {
        let templar = Templar::new(PathBuf::from("."));
        let mut t = task("command");
        t.retries = Some(json!(1));
        for (delay, want) in [(-3, 1), (0, 0), (2, 2)] {
            t.delay = Some(json!(delay));
            let retry = retry_plan(&t, Some(&bare_item()), &templar)
                .unwrap()
                .expect("a retry plan");
            assert_eq!(retry.delay, Duration::from_secs(want), "delay: {delay}");
        }
    }

    /// The plugin path follows both rules: `changed_when` reads `attempts`, and a sub-task that
    /// timed out is the item's result as it stands, with nothing after it sent.
    ///
    /// What would make this red: the plugin handed the timed-out `stat` as if it had answered,
    /// which dresses the timeout as a `copy` failure and retries it; or `attempts` set after the
    /// conditions on this path, which the shared `judge_attempt` is there to prevent.
    #[tokio::test]
    async fn a_plugin_s_retries_follow_the_same_two_rules() {
        let blob = hello_blob();
        let mut stop = watch::channel(false).1;
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(1, json!({"stat": {"exists": false}})).to_vec(),
                vec![state(&blob.hash, true)],
                one_result(2, json!({"failed": true, "msg": "no"})).to_vec(),
                one_result(3, json!({"stat": {"exists": false}})).to_vec(),
                vec![state(&blob.hash, true)],
                one_result(4, json!({"changed": false})).to_vec(),
            ]
            .concat(),
        );
        let mut t = retried_copy(3);
        t.register = Some("r".into());
        t.changed_when = vec!["r.attempts > 1".into()];
        let (ran, _) = copy_attempts(&t, copy_args("/tmp/v/attempts"), &mut agent, &mut stop).await;
        let result = ran.expect("the item ran").expect("nothing stopped it");
        assert!(!result.failed(), "{result:?}");
        assert_eq!(result.0["changed"], json!(true), "{result:?}");
        assert_eq!(result.0["attempts"], json!(2));

        let timed_out = Value::Object(TaskResult::timed_out(1).0);
        let mut agent = FakeAgent::answering(
            [vec![state("ab", true)], one_result(1, timed_out).to_vec()].concat(),
        );
        let (ran, lefts) = copy_attempts(
            &retried_copy(3),
            copy_args("/tmp/v/slow"),
            &mut agent,
            &mut stop,
        )
        .await;
        let result = ran.expect("the item ran").expect("nothing stopped it");
        assert!(result.failed(), "{result:?}");
        assert_eq!(msg(&result), "Task failed: Timed out after 1 second(s).");
        assert_eq!(result.0["timedout"], json!({"period": 1}));
        assert!(!result.0.contains_key("attempts"), "{result:?}");
        assert_eq!(lefts, Vec::<u32>::new());
        assert_eq!(modules_sent(&agent), ["stat"]);
    }

    /// `until` reads the result the whole sequence ended with, judged by the task's conditions,
    /// and never a sub-task's on the way: here the `stat` of each attempt says `changed` and the
    /// first `copy` does not.
    ///
    /// What would make this red: `until` evaluated against the first sub-task's result, which
    /// stops after one attempt; or against the raw result before `changed_when`, which would
    /// have to be tested by the rule a module's retry follows and would drift from it.
    #[tokio::test]
    async fn until_reads_the_result_a_plugin_s_sequence_ended_with() {
        let blob = hello_blob();
        let mut stop = watch::channel(false).1;
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(1, json!({"changed": true, "stat": {"exists": false}})).to_vec(),
                vec![state(&blob.hash, true)],
                one_result(2, json!({"changed": false})).to_vec(),
                one_result(3, json!({"changed": true, "stat": {"exists": false}})).to_vec(),
                vec![state(&blob.hash, true)],
                one_result(4, json!({"changed": true})).to_vec(),
            ]
            .concat(),
        );
        let mut t = retried_copy(3);
        t.register = Some("r".into());
        t.until = vec!["r.changed".into()];
        let (ran, lefts) =
            copy_attempts(&t, copy_args("/tmp/v/until"), &mut agent, &mut stop).await;
        let result = ran.expect("the item ran").expect("nothing stopped it");
        assert!(!result.failed(), "{result:?}");
        assert_eq!(result.0["attempts"], json!(2));
        assert_eq!(lefts, [3]);
    }

    /// A stop raised while one attempt ends is honoured before the next begins, even with no
    /// delay to wait through: the item ends there with no result, and nothing more is sent.
    ///
    /// What would make this red: a zero delay skipping the look at the stop, which starts the
    /// second attempt and sends its batch before the stop is noticed.
    #[tokio::test]
    async fn a_stop_between_two_attempts_ends_the_item_at_once() {
        let (tx, mut stop) = watch::channel(false);
        let mut agent = StopsAfter {
            agent: FakeAgent::answering(
                [
                    vec![state("ab", true)],
                    one_result(1, json!({"failed": true, "msg": "no"})).to_vec(),
                ]
                .concat(),
            ),
            stop: tx,
            after: 1,
        };
        let (ran, lefts) = copy_attempts(
            &retried_copy(2),
            json!({"src": "/a", "dest": "/b", "remote_src": true}),
            &mut agent,
            &mut stop,
        )
        .await;
        assert!(matches!(ran, Ok(None)), "{ran:?}");
        assert_eq!(lefts, [2]);
        assert_eq!(modules_sent(&agent.agent), ["copy"]);
    }

    /// A file the agent cannot store fails the item with the agent's own reason, and the host is
    /// not ended: its next task still runs.
    ///
    /// What would make this red: the refusal read as the link lost, which calls a host
    /// unreachable whose disk is full, or the `copy` sent anyway without its `src`.
    #[tokio::test]
    async fn a_file_the_agent_cannot_store_fails_the_item_not_the_host() {
        use crate::action_plugins::Kind;
        let blob = hello_blob();
        let mut stop = watch::channel(false).1;
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(1, json!({"stat": {"exists": false}})).to_vec(),
                vec![
                    FromAgent::Log {
                        level: volant_protocol::LogLevel::Error,
                        message: format!(
                            "storing payload {}: No space left on device (os error 28)",
                            blob.hash
                        ),
                    },
                    state(&blob.hash, false),
                ],
            ]
            .concat(),
        );
        let (ran, _) = plugin_item(
            Kind::Copy,
            copy_args("/tmp/v/full"),
            json!({}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        let result = ran.expect("a task failure, not the host's end");
        assert!(result.failed(), "{result:?}");
        assert!(
            msg(&result).contains("'src'")
                && msg(&result).contains("No space left on device (os error 28)"),
            "{result:?}"
        );
        let modules: Vec<&str> = batches_sent(&agent)
            .iter()
            .map(|b| b[0].module.as_str())
            .collect();
        assert_eq!(modules, ["stat"], "no copy without its file");
    }

    /// The payload travels with every item of the task that carries it, and with nothing else.
    #[test]
    fn a_python_task_carries_its_payload_to_the_agent() {
        let item = bare_item();
        let module = module_payload("ab");
        let built = protocol_task(&task("ping"), &item, Some((&module, "/usr/bin/python3")));
        assert_eq!(built.payload, Some(payload("ab", "/usr/bin/python3")));
        assert_eq!(protocol_task(&task("command"), &item, None).payload, None);
    }

    /// The module's half of a payload is the same on every host; the interpreter is not, and it
    /// is added where it is chosen.
    ///
    /// What would make this red: an interpreter carried along with the module's half, which is a
    /// payload that can be built before the host has said what it has - and a payload sent with
    /// an empty or stale interpreter runs the module under something other than what was chosen,
    /// which is the failure this whole path exists to avoid.
    #[test]
    fn one_module_payload_serves_every_interpreter_it_is_sent_under() {
        let item = bare_item();
        let module = module_payload("ab");
        let here = protocol_task(&task("ping"), &item, Some((&module, "/usr/bin/python3")));
        let there = protocol_task(&task("ping"), &item, Some((&module, "/usr/bin/python3.12")));
        assert_eq!(
            here.payload.as_ref().map(|p| p.interpreter.as_str()),
            Some("/usr/bin/python3")
        );
        assert_eq!(
            there.payload.as_ref().map(|p| p.interpreter.as_str()),
            Some("/usr/bin/python3.12")
        );
        assert_eq!(
            here.payload.map(|p| p.module_fqn),
            there.payload.map(|p| p.module_fqn)
        );
    }

    fn bare_item() -> Item {
        Item {
            element: None,
            label: None,
            args: Map::new(),
            args_untrusted: BTreeSet::new(),
            vars: HostVars::default(),
            environment: BTreeMap::new(),
            ignore_errors: None,
            skipped: None,
        }
    }

    fn module_payload(blob: &str) -> ModulePayload {
        ModulePayload {
            blob: blob.to_string(),
            facts: crate::python::ModuleFacts {
                module_fqn: "ansible.modules.ping".into(),
                profile: "legacy".into(),
                rlimit_nofile: 0,
                extensions: Map::new(),
                core: true,
            },
            force_python: false,
        }
    }

    fn payload(blob: &str, interpreter: &str) -> volant_protocol::PythonPayload {
        volant_protocol::PythonPayload {
            blob: blob.to_string(),
            module_fqn: "ansible.modules.ping".into(),
            profile: "legacy".into(),
            rlimit_nofile: 0,
            extensions: Map::new(),
            interpreter: interpreter.into(),
        }
    }

    fn one_host_store() -> VarStore {
        let inventory = crate::inventory::Inventory::parse_ini("h1\n").expect("an inventory");
        VarStore::new(&inventory, None, Path::new("."), Map::new()).expect("a var store")
    }

    fn registered(store: &mut VarStore, value: Value) {
        let mut t = task("ping");
        t.register = Some("probe".into());
        let results = vec![(None, TaskResult(vars(value)))];
        record_registered(store, &t, &["h1".to_string()], &results);
    }

    /// A registered result carries no `invocation`, as the reference registers it - measured on
    /// ansible-core 2.19.12, `s.invocation` after a registered `stat` fails with `object of type
    /// 'dict' has no attribute 'invocation'` - while `-vvv` still shows it on the task's line.
    /// A loop's items keep theirs: the reference deletes the top-level key alone.
    ///
    /// What would make this red: the key kept in the registered value, which makes a native
    /// module and the Python module it stands in for register two different things; or removed
    /// from the result itself, which takes it off the `-vvv` line.
    #[test]
    fn a_registered_result_drops_its_invocation_and_the_line_keeps_it() {
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let result = TaskResult(vars(json!({
            "changed": false,
            "invocation": {"module_args": {"path": "/etc/hostname"}},
            "stat": {"exists": true},
        })));
        let mut store = one_host_store();
        let mut t = task("stat");
        t.register = Some("s".into());
        record_registered(
            &mut store,
            &t,
            &["h1".to_string()],
            &[(None, result.clone())],
        );
        let seen = store.for_host("h1", &crate::vars::Scope::default());
        assert_eq!(
            seen["s"],
            json!({"changed": false, "stat": {"exists": true}})
        );
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut out =
            crate::render::Renderer::with_writer(Box::new(Sink(buf.clone())), false, 79, 3);
        out.result(
            "h1",
            Outcome::Ok,
            &result,
            None,
            crate::render::Dump::No,
            false,
            None,
        );
        let line = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(line.contains(r#""invocation": {"module_args""#), "{line}");

        // A loop's items are registered whole.
        t.loop_items = Some(json!(["a"]));
        record_registered(
            &mut store,
            &t,
            &["h1".to_string()],
            &[(Some(json!("a")), result)],
        );
        let seen = store.for_host("h1", &crate::vars::Scope::default());
        assert!(
            seen["s"]["results"][0].get("invocation").is_some(),
            "{}",
            seen["s"]
        );
    }

    /// Everything a Python module returns is untrusted, exactly like a native module's result.
    ///
    /// What would make this red: the result entered through `set_fact`, which would let a
    /// managed host put a template in a value and have the controller render it - the hole the
    /// trust model closed in 1.4c, reopened by a new entry point. There is no backstop under a
    /// missed site: the rule that every `set_fact` is data was dropped when the taint took over.
    #[test]
    fn a_python_module_result_enters_untrusted() {
        let mut store = one_host_store();
        registered(&mut store, json!({"changed": false}));
        assert!(
            store
                .untrusted_of("h1", &crate::vars::Scope::default())
                .contains("probe")
        );
    }

    /// A module that returns a template does not get it rendered. The marker file is the proof.
    ///
    /// Unix only: the lookup it runs is a `touch`, and on a Windows checkout the temporary path
    /// it is given comes back mangled, which drops the marker somewhere else and fails the
    /// control below. The mechanism is the same on both, and this is the platform the tests run
    /// on.
    ///
    /// What would make this red: the registered value rendered on the way in, which turns any
    /// managed host into a command execution on the controller. The second half is what keeps
    /// the first from passing for the wrong reason: the same text written by an author-side
    /// source does run the lookup, so the marker's absence above is the taint and not a
    /// `lookup` that does nothing here.
    #[cfg(unix)]
    #[test]
    fn a_template_returned_by_a_python_module_is_never_rendered() {
        let dir = std::env::temp_dir().join(format!("volant-untrusted-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let templar = Templar::new(PathBuf::from("."));
        let render = |store: &mut VarStore| {
            let vars = HostVars {
                map: store.for_host("h1", &crate::vars::Scope::default()),
                untrusted: store.untrusted_of("h1", &crate::vars::Scope::default()),
                untrusted_hosts: store.untrusted_hosts(),
                ..HostVars::default()
            };
            templar.render("{{ probe.msg }}", crate::template::Vars::from(&vars))
        };

        let marker = dir.join("untrusted-marker");
        let _ = std::fs::remove_file(&marker);
        let mut store = one_host_store();
        let text = format!("{{{{ lookup('pipe', 'touch {}') }}}}", marker.display());
        registered(&mut store, json!({ "msg": text }));
        // Asserted, not discarded: a `record_registered` that wrote nothing would leave
        // `probe.msg` undefined, the render would fail, and the marker would be absent for a
        // reason that has nothing to do with the taint. The value has to arrive, and arrive
        // verbatim.
        assert_eq!(
            render(&mut store).expect("the value renders as the text it is"),
            text
        );
        assert!(!marker.exists(), "a module's own output was rendered");

        let author = dir.join("author-marker");
        let _ = std::fs::remove_file(&author);
        let mut store = one_host_store();
        store.set_fact(
            "h1",
            "probe",
            json!({"msg": format!("{{{{ lookup('pipe', 'touch {}') }}}}", author.display())}),
        );
        render(&mut store).expect("an author-side template renders");
        assert!(author.exists(), "the lookup itself does nothing here");
    }

    /// Gathered facts are untrusted, and they land under `ansible_facts` as well as flat.
    ///
    /// What would make this red: facts entered through `set_fact`, which trusts a managed
    /// host's own words; or only the flat names written, which breaks
    /// `ansible_facts['hostname']` - measurement 10 of plan 1.5, where `gather_facts: true`
    /// followed by a `set_fact` of the same name leaves both readable: the flat name is masked
    /// and the entry under `ansible_facts` is not. Or the namespace keyed by the name `setup`
    /// returned, which keeps its `ansible_` prefix where ansible-core strips it.
    #[test]
    fn gathered_facts_are_untrusted_and_land_under_both_names() {
        let templar = Templar::new(PathBuf::from("."));
        let mut store = one_host_store();
        let render = |store: &mut VarStore, text: &str| {
            let vars = HostVars {
                map: store.for_host("h1", &crate::vars::Scope::default()),
                untrusted: store.untrusted_of("h1", &crate::vars::Scope::default()),
                untrusted_hosts: store.untrusted_hosts(),
                ..HostVars::default()
            };
            templar
                .render(text, crate::template::Vars::from(&vars))
                .expect("a gathered fact reads")
        };
        // The shape ansible-core 2.19.12's `setup` returns, measured with `gather_subset: [min]`:
        // every fact already carries the `ansible_` prefix except `gather_subset` and
        // `module_setup`, and `ansible_local` is always there.
        let result = TaskResult(vars(json!({
            "ansible_facts": {
                "ansible_hostname": "probe-hostname",
                "ansible_distribution": "Ubuntu",
                "ansible_local": {},
                "gather_subset": ["min"],
                "module_setup": true,
            },
            "changed": false,
        })));
        record_facts(&mut store, &["h1".to_string()], &[(None, result)]);
        // Read through a real render rather than off the store, because reading is what a
        // playbook does with a fact: a name written somewhere `for_host` does not merge is a
        // fact nothing can use.
        assert_eq!(
            render(&mut store, "{{ ansible_hostname }}"),
            "probe-hostname"
        );
        assert_eq!(
            render(&mut store, "{{ ansible_facts['distribution'] }}"),
            "Ubuntu"
        );
        // The reference, same play: `ansible_facts['ansible_distribution']` is undefined, and
        // `ansible_local` is the one prefix `namespace_facts()` keeps.
        assert_eq!(
            render(
                &mut store,
                "{{ ansible_facts['ansible_distribution'] is defined }} \
                 {{ ansible_facts.ansible_local is defined }} {{ ansible_facts.local is defined }} \
                 {{ ansible_facts.module_setup }} {{ ansible_ansible_hostname is defined }}"
            ),
            "False True False True False"
        );
        // Flat, a key lands as the module spelled it: the reference's `clean_facts()` adds no
        // prefix, so `module_setup` is there and `ansible_module_setup` is undefined.
        assert_eq!(
            render(
                &mut store,
                "{{ module_setup }} {{ gather_subset | first }} {{ ansible_module_setup is defined }}"
            ),
            "True min False"
        );
        for name in ["ansible_hostname", "ansible_facts"] {
            assert!(
                store
                    .untrusted_of("h1", &crate::vars::Scope::default())
                    .contains(name),
                "{name} carries a managed host's own words"
            );
        }
        store.set_fact("h1", "ansible_hostname", json!("SHADOWED"));
        assert_eq!(
            render(
                &mut store,
                "{{ ansible_hostname }} / {{ ansible_facts.hostname }}"
            ),
            "SHADOWED / probe-hostname"
        );
    }

    /// A failed task keeps none of the facts its host's results carry, whatever module ran there
    /// (a controller-side `set_fact` writes its own as it runs, and is not judged here): the
    /// reference records facts only in the branch of its strategy that a task that is neither
    /// failed, unreachable nor skipped takes, `ignore_errors` or not. Measured on ansible-core
    /// 2.19.12 with a `dnf` task that failed carrying `ansible_facts: {pkg_mgr: apt}`: the fact is
    /// undefined at the next task. A loop is judged whole, as the reference judges it: one failed
    /// item fails the task, so the facts of the items that did not fail go too.
    ///
    /// What would make this red: `record_facts` reading a failed result's facts, which lets a
    /// task that failed change what the next task reads; or reading `failed` before
    /// `failed_when` decided, which drops the facts of a result the playbook forgave.
    #[test]
    fn a_failed_task_keeps_none_of_its_facts() {
        let scope = crate::vars::Scope::default();
        let fact = |value: Value| TaskResult(vars(value));
        let mut store = one_host_store();
        record_facts(
            &mut store,
            &["h1".to_string()],
            &[(
                None,
                fact(json!({"failed": true, "msg": "boom", "ansible_facts": {"kept": 1}})),
            )],
        );
        record_facts(
            &mut store,
            &["h1".to_string()],
            &[
                (
                    Some(json!(1)),
                    fact(json!({"changed": false, "ansible_facts": {"looped": 1}})),
                ),
                (Some(json!(2)), fact(json!({"failed": true, "rc": 1}))),
            ],
        );
        let host = store.for_host("h1", &scope);
        assert!(!host.contains_key("kept"), "{host:?}");
        assert!(!host.contains_key("looped"), "{host:?}");
        // A non-zero `rc` that `failed_when: false` forgave: `failed` is what decides.
        record_facts(
            &mut store,
            &["h1".to_string()],
            &[(
                None,
                fact(json!({"rc": 2, "failed": false, "ansible_facts": {"forgiven": 1}})),
            )],
        );
        assert_eq!(store.for_host("h1", &scope)["forgiven"], json!(1));
    }

    /// A managed host cannot choose where its own next task connects, as whom, or under which
    /// interpreter. Gathered facts rank over the inventory, where `ansible_host` lives, so a
    /// module answering `ansible_facts: {ansible_host: <elsewhere>}` would otherwise move the
    /// connection. ansible-core 2.19.12's `clean_facts()` strips those names from the flat
    /// variables with `[WARNING]: Removed restricted key from module data: <name>`; measured with
    /// a module returning this set, the reference kept `ansible_ssh_host_key_rsa_public`,
    /// `ansible_ssh_foo_bridge`, `ansible_local` and `plain`, dropped `_ansible_hidden` and the
    /// nested `_ansible_inner` without a word, and warned once for each of the others.
    ///
    /// What would make this red: facts merged without the restricted names taken out, so the
    /// transport built for the host's next task reads the host's own answer.
    #[test]
    fn a_fact_cannot_move_the_hosts_next_connection() {
        let inventory = crate::inventory::Inventory::parse_ini(
            "h1 ansible_host=192.0.2.10 ansible_python_interpreter=/usr/bin/python3\n",
        )
        .expect("an inventory");
        let mut store = VarStore::new(&inventory, None, Path::new("."), Map::new()).unwrap();
        let result = TaskResult(vars(json!({
            "ansible_facts": {
                "ansible_host": "203.0.113.9",
                "ansible_connection": "local",
                "ansible_user": "intruder",
                "ansible_python_interpreter": "/tmp/evil/python",
                "ansible_foo_interpreter": "x",
                "ansible_become_password": "x",
                "ansible_become_anything": "x",
                "ansible_ssh_extra_args": "-oProxyCommand=x",
                "ansible_remote_tmp": "/home/deploy/x",
                "ansible_ssh_host_key_rsa_public": "KEY",
                "ansible_ssh_foo_bridge": "br",
                "ansible_local": {"a": 1},
                "ansible_local_thing": "x",
                "ansible_winrm_x": "x",
                "ansible_paramiko_ssh_x": "x",
                "ansible_psrp_x": "x",
                "ansible_network_os": "x",
                "ansible_rsync_path": "x",
                "ansible_playbook_python": "x",
                "add_host": "x",
                "add_group": "x",
                "_ansible_hidden": "x",
                "nested": {"_ansible_inner": 1, "kept": 2},
                "plain": 1,
            },
            "changed": false,
        })));
        let warned = record_facts(&mut store, &["h1".to_string()], &[(None, result)]);
        let v = store.for_host("h1", &crate::vars::Scope::default());
        let defaults = ConnectionDefaults {
            remote_user: None,
            private_key: None,
            host_key_checking: true,
            remote_tmp: "~/.ansible/tmp".to_string(),
            connect_timeout: Duration::from_secs(10),
            r#become: false,
            become_user: "root".to_string(),
            become_method: "sudo".to_string(),
            become_password: None,
        };
        let Transport::Ssh(target) = Transport::for_vars("h1", &v, &defaults).unwrap() else {
            panic!("a fact turned the host's connection local");
        };
        assert_eq!(target.address, "192.0.2.10");
        assert_eq!(target.user, None);
        assert!(target.extra_args.is_empty(), "{:?}", target.extra_args);
        // Where the escalated agent is looked for: a host that picks it plants its own.
        assert_eq!(target.remote_tmp, defaults.remote_tmp);
        assert_eq!(
            requested_interpreter(&v).as_deref(),
            Some("/usr/bin/python3")
        );
        assert!(!v.contains_key("ansible_become_password"));
        assert_eq!(v["ansible_ssh_host_key_rsa_public"], json!("KEY"));
        assert_eq!(v["ansible_ssh_foo_bridge"], json!("br"));
        assert_eq!(v["ansible_local"], json!({"a": 1}));
        assert_eq!(v["plain"], json!(1));
        assert!(!v.contains_key("_ansible_hidden"));
        assert_eq!(v["nested"], json!({"kept": 2}));
        assert_eq!(
            warned,
            [
                "add_group",
                "add_host",
                "ansible_become_anything",
                "ansible_become_password",
                "ansible_connection",
                "ansible_foo_interpreter",
                "ansible_host",
                "ansible_local_thing",
                "ansible_network_os",
                "ansible_paramiko_ssh_x",
                "ansible_playbook_python",
                "ansible_psrp_x",
                "ansible_python_interpreter",
                "ansible_remote_tmp",
                "ansible_rsync_path",
                "ansible_ssh_extra_args",
                "ansible_user",
                "ansible_winrm_x",
            ]
        );
        // The reference leaves the namespace alone, and nothing reads a connection from it.
        assert_eq!(v["ansible_facts"]["host"], json!("203.0.113.9"));
    }

    /// A template inside a gathered fact is text, the way one inside a `register` already is.
    ///
    /// Unix only, for the reason the test above it gives: the lookup is a `touch`.
    ///
    /// What would make this red: the harvest writing through `set_fact`. `untrusted_of` above
    /// names the mechanism; this names what the mechanism is for, and there is nothing under a
    /// missed site since the unconditional "a `set_fact` is data" rule was dropped.
    #[cfg(unix)]
    #[test]
    fn a_template_in_a_gathered_fact_is_never_rendered() {
        let dir = std::env::temp_dir().join(format!("volant-gathered-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let marker = dir.join("gathered-marker");
        let _ = std::fs::remove_file(&marker);
        let text = format!("{{{{ lookup('pipe', 'touch {}') }}}}", marker.display());
        let mut store = one_host_store();
        let result = TaskResult(vars(json!({ "ansible_facts": { "hostname": text } })));
        record_facts(&mut store, &["h1".to_string()], &[(None, result)]);
        let templar = Templar::new(PathBuf::from("."));
        let vars = HostVars {
            map: store.for_host("h1", &crate::vars::Scope::default()),
            untrusted: store.untrusted_of("h1", &crate::vars::Scope::default()),
            untrusted_hosts: store.untrusted_hosts(),
            ..HostVars::default()
        };
        assert_eq!(
            templar
                .render(
                    "{{ ansible_facts.hostname }}",
                    crate::template::Vars::from(&vars)
                )
                .expect("the value renders as the text it is"),
            text
        );
        assert!(!marker.exists(), "a gathered fact was rendered");
    }

    /// `AgentLink`'s `stop_batch` is a one-line forward to `cancel`, and `AgentChannel` is
    /// `pub(super)`: no integration test file can reach it, only a mock `FakeLink` can, and
    /// every other test in this module drives the trait through that mock. `cancel` itself is
    /// proven directly, against a real agent, in `local_transport.rs` - this is the trait's own
    /// forward, against a real agent, called the way the driver actually calls it.
    ///
    /// The link stays open until after the check: `local_transport.rs`'s own
    /// `dropping_the_link_lets_the_agent_stop_its_task` proves the agent stops everything on its
    /// own once the connection closes, so a test that dropped the link right after calling
    /// `stop_batch` could pass for that unrelated reason and prove nothing about the forward.
    ///
    /// What would make this red: emptying `AgentLink`'s `stop_batch` body (returning `true`
    /// without calling `cancel`), which nothing else in this suite would catch.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_real_link_s_stop_batch_forward_reaches_the_agent() {
        let marker = format!("40.{}", std::process::id());
        // The lib test binary lives in target/<profile>/deps/; the workspace's other binaries,
        // volant-agent included, are built one level up, in target/<profile>/ itself.
        let exe = std::env::current_exe().expect("this test's own binary path");
        let agent_dir = exe
            .parent()
            .and_then(Path::parent)
            .expect("deps/ has a parent")
            .to_path_buf();
        let saved = std::env::var("VOLANT_AGENT_DIR").ok();
        // SAFETY: nothing else in this process reads or writes this variable concurrently -
        // `AgentSource::discover` runs synchronously between the two lines that set and restore
        // it, and nextest gives every test its own process.
        unsafe {
            std::env::set_var("VOLANT_AGENT_DIR", &agent_dir);
        }
        // Caught so the restore below runs even when `discover` panics, then re-raised.
        let source = std::panic::catch_unwind(AgentSource::discover);
        match saved {
            Some(v) => unsafe { std::env::set_var("VOLANT_AGENT_DIR", v) },
            None => unsafe { std::env::remove_var("VOLANT_AGENT_DIR") },
        }
        let source = source.unwrap_or_else(|panic| std::panic::resume_unwind(panic));

        let mut vars = BTreeMap::new();
        vars.insert("ansible_connection".to_string(), json!("local"));
        let host = crate::inventory::Host {
            name: "localhost".to_string(),
            vars,
        };
        let defaults = ConnectionDefaults {
            remote_user: None,
            private_key: None,
            host_key_checking: true,
            remote_tmp: "~/.ansible/tmp".to_string(),
            connect_timeout: Duration::from_secs(10),
            r#become: false,
            become_user: "root".to_string(),
            become_method: "sudo".to_string(),
            become_password: None,
        };
        let transport = Transport::for_host(&host, &defaults).expect("a local transport");
        let mut link = transport
            .connect(&source, None)
            .await
            .expect("a real agent");
        link.handshake().await.expect("a real handshake");
        link.send(&ToAgent::RunBatch {
            id: 77,
            tasks: vec![Task {
                module: "shell".into(),
                args: json!({"_raw_params": format!("sleep {marker} & wait")})
                    .as_object()
                    .unwrap()
                    .clone(),
                ignore_errors: false,
                timeout: None,
                environment: BTreeMap::default(),
                payload: None,
                files: Vec::new(),
                force_python: false,
            }],
        })
        .await
        .expect("sending the batch");
        tokio::time::sleep(Duration::from_millis(300)).await;

        // The trait method the driver actually calls, not `cancel` directly.
        let confirmed = AgentChannel::stop_batch(&mut link, 77, Duration::from_secs(5)).await;
        // `kill_group` is a fire-and-forget SIGKILL with nothing waiting on the backgrounded
        // grandchild, so a dying `sleep` can still be in /proc for one instant after the
        // confirmation frame - the same slack `cancellation_kills_the_whole_process_group` gives
        // it before its own `pgrep`.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let survivors = std::process::Command::new("pgrep")
            .args(["-f", &marker])
            .output()
            .expect("pgrep");
        // Before the assertion, not after: a failure here must not also leak the agent and its
        // 40-second sleep.
        link.shutdown().await;
        assert!(confirmed, "the agent must confirm the cancel");
        assert!(
            survivors.stdout.is_empty(),
            "the task kept running on a real agent after the real stop_batch forward: {}",
            String::from_utf8_lossy(&survivors.stdout)
        );
    }

    /// What one connection attempt of a scripted reconnection does.
    enum Try {
        /// A fresh link comes up, answering what this agent was scripted to.
        Up(FakeAgent),
        /// No link, for this reason.
        Down(&'static str),
        /// No answer at all, for as long as anybody waits.
        Hangs,
    }

    /// Reconnections a test scripted, one entry per try, then `after` for every try past them.
    /// Keeps every link it replaced, so a test reads what each one was sent.
    struct Scripted {
        tries: std::collections::VecDeque<Try>,
        after: Option<fn() -> FakeAgent>,
        attempts: usize,
        /// The `connect_timeout` each try was given.
        connect_timeouts: Vec<Option<Duration>>,
        retired: Vec<FakeAgent>,
        /// Raised a tenth of a second after the first try starts, for a test of the run being
        /// interrupted while it waits.
        stop: Option<watch::Sender<bool>>,
        /// A millisecond in place of the reference's seconds, so the schedule's shape is tested
        /// without its length (`the_pause_doubles_from_one_second_up_to_twelve`), unless a test
        /// needs a pause long enough to be interrupted.
        pause: Duration,
    }

    impl Scripted {
        fn new(tries: Vec<Try>) -> Self {
            Scripted {
                tries: tries.into(),
                after: None,
                attempts: 0,
                connect_timeouts: Vec::new(),
                retired: Vec::new(),
                stop: None,
                pause: Duration::from_millis(1),
            }
        }
    }

    impl Relink<FakeAgent> for Scripted {
        async fn relink(
            &mut self,
            link: &mut FakeAgent,
            connect_timeout: Option<Duration>,
        ) -> Result<(), String> {
            self.attempts += 1;
            self.connect_timeouts.push(connect_timeout);
            if let Some(stop) = self.stop.take() {
                // A moment later, so that it lands inside whatever wait follows this try.
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let _ = stop.send(true);
                });
            }
            let next = self
                .tries
                .pop_front()
                .or_else(|| self.after.map(|make| Try::Up(make())));
            match next {
                Some(Try::Up(fresh)) => {
                    self.retired.push(std::mem::replace(link, fresh));
                    Ok(())
                }
                Some(Try::Down(why)) => Err(why.to_string()),
                Some(Try::Hangs) => std::future::pending().await,
                None => Err("the script ran out".to_string()),
            }
        }

        fn pause(&self, _failed: u32) -> Duration {
            self.pause
        }
    }

    /// Every `raw` line and module a set of links was sent, in order.
    fn sent_lines(links: &[&FakeAgent]) -> Vec<Vec<String>> {
        links
            .iter()
            .map(|agent| {
                batches_sent(agent)
                    .iter()
                    .map(|b| match b[0].module.as_str() {
                        "raw" => b[0].args["_raw_params"].as_str().unwrap_or("").to_string(),
                        module => module.to_string(),
                    })
                    .collect()
            })
            .collect()
    }

    fn raw_answer(batch: u64, rc: i64, stdout: &str) -> [FromAgent; 2] {
        one_result(
            batch,
            json!({"rc": rc, "stdout": stdout, "stderr": "", "changed": true}),
        )
    }

    /// The host a `reboot` starts on: `setup`, the boot id, `find`, and whatever `shutdown`
    /// answers, which is nothing at all when `shutdown` is `None` - the link goes down under it.
    fn before_the_reboot(shutdown: Option<Vec<FromAgent>>) -> FakeAgent {
        FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(
                    1,
                    json!({"ansible_facts": {"ansible_distribution": "Ubuntu",
                        "ansible_distribution_version": "24.04", "ansible_os_family": "Debian"}}),
                )
                .to_vec(),
                raw_answer(2, 0, "old-boot-id\n").to_vec(),
                one_result(
                    3,
                    json!({"files": [{"path": "/sbin/shutdown"}], "matched": 1}),
                )
                .to_vec(),
                shutdown.unwrap_or_default(),
            ]
            .concat(),
        )
    }

    async fn reboot_item(
        args: Value,
        agent: &mut FakeAgent,
        relink: &mut Scripted,
        local: bool,
        stop: &mut watch::Receiver<bool>,
    ) -> Result<TaskResult, Result<BatchOutcome, String>> {
        reboot_item_under(None, args, agent, relink, local, stop).await
    }

    /// [`reboot_item`] for a task with the `timeout` keyword.
    async fn reboot_item_under(
        timeout: Option<u64>,
        args: Value,
        agent: &mut FakeAgent,
        relink: &mut Scripted,
        local: bool,
        stop: &mut watch::Receiver<bool>,
    ) -> Result<TaskResult, Result<BatchOutcome, String>> {
        // A broken wait is a wait that never ends: bounded here, so it fails rather than hangs.
        let (ran, _) = tokio::time::timeout(
            Duration::from_secs(30),
            plugin_item_on(
                crate::action_plugins::Kind::Reboot,
                args,
                json!({}),
                agent,
                relink,
                local,
                timeout,
                &python3(),
                stop,
            ),
        )
        .await
        .expect("the item ended within thirty seconds");
        ran
    }

    /// The whole sequence measured on ansible-core 2.19.12 with `reboot: {}` on a real host:
    /// `setup` of the `min` subset, the boot id, `find` for `shutdown`, `/sbin/shutdown -r 0
    /// "Reboot initiated by Ansible"` under which the link goes down, then fresh links until the
    /// boot id changes - through two that did not come up and one that still read the old boot
    /// id - and `whoami`. The result is the reference's `{"changed": true, "elapsed": <s>,
    /// "rebooted": true}`.
    ///
    /// What would make this red: the link lost under `shutdown` taken for the host lost, which
    /// reports it unreachable; a failed connection taken for the end; the old boot id taken for
    /// a new boot, which stops one link early; the boot id read again on the link that was open
    /// before the reboot rather than on a fresh one; or `whoami` never run.
    #[tokio::test]
    async fn a_reboot_waits_through_dead_links_for_a_new_boot_id() {
        let mut stop = watch::channel(false).1;
        let mut agent = before_the_reboot(None);
        let mut relink = Scripted::new(vec![
            Try::Down("ssh: connect to host target port 22: No route to host"),
            Try::Down("Timeout (12s) waiting for privilege escalation prompt"),
            Try::Up(FakeAgent::answering(
                raw_answer(5, 0, "old-boot-id\n").to_vec(),
            )),
            Try::Up(FakeAgent::answering(
                [
                    raw_answer(6, 0, "new-boot-id\n").to_vec(),
                    raw_answer(7, 0, "root\n").to_vec(),
                ]
                .concat(),
            )),
        ]);
        let result = reboot_item(json!({}), &mut agent, &mut relink, false, &mut stop)
            .await
            .expect("the item ran to its end");
        assert_eq!(
            Value::Object(result.0),
            json!({"changed": true, "elapsed": 0, "rebooted": true})
        );
        assert_eq!(
            relink.attempts, 4,
            "two down, one on the old boot, one back"
        );
        let links: Vec<&FakeAgent> = relink.retired.iter().chain([&agent]).collect();
        assert_eq!(
            sent_lines(&links),
            [
                vec![
                    "setup",
                    "cat /proc/sys/kernel/random/boot_id",
                    "find",
                    "/sbin/shutdown -r 0 \"Reboot initiated by Ansible\"",
                ],
                vec!["cat /proc/sys/kernel/random/boot_id"],
                vec!["cat /proc/sys/kernel/random/boot_id", "whoami"],
            ]
        );
        let first = batches_sent(links[0]);
        assert_eq!(
            Value::Object(first[0][0].args.clone()),
            json!({"gather_subset": ["min"]})
        );
        assert_eq!(
            Value::Object(first[2][0].args.clone()),
            json!({"paths": ["/sbin", "/bin", "/usr/sbin", "/usr/bin", "/usr/local/sbin"],
                "patterns": ["shutdown"], "file_type": "any"})
        );
        assert!(
            first[1][0].payload.is_none() && first[3][0].payload.is_none(),
            "the commands are the agent's own `raw`"
        );
    }

    /// A host whose boot id never changes fails the task once `reboot_timeout` has run out, in
    /// the reference's words (`reboot.py`, `do_until_success_or_timeout`), with `rebooted: true`.
    ///
    /// What would make this red: the wait never ending - `volant_within` is not needed, the
    /// test's own clock is one second - or ending on the first unchanged answer, or failing
    /// with a sentence of its own.
    #[tokio::test]
    async fn a_boot_id_that_never_changes_times_out_by_name() {
        let mut stop = watch::channel(false).1;
        let mut agent = before_the_reboot(None);
        let mut relink = Scripted::new(Vec::new());
        relink.after = Some(|| FakeAgent::answering(raw_answer(9, 0, "old-boot-id\n").to_vec()));
        let result = reboot_item(
            json!({"reboot_timeout": 1}),
            &mut agent,
            &mut relink,
            false,
            &mut stop,
        )
        .await
        .expect("the item ran to its end");
        assert!(result.failed(), "{result:?}");
        assert_eq!(
            msg(&result),
            "Timed out waiting for last boot time check (timeout=1)"
        );
        assert_eq!(result.0["rebooted"], json!(true));
        assert!(relink.attempts > 1, "{}", relink.attempts);
    }

    /// A `shutdown` that answers and fails is the task's failure at once, with the reference's
    /// sentence, and no reconnection is tried: nothing is going to reboot.
    ///
    /// What would make this red: the answer ignored because the link is expected to die, which
    /// waits out the whole `reboot_timeout` - ten minutes by default - for a reboot that was
    /// refused.
    #[tokio::test]
    async fn a_refused_shutdown_fails_at_once() {
        let mut stop = watch::channel(false).1;
        let refused = one_result(
            4,
            json!({"rc": 1, "stdout": "", "stderr": "Failed to set wall message, ignoring: Interactive authentication required.\n", "changed": true}),
        )
        .to_vec();
        let mut agent = before_the_reboot(Some(refused));
        let mut relink = Scripted::new(Vec::new());
        let result = reboot_item(json!({}), &mut agent, &mut relink, false, &mut stop)
            .await
            .expect("the item ran to its end");
        assert!(result.failed(), "{result:?}");
        assert_eq!(
            msg(&result),
            "Reboot command failed. Error was: ', Failed to set wall message, ignoring: Interactive authentication required.'"
        );
        assert_eq!(result.0["rebooted"], json!(false));
        assert_eq!(relink.attempts, 0);
    }

    /// No `shutdown` in the search paths fails the task naming them as Python prints a list, and
    /// nothing is run in its place.
    #[tokio::test]
    async fn a_shutdown_nowhere_to_be_found_fails_naming_the_paths() {
        let mut stop = watch::channel(false).1;
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(
                    1,
                    json!({"ansible_facts": {"ansible_distribution": "Ubuntu",
                        "ansible_distribution_version": "24.04", "ansible_os_family": "Debian"}}),
                )
                .to_vec(),
                raw_answer(2, 0, "old-boot-id\n").to_vec(),
                one_result(3, json!({"files": [], "matched": 0})).to_vec(),
            ]
            .concat(),
        );
        let mut relink = Scripted::new(Vec::new());
        let result = reboot_item(
            json!({"search_paths": ["/opt/bin", "/usr/local/bin"]}),
            &mut agent,
            &mut relink,
            false,
            &mut stop,
        )
        .await
        .expect("the item ran to its end");
        assert_eq!(
            msg(&result),
            "Unable to find command \"shutdown\" in search paths: ['/opt/bin', '/usr/local/bin']"
        );
        assert_eq!(batches_sent(&agent).len(), 3, "no shutdown sent");
    }

    /// On the controller's own connection, `reboot` refuses before anything runs, with the
    /// reference's result: there is only one machine it could restart.
    ///
    /// What would make this red: the guard gone, which runs `shutdown -r` on the machine running
    /// the playbook.
    #[tokio::test]
    async fn a_local_connection_is_never_rebooted() {
        let mut stop = watch::channel(false).1;
        let mut agent = FakeAgent::answering(Vec::new());
        let mut relink = Scripted::new(Vec::new());
        let result = reboot_item(json!({}), &mut agent, &mut relink, true, &mut stop)
            .await
            .expect("the item ran to its end");
        assert_eq!(
            Value::Object(result.0),
            json!({"changed": false, "elapsed": 0, "failed": true,
                "msg": "Running reboot with local connection would reboot the control node.",
                "rebooted": false})
        );
        assert!(agent.sent.is_empty(), "{:?}", agent.sent);
    }

    /// An interruption during the pause between two tries ends the item at once, as one during a
    /// retry's `delay` does, and nothing more is tried.
    ///
    /// What would make this red: the pause not watching the stop, which sits out the whole pause
    /// - here a minute - before the next try notices it.
    #[tokio::test]
    async fn an_interruption_ends_the_pause_between_two_tries() {
        let (tx, mut stop) = watch::channel(false);
        let mut agent = before_the_reboot(None);
        let mut relink = Scripted::new(vec![Try::Down("No route to host")]);
        relink.stop = Some(tx);
        relink.pause = Duration::from_secs(60);
        let ended = reboot_item(json!({}), &mut agent, &mut relink, false, &mut stop).await;
        assert!(
            matches!(ended, Err(Ok(BatchOutcome::Cancelled { .. }))),
            "{ended:?}"
        );
        assert_eq!(relink.attempts, 1);
    }

    /// An interruption while a try hangs ends the item at once, without waiting for the try.
    ///
    /// What would make this red: the try not raced against the stop, which waits for it as long
    /// as `reboot_timeout` allows - ten minutes by default.
    #[tokio::test]
    async fn an_interruption_ends_a_try_that_hangs() {
        let (tx, mut stop) = watch::channel(false);
        let mut agent = before_the_reboot(None);
        let mut relink = Scripted::new(vec![Try::Hangs]);
        relink.stop = Some(tx);
        let ended = reboot_item(json!({}), &mut agent, &mut relink, false, &mut stop).await;
        assert!(
            matches!(ended, Err(Ok(BatchOutcome::Cancelled { .. }))),
            "{ended:?}"
        );
        assert_eq!(relink.attempts, 1);
    }

    /// `connect_timeout` reaches each try as the connection's own setup timeout, and nothing else
    /// is bounded by it: the reference sets it on the connection plugin, so a slow `sudo` or a
    /// slow probe on a connection that did come up is not cut short by it.
    ///
    /// What would make this red: the option read and dropped on its way to the connection.
    #[tokio::test]
    async fn connect_timeout_reaches_the_connection() {
        let mut stop = watch::channel(false).1;
        let mut agent = before_the_reboot(None);
        let mut relink = Scripted::new(vec![Try::Up(FakeAgent::answering(
            [
                raw_answer(5, 0, "new-boot-id\n").to_vec(),
                raw_answer(6, 0, "root\n").to_vec(),
            ]
            .concat(),
        ))]);
        let result = reboot_item(
            json!({"connect_timeout": 3}),
            &mut agent,
            &mut relink,
            false,
            &mut stop,
        )
        .await
        .expect("the item ran to its end");
        assert!(!result.failed(), "{result:?}");
        assert_eq!(relink.connect_timeouts, [Some(Duration::from_secs(3))]);
    }

    /// On ssh the timeout becomes `ConnectTimeout`, in the whole seconds ssh takes; the kept
    /// transport, which names the link, is left alone.
    ///
    /// What would make this red: the fresh connection opened with the play's own timeout.
    #[test]
    fn a_connect_timeout_becomes_ssh_s_connecttimeout() {
        let host = crate::inventory::Host {
            name: "h1".to_string(),
            vars: BTreeMap::from([("ansible_host".to_string(), json!("probe-hostname"))]),
        };
        let defaults = ConnectionDefaults {
            remote_user: None,
            private_key: None,
            host_key_checking: true,
            remote_tmp: "~/.ansible/tmp".to_string(),
            connect_timeout: Duration::from_secs(10),
            r#become: false,
            become_user: "root".to_string(),
            become_method: "sudo".to_string(),
            become_password: None,
        };
        let kept = Transport::for_host(&host, &defaults).expect("an ssh transport");
        let argv = |t: &Transport| match t {
            Transport::Ssh(target) => target.ssh_argv("true").join(" "),
            Transport::Local => panic!("an ssh transport"),
        };
        let fresh = with_connect_timeout(&kept, Some(Duration::from_millis(2500)));
        assert!(
            argv(&fresh).contains("ConnectTimeout=3"),
            "{}",
            argv(&fresh)
        );
        assert!(argv(&kept).contains("ConnectTimeout=10"), "{}", argv(&kept));
        assert_eq!(with_connect_timeout(&kept, None), kept);
    }

    /// The `timeout` keyword bounds the whole item, waits for the host included: a `reboot`
    /// under `timeout: 2` whose host never comes back ends at two seconds with the timeout's own
    /// result, not after `reboot_timeout`. Read from ansible-core 2.19.12 `task_executor.py`: the
    /// alarm wraps the action's whole run and raises out of it.
    ///
    /// What would make this red: the keyword left to each sub-task, which the waits between them
    /// never see, so the item runs to `reboot_timeout` and reports the plugin's own failure.
    #[tokio::test]
    async fn the_timeout_keyword_bounds_a_plugin_s_whole_item() {
        let mut stop = watch::channel(false).1;
        let mut agent = before_the_reboot(None);
        let mut relink = Scripted::new(Vec::new());
        relink.after = Some(|| FakeAgent::answering(raw_answer(9, 0, "old-boot-id\n").to_vec()));
        let result = reboot_item_under(
            Some(2),
            json!({}),
            &mut agent,
            &mut relink,
            false,
            &mut stop,
        )
        .await
        .expect("the item ran to its end");
        assert_eq!(result, TaskResult::timed_out(2));
        // Each sub-task carries what is left of the item's time, in whole seconds: all of it at
        // the start, one second of it once the host has been away for more than one.
        let timeouts = |link: &FakeAgent| -> Vec<Option<u64>> {
            batches_sent(link).iter().map(|b| b[0].timeout).collect()
        };
        assert_eq!(timeouts(&relink.retired[0]), [Some(2); 4]);
        assert_eq!(timeouts(relink.retired.last().expect("a probe")), [Some(1)]);
    }

    /// A `shutdown` that answered and then took the link down reports its answer: a refusal
    /// written just before the connection closed is still a refusal.
    ///
    /// What would make this red: the answer thrown away because the link went, which reports
    /// the host unreachable when the shutdown was refused.
    #[tokio::test]
    async fn a_shutdown_answer_before_the_link_went_is_kept() {
        let mut stop = watch::channel(false).1;
        let answered = vec![FromAgent::TaskResult {
            batch: 4,
            index: 0,
            result: TaskResult(vars(
                json!({"rc": 1, "stdout": "", "stderr": "shutdown: Permission denied\n"}),
            )),
            ran: None,
        }];
        let mut agent = before_the_reboot(Some(answered));
        let mut relink = Scripted::new(Vec::new());
        let result = reboot_item(json!({}), &mut agent, &mut relink, false, &mut stop)
            .await
            .expect("the item ran to its end");
        assert_eq!(
            msg(&result),
            "Reboot command failed. Error was: ', shutdown: Permission denied'"
        );
        assert_eq!(relink.attempts, 0);
    }

    /// The reference's schedule between two tries: 1, 2, 4, 8, then 12 seconds for good.
    #[test]
    fn the_pause_doubles_from_one_second_up_to_twelve() {
        let pauses: Vec<u64> = (1..=7)
            .map(|n| Relink::<FakeAgent>::pause(&NoRelink, n).as_secs())
            .collect();
        assert_eq!(pauses, [1, 2, 4, 8, 12, 12, 12]);
    }

    /// Real agents on the local connection, found the way the other real-link test finds them.
    #[cfg(unix)]
    fn local_agents() -> (AgentSource, ConnectionDefaults) {
        let exe = std::env::current_exe().expect("this test's own binary path");
        let agent_dir = exe
            .parent()
            .and_then(Path::parent)
            .expect("deps/ has a parent")
            .to_path_buf();
        // SAFETY: nextest gives every test its own process, and nothing else here reads it.
        unsafe {
            std::env::set_var("VOLANT_AGENT_DIR", &agent_dir);
        }
        let defaults = ConnectionDefaults {
            remote_user: None,
            private_key: None,
            host_key_checking: true,
            remote_tmp: "~/.ansible/tmp".to_string(),
            connect_timeout: Duration::from_secs(10),
            r#become: false,
            become_user: "root".to_string(),
            become_method: "sudo".to_string(),
            become_password: None,
        };
        (AgentSource::discover(), defaults)
    }

    #[cfg(unix)]
    fn local_key(host: &str, user: Option<&str>) -> LinkKey {
        LinkKey {
            host: host.to_string(),
            become_user: user.map(str::to_string),
            transport: Transport::Local,
        }
    }

    /// Reconnecting drops every link kept for the host - its own and the escalated one - and
    /// no other host's, and puts a fresh link in place of the one the plugin holds. Run against
    /// real agents on the local connection.
    ///
    /// What would make this red: the escalated link left in the map, which the next escalated
    /// task reuses unchecked - it was checked once this play - and fails on as a dead host.
    #[cfg(unix)]
    #[tokio::test]
    async fn reconnecting_drops_the_host_s_links_and_no_other() {
        let (source, defaults) = local_agents();
        let key = local_key;
        let reboots = Reboots::default();
        let mut links = HashMap::new();
        let mut checked = HashMap::new();
        for k in [key("h1", Some("root")), key("h2", None)] {
            let link = connect(&Transport::Local, &source, &defaults, None)
                .await
                .expect("a real agent");
            checked.insert(k.clone(), 0);
            links.insert(k, link);
        }
        let mine = key("h1", None);
        checked.insert(mine.clone(), 0);
        let mut held = connect(&Transport::Local, &source, &defaults, None)
            .await
            .expect("a real agent");
        let mut relink = Relinker {
            links: &mut links,
            checked: &mut checked,
            reboots: &reboots,
            key: &mine,
            escalation: None,
            agents: &source,
            defaults: &defaults,
        };
        relink.relink(&mut held, None).await.expect("a fresh link");
        let left: Vec<&LinkKey> = links.keys().collect();
        assert_eq!(left, [&key("h2", None)]);
        assert_eq!(checked, HashMap::from([(key("h2", None), 0)]));
        assert_eq!((reboots.of("h1"), reboots.of("h2")), (1, 0));
        held.handshake().await.expect("the fresh link answers");
        held.shutdown().await;
        for (_, link) in links.drain() {
            link.shutdown().await;
        }
    }

    /// A link another host's driver keeps to a host this one rebooted is checked again before
    /// it is reused, and replaced when the reboot killed it. Two drivers' maps, one run's count:
    /// `h2`'s driver delegated to `h1` and kept the link; `h1`'s driver rebooted `h1`; `h2`
    /// delegates to `h1` again. In the k3s proof the agents delegate to the first server like
    /// this.
    ///
    /// What would make this red: the reboot count left out of the check, which hands `h2` the
    /// dead link it checked earlier in the play, and reports it unreachable.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_another_driver_keeps_is_checked_again_after_a_reboot() {
        let (source, defaults) = local_agents();
        let (stop_tx, stop) = watch::channel(false);
        let options = RunOptions {
            defaults: defaults.clone(),
            forks: 1,
            force_handlers: false,
            batching: false,
            control_dir: None,
            stop,
            abort: Arc::new(super::super::Abort::new(stop_tx)),
            reboots: Arc::default(),
            profile: Arc::default(),
        };
        let to_h1 = local_key("h1", None);
        // `h2`'s driver: its link to `h1`, proved alive once this play.
        let mut h2_links = HashMap::new();
        let mut h2_checked = HashMap::new();
        reuse_or_connect(
            &mut h2_links,
            &mut h2_checked,
            &to_h1,
            None,
            &source,
            &options,
        )
        .await
        .expect("h2 reaches h1");
        // `h1`'s driver reboots `h1`, which kills every agent on it, `h2`'s included.
        h2_links
            .get_mut(&to_h1)
            .expect("h2 kept its link")
            .kill()
            .await;
        let mut h1_links = HashMap::new();
        let mut h1_checked = HashMap::new();
        let mut held = connect(&Transport::Local, &source, &defaults, None)
            .await
            .expect("a real agent");
        Relinker {
            links: &mut h1_links,
            checked: &mut h1_checked,
            reboots: &options.reboots,
            key: &to_h1,
            escalation: None,
            agents: &source,
            defaults: &defaults,
        }
        .relink(&mut held, None)
        .await
        .expect("h1 is back");
        // `h2` delegates to `h1` again.
        let link = reuse_or_connect(
            &mut h2_links,
            &mut h2_checked,
            &to_h1,
            None,
            &source,
            &options,
        )
        .await
        .expect("h2 reaches h1 again");
        let answered = link.handshake().await;
        held.shutdown().await;
        for (_, link) in h2_links.drain() {
            link.shutdown().await;
        }
        answered.expect("the link h2 got is alive, not the one the reboot killed");
    }
}
