// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeSet;
use std::io::IsTerminal as _;
use std::sync::Mutex;

use serde_json::{Map, Value, json};
use tokio::sync::watch;
use volant_protocol::TaskResult;
use volant_protocol::modules::{FAIL, short_name};

use crate::compile::Step;
use crate::executor::prepare::Item;
use crate::playbook::PlayTask;
use crate::template::Templar;
use crate::vars::{VarStore, load_vars_file};

use super::argspec::{action_name, unchanged_failure, unknown_args, validate_argument_spec};
use super::controller::{assert_module, pause};
use super::output::summarise_exception;

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
/// [`super::record::record_local_facts`].
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
pub(in crate::executor) async fn run_local(
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
            async_val: task.async_val,
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
pub(super) fn set_fact_data(task: &PlayTask, item: &Item, templar: &Templar) -> BTreeSet<String> {
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
