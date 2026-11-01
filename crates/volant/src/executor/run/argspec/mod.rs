// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;
use volant_protocol::modules::{ModuleSpec, short_name};

use crate::playbook::PlayTask;

use super::interpreter::action_fqcn;
use super::retry::raised;

mod families;
mod types;
mod validate;

pub(super) use validate::validate_argument_spec;

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
    let msg = format!("Action '{action}' does not support raw params.");
    let mut r = Map::new();
    r.insert("changed".into(), json!(false));
    r.insert("exception".into(), json!("(traceback unavailable)"));
    r.insert("failed".into(), json!(true));
    r.insert("msg".into(), json!(format!("Task failed: {msg}")));
    // Measured: an `action: ping foo` statement, and an ad hoc `-a`, add a cause at the action's
    // value; `ping: foo`, whose value is the task's own place, adds none.
    if let Some(at) = task.value_origins.get("action.value") {
        r.insert(
            crate::render::CHAIN_KEY.into(),
            json!([crate::render::cause(&msg, Some(at), None)]),
        );
    }
    Some(TaskResult(r))
}

/// What each item of a task reports when its `action:` names no action: a first word with a `=`
/// in it, a `module=ping` spelling.
///
/// Measured on ansible-core 2.19.12: the play starts, the banner shows the word, and the task
/// fails with `Task failed: Cannot resolve 'module=ping' to an action or module.` and a cause at
/// the `action:` value; `ignore_errors` and `rescue` take it like any failure of a task. So does
/// a module nothing on the controller answers to, named in an `action:` statement, after the
/// loader's warning when its collection is not installed (`prepare` says it).
fn unresolved_action_refusal(task: &PlayTask) -> Option<TaskResult> {
    if !crate::playbook::names_no_action(&task.module)
        && crate::preflight::missing_at_run(task).is_none()
    {
        return None;
    }
    let msg = format!("Cannot resolve '{}' to an action or module.", task.module);
    let mut r = TaskResult::failed_with(format!("Task failed: {msg}"));
    r.0.insert("changed".into(), json!(false));
    // Registered too, measured, and what `oneline` notes of the failure.
    r.0.insert("exception".into(), json!("(traceback unavailable)"));
    r.0.insert(
        crate::render::CHAIN_KEY.into(),
        json!([crate::render::cause(
            &msg,
            task.value_origins.get("action.value"),
            None
        )]),
    );
    Some(r)
}

/// `expand_argument_vars` on `shell`, which the reference's `shell` action raises on before its
/// module runs, whatever the value, naming the action as the task wrote it. A value that is
/// `omit` takes the key out there, and is let through here to the same end.
fn shell_expand_refusal(task: &PlayTask) -> Option<TaskResult> {
    let value = task.args.get("expand_argument_vars")?;
    let omitted = value
        .as_str()
        .is_some_and(|text| crate::template::Templar::is_template(text) && text.contains("omit"));
    if omitted || volant_protocol::modules::native(&task.module).is_none_or(|m| m.name != "shell") {
        return None;
    }
    let mut r = TaskResult::failed_with(format!(
        "Unsupported parameters for ({}) module: expand_argument_vars",
        task.module
    ));
    r.0.insert("changed".into(), json!(false));
    r.0.insert("exception".into(), json!("(traceback unavailable)"));
    r.0.insert(super::RAISED_FAIL.into(), json!(true));
    Some(r)
}

/// What each item of a task reports when the task asks for `async` and its action does not
/// support it, the raw parameters it was given are refused, or its action cannot be resolved: the
/// failure the reference raises before the action runs, so neither `failed_when` nor `retries`
/// changes it.
pub(in crate::executor) fn refusal(task: &PlayTask) -> Option<TaskResult> {
    unresolved_action_refusal(task)
        .or_else(|| raw_params_refusal(task))
        .or_else(|| shell_expand_refusal(task))
        .or_else(|| super::async_job::async_refusal(task))
}

#[cfg(test)]
mod tests;
