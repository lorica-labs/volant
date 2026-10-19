// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;
use volant_protocol::modules::{ModuleSpec, short_name};

use crate::executor::prepare::{Item, display};
use crate::executor::{as_bool_value, python_type};
use crate::playbook::PlayTask;

use super::interpreter::action_fqcn;
use super::retry::raised;

/// An action's refusal of its arguments, measured on ansible-core 2.19.12 for `assert`, `fail`
/// and `pause`: `changed: false`, and an error block that says `Task failed: ` and the message,
/// with no `Action failed: ` between, since the action raised it before it ran.
pub(super) fn unchanged_failure(msg: impl Into<String>) -> TaskResult {
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
pub(super) fn action_name(module: &str, long: &str) -> String {
    if module.starts_with("ansible.builtin.") {
        long.to_string()
    } else {
        short_name(module).to_string()
    }
}

/// The names a task wrote that the module does not have, sorted.
pub(super) fn unknown_args<'a>(spec: &ModuleSpec, args: &'a Map<String, Value>) -> Vec<&'a str> {
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
pub(super) fn unsupported(
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
pub(super) fn validate_argument_spec(item: &Item) -> TaskResult {
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
            Value::Number(n) if !n.is_f64() => Ok(()),
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
pub(in crate::executor) fn raw_params_refusal(task: &PlayTask) -> Option<TaskResult> {
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

/// What each item of a task reports when the task asks for `async` and its action does not
/// support it, or the raw parameters it was given are refused: the failure the reference raises
/// before the action runs, so neither `failed_when` nor `retries` changes it.
pub(in crate::executor) fn refusal(task: &PlayTask) -> Option<TaskResult> {
    raw_params_refusal(task).or_else(|| super::async_job::async_refusal(task))
}
