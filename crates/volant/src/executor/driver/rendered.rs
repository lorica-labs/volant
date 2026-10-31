// SPDX-License-Identifier: GPL-3.0-or-later
//! The steps of a batch as one host runs them, with the keywords written as templates rendered.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::mpsc;

use crate::compile::{Compiled, Step, StepKind};
use crate::template::{Templar, TemplateError};
use crate::vars::VarStore;

use super::super::coordinator::{Event, Progress};
use super::super::prepare::{PlayPlan, Prepared, hide_after_failure, resolve};

/// The steps of the batch in hand whose keywords were rendered for this host, by position. A
/// step with nothing to render is not in it.
pub(super) type Rendered = HashMap<usize, Step>;

/// The `meta` actions whose `when` the reference does not read: it warns that the task does not
/// support one and runs the action all the same.
pub(super) const NO_WHEN: [&str; 3] = ["noop", "refresh_inventory", "reset_connection"];

/// `step` as it is prepared: a `NO_WHEN` action loses its `when`, since the reference never
/// reads it, so a condition that cannot be evaluated must not fail the host either.
pub(super) fn read_as(step: &Step) -> Cow<'_, Step> {
    if step.kind != StepKind::Meta || !NO_WHEN.contains(&crate::compile::meta_action(&step.task)) {
        return Cow::Borrowed(step);
    }
    let mut copy = step.clone();
    copy.task.when.clear();
    Cow::Owned(copy)
}

/// The step at `index` as this host runs it: the copy its templated keywords were rendered into
/// when there is one, the compiled step otherwise.
pub(super) fn step_at<'a>(c: &'a Compiled, rendered: &'a Rendered, index: usize) -> &'a Step {
    rendered.get(&index).unwrap_or(&c.steps[index])
}

/// Renders the templated keywords of `step` for this host, once the task's own `when` has held
/// for it, and keeps the copy that holds what they rendered to in `rendered`. What the reference
/// prints before the task goes to `tx`. A value that is not what its keyword needs turns the
/// preparation into the failure of the task for this host, the way an argument that does not
/// render does.
pub(super) async fn render_keywords(
    step: &Step,
    index: usize,
    host: &str,
    prepared: Result<Prepared, TemplateError>,
    rendered: &mut Rendered,
    (plan, live, store): (&PlayPlan, &Progress, &Mutex<VarStore>),
    (templar, tx): (&Templar, &mpsc::Sender<Event>),
) -> Result<Prepared, TemplateError> {
    if !step.task.deferred.any() {
        return prepared;
    }
    rendered.remove(&index);
    let Ok(ready) = &prepared else {
        // The task failed before its keywords could be read. Its `no_log` still applies.
        if let Some(copy) = hide_after_failure(step, host, plan, live, templar, store) {
            rendered.insert(index, *copy);
        }
        return prepared;
    };
    if matches!(ready, Prepared::Skipped(_)) {
        return prepared;
    }
    let items: Vec<_> = ready
        .items()
        .iter()
        .filter(|i| i.skipped.is_none())
        .map(|i| &i.vars)
        .collect();
    let resolution = resolve(step, &items, templar);
    for message in resolution.warnings {
        let _ = tx
            .send(Event::Warning {
                host: host.to_string(),
                index,
                message,
                censored: false,
            })
            .await;
    }
    if let Some(copy) = resolution.step {
        rendered.insert(index, *copy);
    } else if items.is_empty()
        // Every item failed before its keywords could be read: their `no_log` still applies,
        // measured on ansible-core 2.19.12 for a loop whose arguments do not render.
        && let Some(copy) = hide_after_failure(step, host, plan, live, templar, store)
    {
        rendered.insert(index, *copy);
    }
    match resolution.error {
        Some(err) => Err(err),
        None => prepared,
    }
}

/// `step` with the module its `action:` template renders to for this host, as the reference
/// resolves the action when the task runs: `action: "{{ module_to_run }} msg=hi"`. `None` when
/// the statement named its module outright, or when the template does not render to a name, which
/// leaves the task to fail on the word it wrote.
pub(super) fn with_module(
    step: &Step,
    host: &str,
    plan: &PlayPlan,
    live: &Progress,
    templar: &Templar,
    store: &Mutex<VarStore>,
) -> Option<Step> {
    let (template, _) = step.task.deferred.module.as_ref()?;
    let playbook_dir = store
        .lock()
        .expect("vars lock")
        .playbook_dir()
        .to_path_buf();
    let vars = super::super::prepare::host_vars(
        host,
        plan,
        &step.task.vars,
        step.role,
        step.include_params.as_deref(),
        live,
        templar,
        store,
        &step.origin,
        &playbook_dir,
    );
    let name = templar.render(template, &vars).ok()?;
    let name = name.as_str().map(str::trim).filter(|n| !n.is_empty())?;
    let task = crate::playbook::with_module(&step.task, name).ok()?;
    Some(Step {
        task,
        ..step.clone()
    })
}
