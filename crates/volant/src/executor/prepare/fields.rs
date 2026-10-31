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
    /// Set for a later item of a loop `break_when` may end: nothing past its `when` is rendered.
    pub(super) deferred: bool,
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
            deferred: false,
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
/// cannot be evaluated yields to it, and it is raised once the task would run. `at` says where
/// the profile notes the texts this item renders.
pub(super) fn render_fields(
    task: &PlayTask,
    plan: &PlayPlan,
    templar: &Templar,
    vars: &HostVars,
    at: &Noting<'_>,
    loop_error: &mut Option<TemplateError>,
    warnings: &mut Vec<String>,
) -> Result<Fields, TemplateError> {
    let mut skipped = None;
    crate::vault::begin_failures();
    for condition in &task.when {
        crate::profile::count(crate::profile::Counter::RenderWhen, 1);
        at.note(templar, condition, true);
        // A conditional that cannot be evaluated yields to a kept loop error, as there.
        let (held, read) = crate::template::watch_play_hosts(|| templar.condition(condition, vars));
        if read && condition.contains("play_hosts") {
            crate::playbook::deprecate_play_hosts(task.value_origins.get("when"));
        }
        // Measured on ansible-core 2.19.12: a condition that is one template is rendered first,
        // and deprecated when it renders to anything but an expression.
        if held.is_ok()
            && crate::playbook::possibly_all_template(condition.trim())
            && templar
                .render(condition.trim(), vars)
                .is_ok_and(|rendered| !rendered.is_string())
        {
            crate::playbook::deprecate_conditional_template(task.value_origins.get("when"));
        }
        let holds = match held {
            // A condition that reads an unknown value takes the recorded branch.
            Err(e) if crate::template::unknown::reason_in(&e.0).is_some() => {
                match crate::shadow::replay::recorded_skip(task, at.item) {
                    Some(skipped) => !skipped,
                    None => return Err(e),
                }
            }
            held => held.map_err(|e| super::undefined::when_error(e, loop_error.as_ref()))?,
        };
        if !holds {
            let mut r = Map::new();
            r.insert("changed".into(), json!(false));
            r.insert("skipped".into(), json!(true));
            r.insert("skip_reason".into(), json!("Conditional result was False"));
            r.insert(
                "false_condition".into(),
                super::item::false_condition(condition),
            );
            skipped = Some(TaskResult(r));
            break;
        }
    }
    if skipped.is_none()
        && let Some(e) = loop_error.take()
    {
        return Err(e);
    }
    // An item a `when` left out renders nothing more, and a later item of a loop `break_when`
    // may end renders the rest when the loop reaches it: see `render_deferred`.
    if skipped.is_some() || at.defer {
        return Ok(Fields {
            deferred: skipped.is_none(),
            skipped,
            check_mode: flag_for("check_mode", task.check_mode.as_ref(), vars, templar, true)?,
            diff: flag_for("diff", task.diff.as_ref(), vars, templar, true)?,
            ..Fields::failed(TaskResult::default())
        });
    }
    render_rest(task, plan, templar, vars, at, warnings)
}

/// The arguments and the keywords of an item that runs, rendered after its `when` held.
pub(super) fn render_rest(
    task: &PlayTask,
    plan: &PlayPlan,
    templar: &Templar,
    vars: &HostVars,
    at: &Noting<'_>,
    warnings: &mut Vec<String>,
) -> Result<Fields, TemplateError> {
    crate::profile::count(crate::profile::Counter::RenderArgs, 1);
    if crate::profile::tracks_repeats() {
        for value in task.args.values() {
            note_texts(at, templar, value);
        }
    }
    // The reference warns once for each template that renders to arguments, at the template.
    for (_, at) in &task.splats {
        let place = at
            .as_ref()
            .map(|at| {
                format!(
                    "
{}
",
                    crate::render::source_context(at)
                )
            })
            .unwrap_or_default();
        warnings.push(format!(
            "Using a template for task args is unsafe in some situations (see https://docs.ansible.com/ansible/devel/reference_appendices/faq.html#argsplat-unsafe).{place}"
        ));
    }
    let (args, args_untrusted) = render_args(task, plan, templar, vars)?;
    // Per item, against the item's own variables: the reference evaluates the conditional
    // before it renders the task's keywords.
    let ignore_errors = match &task.ignore_errors {
        Some(Flag::Template(raw)) => Some(ignore_errors_for(raw, vars, templar)?),
        _ => None,
    };
    let check_mode = flag_for("check_mode", task.check_mode.as_ref(), vars, templar, false)?;
    let diff = flag_for("diff", task.diff.as_ref(), vars, templar, false)?;
    Ok(Fields {
        skipped: None,
        deferred: false,
        args,
        args_untrusted,
        ignore_errors,
        check_mode,
        diff,
        environment: environment_for(task, vars, templar, warnings)?,
    })
}

