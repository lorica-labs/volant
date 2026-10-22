// SPDX-License-Identifier: GPL-3.0-or-later
//! The fields of one item rendered: its `when`, its arguments and the keywords read per item.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;
use volant_protocol::modules::short_name;

use crate::playbook::{Flag, PlayTask};
use crate::template::{Templar, TemplateError};
use crate::vars::HostVars;

use super::escalation::remove_omit;
use super::item::{FINALIZING, check_gather_subset, environment_for, flag_for, ignore_errors_for};
use super::plan::PlayPlan;

/// What one item renders before it runs.
pub(super) struct Fields {
    /// Set when `when` was false, or when a field failed to render: the result to report.
    pub(super) skipped: Option<TaskResult>,
    pub(super) args: Map<String, Value>,
    pub(super) args_untrusted: BTreeSet<String>,
    pub(super) ignore_errors: Option<bool>,
    pub(super) check_mode: Option<bool>,
    pub(super) diff: Option<bool>,
    pub(super) environment: BTreeMap<String, String>,
}

impl Fields {
    /// An item that failed to render: nothing of it runs, and `result` is what it reports.
    pub(super) fn failed(result: TaskResult) -> Self {
        Self {
            skipped: Some(result),
            args: Map::new(),
            args_untrusted: BTreeSet::new(),
            ignore_errors: None,
            check_mode: None,
            diff: None,
            environment: BTreeMap::new(),
        }
    }
}

/// The `when`, the arguments and the per-item keywords of one item, against its variables.
///
/// `loop_error` is a loop error kept until the task's `when` has decided: a conditional that
/// cannot be evaluated yields to it, and it is raised once the task would run.
pub(super) fn render_fields(
    task: &PlayTask,
    plan: &PlayPlan,
    templar: &Templar,
    vars: &HostVars,
    loop_error: &mut Option<TemplateError>,
    warnings: &mut Vec<String>,
) -> Result<Fields, TemplateError> {
    let mut skipped = None;
    for condition in &task.when {
        // A conditional that cannot be evaluated yields to a kept loop error, as there.
        let holds = templar
            .condition(condition, vars)
            .map_err(|e| loop_error.clone().unwrap_or(e))?;
        if !holds {
            let mut r = Map::new();
            r.insert("changed".into(), json!(false));
            r.insert("skipped".into(), json!(true));
            r.insert("skip_reason".into(), json!("Conditional result was False"));
            r.insert("false_condition".into(), json!(condition));
            skipped = Some(TaskResult(r));
            break;
        }
    }
    if skipped.is_none()
        && let Some(e) = loop_error.take()
    {
        return Err(e);
    }
    let (args, args_untrusted) = if skipped.is_some() {
        (Map::new(), BTreeSet::new())
    } else {
        render_args(task, plan, templar, vars)?
    };
    // Per item, against the item's own variables, and never for an item a `when` left out:
    // the reference evaluates the conditional before it renders the task's keywords.
    let ignore_errors = match (&task.ignore_errors, &skipped) {
        (Some(Flag::Template(raw)), None) => Some(ignore_errors_for(raw, vars, templar)?),
        _ => None,
    };
    let check_mode = flag_for(
        "check_mode",
        task.check_mode.as_ref(),
        vars,
        templar,
        skipped.is_some(),
    )?;
    let diff = flag_for("diff", task.diff.as_ref(), vars, templar, skipped.is_some())?;
    // A skipped item runs nothing, so a layer it could not render is not its problem: the
    // reference does not evaluate `environment` for a task a `when` left out.
    let environment = if skipped.is_some() {
        BTreeMap::new()
    } else {
        environment_for(task, vars, templar, warnings)?
    };
    Ok(Fields {
        skipped,
        args,
        args_untrusted,
        ignore_errors,
        check_mode,
        diff,
        environment,
    })
}

/// The arguments of one item, rendered argument by argument rather than as one value, because a
/// few arguments are read back as engine input rather than as data - the name a `debug: var:`
/// compiles - and the render is the only place that can say which of them came from a host.
fn render_args(
    task: &PlayTask,
    plan: &PlayPlan,
    templar: &Templar,
    vars: &HostVars,
) -> Result<(Map<String, Value>, BTreeSet<String>), TemplateError> {
    // `assert` reads `that` off the task as written and evaluates it the way `when` is
    // evaluated, so it is not rendered here: a render error in it would fail the task in the
    // render's words before the conditional could report it, which the reference's
    // `finalize_task_arg` for `assert` does not do.
    let raw = if short_name(&task.module) == "assert" {
        let mut raw = task.args.clone();
        raw.remove("that");
        std::borrow::Cow::Owned(raw)
    } else {
        std::borrow::Cow::Borrowed(&task.args)
    };
    let finalizing = |err: TemplateError| {
        if err.0.starts_with(crate::template::RESOLVING) {
            TemplateError(format!(
                "{FINALIZING}'{}' failed: {}",
                crate::executor::run::action_fqcn(&task.module),
                err.0
            ))
        } else {
            err
        }
    };
    let (mut map, mut untrusted) = templar.render_map_tainted(&raw, vars).map_err(finalizing)?;
    if task.name == "Gathering Facts" && task.module == "ansible.legacy.setup" {
        check_gather_subset(raw.get("gather_subset"), map.get("gather_subset"))?;
    }
    // The task's `module_defaults` lie under its own arguments, each rendered once on its own: a
    // default is a template like an argument, and may be a whole template that renders to a
    // mapping. A task's argument wins, `omit` included.
    if !task.module_defaults.is_empty() {
        let mut from_default = BTreeSet::new();
        let mut defaults = crate::collection::defaults_for(
            &task.module,
            &task.module_defaults,
            &plan.steps().resolved.groups(),
            &mut |key, value| {
                render_default(templar, vars, key, value, &mut from_default)
                    .map_err(|err| finalizing(err).0)
            },
        )
        .map_err(TemplateError)?;
        from_default.retain(|name| !map.contains_key(name));
        untrusted.extend(from_default);
        defaults.extend(std::mem::take(&mut map));
        map = defaults;
    }
    let mut rendered = Value::Object(map);
    remove_omit(&mut rendered);
    let Value::Object(map) = rendered else {
        unreachable!("an object renders to an object")
    };
    Ok((map, untrusted))
}

/// One `module_defaults` value as the mapping it stands for: a mapping with each of its values
/// rendered, or a whole template that renders to one. The names that read something a managed
/// host wrote go into `untrusted`, so the arguments they become are never rendered again.
fn render_default(
    templar: &Templar,
    vars: &HostVars,
    key: &str,
    value: &Value,
    untrusted: &mut BTreeSet<String>,
) -> Result<Map<String, Value>, TemplateError> {
    let not_a_mapping = || TemplateError(format!("module_defaults for '{key}' must be a mapping"));
    match value {
        Value::Null => Ok(Map::new()),
        Value::Object(map) => {
            let (map, tainted) = templar.render_map_tainted(map, vars)?;
            untrusted.extend(tainted);
            Ok(map)
        }
        other => {
            let mut wrapped = Map::new();
            wrapped.insert("value".into(), other.clone());
            let (mut rendered, tainted) = templar.render_map_tainted(&wrapped, vars)?;
            match rendered.remove("value") {
                Some(Value::Object(map)) => {
                    if tainted.contains("value") {
                        untrusted.extend(map.keys().cloned());
                    }
                    Ok(map)
                }
                Some(Value::Null) => Ok(Map::new()),
                _ => Err(not_a_mapping()),
            }
        }
    }
}
