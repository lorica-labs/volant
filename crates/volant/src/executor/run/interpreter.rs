// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::{Map, Value, json};
use volant_protocol::modules::short_name;
use volant_protocol::{BatchOutcome, StagedFile, Task, TaskResult};

use crate::executor::prepare::Item;
use crate::playbook::PlayTask;
use crate::python::{ModulePayload, Union};
use crate::vars::HostVars;

use super::argspec::raw_params_refusal;
use super::plugin::{protocol_task, with_modes};

/// One step's items, as the tasks to send and the results of those that cannot be sent at all.
///
/// The results go in here rather than at the call site so that they cannot be dropped there: an
/// item that cannot travel and reports nothing leaves the driver with no result for it, and a
/// step with no results at all is a step the run reports **no line for** - it finishes `ok`,
/// counts nothing, and says nothing about a task that never ran. That is this project's worst
/// failure, and this function is where it is kept shut.
pub(in crate::executor) fn step_tasks(
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
pub(in crate::executor) fn action_fqcn(module: &str) -> String {
    if crate::python::is_resolved_name(module) {
        module.to_string()
    } else {
        format!("ansible.builtin.{}", short_name(module))
    }
}

/// The variables of the host a task's module actually runs on: the delegate's when the task has
/// one, the task's own host's otherwise.
///
/// One expression, used for the connection and for the interpreter both, because they have to be
/// the same host: a link opened to the delegate and an interpreter read off the delegating host
/// runs a module on one machine under the Python of another, which is a failure only a path that
/// does not exist on both would show.
pub(in crate::executor) fn running_host_vars<'a>(
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
pub(in crate::executor) fn requested_interpreter<V: crate::vars::HostSettings + ?Sized>(
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
pub(in crate::executor) const DISCOVERED_INTERPRETER: &str = "discovered_interpreter_python";

/// What the warning on screen ends with; the copy a task registers stops before it.
pub(in crate::executor) const DISCOVERY_HELP: &str = "See https://docs.ansible.com/ansible-core/2.19/reference_appendices/interpreter_discovery.html for more information.";

/// Whether the reference discovers the interpreter this task's module runs under, once nobody
/// named one. Measured on ansible-core 2.19.12: every module sent to a host does, but not `raw`
/// and `script` (they hand a line or a file to the shell) and not `fetch`, `reboot` and
/// `wait_for_connection`, whose result is not a module's. `add_host`, `group_by` and
/// `set_stats` send no module at all.
pub(in crate::executor) fn discovers_interpreter<V: crate::vars::HostSettings + ?Sized>(
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
pub(in crate::executor) fn silent_discovery<V: crate::vars::HostSettings + ?Sized>(
    vars: &V,
) -> bool {
    crate::vars::host_setting(vars, "ansible_python_interpreter")
        .and_then(Value::as_str)
        .is_some_and(|asked| matches!(asked.trim(), "auto_silent" | "auto_legacy_silent"))
}

/// Puts the discovered interpreter in the result's `ansible_facts`, where the reference returns
/// it and where [`super::record::record_facts`] makes it the host's own fact.
pub(in crate::executor) fn stamp_discovery(result: &mut TaskResult, interpreter: &str) {
    let facts = result
        .0
        .entry("ansible_facts")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(facts) = facts {
        facts.insert(DISCOVERED_INTERPRETER.into(), json!(interpreter));
    }
}

/// The warning for an interpreter found rather than named, as a task registers it.
pub(in crate::executor) fn discovery_warning(host: &str, interpreter: &str) -> String {
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
pub(in crate::executor) fn chosen_interpreter(
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
pub(in crate::executor) fn python_for<'a>(
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
/// module a plugin can pick is in the union by construction (`python::reach::modules_to_build`),
/// so reaching that arm is a controller bug, and `service` is a name the agent would take for a
/// module it runs itself. The agent's own `command`, `raw` and `shell` are the exception, sent
/// with no payload as a playbook's would be: `reboot` runs its commands through `raw`.
pub(super) fn sub_task(
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
pub(super) fn unreachable_ending(
    result: TaskResult,
) -> Result<TaskResult, Result<BatchOutcome, String>> {
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