/// Where the profile notes what one item renders.
pub(super) struct Noting<'a> {
    /// This item's prepare, numbered across the run: see [`crate::profile::next_prepare`].
    pub(super) prepare: u64,
    pub(super) host: &'a str,
    /// A loop item after the first.
    pub(super) later_item: bool,
    /// The item's index, 0 for a task with no loop.
    pub(super) item: usize,
    /// The names an item binds: a text that reads none of them renders alike for every item.
    pub(super) loop_names: [&'a str; 3],
    /// A later item of a loop `break_when` may end: see [`Fields::deferred`].
    pub(super) defer: bool,
}

impl Noting<'_> {
    /// Notes that this item renders `text`, a bare condition when `condition`.
    fn note(&self, templar: &Templar, text: &str, condition: bool) {
        if !crate::profile::tracks_repeats() {
            return;
        }
        crate::profile::note_render(self.prepare, self.host, text);
        if self.later_item {
            let source = if condition && !Templar::is_template(text) {
                std::borrow::Cow::Owned(format!("{{{{ {text} }}}}"))
            } else {
                std::borrow::Cow::Borrowed(text)
            };
            if !templar.may_read(&source, &self.loop_names) {
                crate::profile::note_invariant(text);
            }
        }
    }
}

/// Notes every template in `value` as a text this item renders, for the profile.
fn note_texts(at: &Noting<'_>, templar: &Templar, value: &Value) {
    match value {
        Value::String(text) if Templar::is_template(text) => at.note(templar, text, false),
        Value::Array(items) => items.iter().for_each(|item| note_texts(at, templar, item)),
        Value::Object(map) => map.values().for_each(|item| note_texts(at, templar, item)),
        _ => {}
    }
}

/// The arguments of one item, rendered argument by argument rather than as one value, because a
/// few arguments are read back as engine input rather than as data - the name a `debug: var:`
/// compiles - and the render is the only place that can say which of them came from a host.
/// The reference's warning at each argument whose template passes a lookup a constant string
/// that holds a template.
fn warn_embedded(task: &PlayTask, raw: &Map<String, Value>) {
    for (key, value) in raw {
        if value.as_str().is_some_and(crate::template::embeds) {
            let place = task
                .value_origins
                .get(&format!("args.{key}"))
                .map(|origin| {
                    format!(
                        "{}

",
                        crate::render::source_context(origin)
                    )
                })
                .unwrap_or_default();
            crate::render::warn_once(&format!(
                "[WARNING]: {}
{place}{}

",
                crate::template::EMBEDDED_WARNING,
                crate::template::EMBEDDED_HELP
            ));
        }
    }
}

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
    let (rendered, read) =
        crate::template::watch_play_hosts(|| templar.render_map_tainted(&raw, vars));
    // Placed at each argument that names it: one read through another variable is not placed.
    if read {
        for (key, value) in raw.iter() {
            if value
                .as_str()
                .is_some_and(|text| text.contains("play_hosts"))
            {
                crate::playbook::deprecate_play_hosts(
                    task.value_origins.get(&format!("args.{key}")),
                );
            }
        }
    }
    warn_embedded(task, &raw);
    let (mut map, mut untrusted) = rendered.map_err(finalizing)?;
    // The splats lie under the arguments written as such, each over the one before it.
    let mut splatted: BTreeMap<String, (Value, bool)> = BTreeMap::new();
    for (template, _) in &task.splats {
        let (value, tainted) = templar
            .render_value_tainted(&Value::String(template.clone()), vars)
            .map_err(|err| splat_failure(task, &super::undefined_name(template, err.0)))?;
        let Value::Object(fields) = value else {
            return Err(splat_failure(
                task,
                &format!(
                    "Task args must resolve to a 'dict' not '{}'.",
                    crate::executor::python_type(&value)
                ),
            ));
        };
        for (key, value) in fields {
            splatted.insert(key, (value, tainted));
        }
    }
    for (key, (value, tainted)) in splatted {
        if !map.contains_key(&key) {
            if tainted {
                untrusted.insert(key.clone());
            }
            map.insert(key, value);
        }
    }
    if task.name == "Gathering Facts" && task.module == "ansible.legacy.setup" {
        check_gather_subset(raw.get("gather_subset"), map.get("gather_subset"))?;
    }
    // The task's `module_defaults` lie under its own arguments, each rendered once on its own: a
    // default is a template like an argument, and may be a whole template that renders to a
    // mapping. A task's argument wins, `omit` included.
    if !task.module_defaults.is_empty() {
        let mut from_default = BTreeSet::new();
        let resolved = plan
            .python
            .as_deref()
            .and_then(|u| u.resolved_fqcn(&task.module));
        let mut defaults = crate::collection::defaults_for(
            &task.module,
            resolved.as_deref(),
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

/// The reference's error for an argument splat that did not render to arguments: the
/// finalization of the task's arguments failed, for the sentence given.
fn splat_failure(task: &PlayTask, why: &str) -> TemplateError {
    TemplateError(format!(
        "{FINALIZING}'{}' failed: {why}",
        crate::executor::run::action_fqcn(&task.module)
    ))
}
