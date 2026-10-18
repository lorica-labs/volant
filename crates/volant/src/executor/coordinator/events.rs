// SPDX-License-Identifier: GPL-3.0-or-later
//! Which events reach the terminal or the debugger, and writing one the moment it arrives.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Map, Value};
use tokio::sync::watch;

use crate::render::Renderer;
use crate::stats::{Outcome, Stats};

use super::display::{report_result, switch_header};
use super::publish::task_name;
use super::types::{Event, Progress};
use crate::executor::debugger;
use crate::executor::prepare::{PlayPlan, Strategy, host_vars};
use crate::executor::{RunOptions, RunState};
/// Whether an event puts a line on the terminal, which is what a step's header goes in front
/// of. A result nobody shows still speaks when `ignore_errors` swallowed a failure: the
/// `...ignoring` line is the whole display of that step for that host.
pub(super) fn shows_a_line(event: &Event) -> bool {
    match event {
        Event::Result { show, outcome, .. } => *show || *outcome == Outcome::Ignored,
        // A retry line is the first thing a retried task prints, and the reference shows the
        // task's banner above it: a task whose every attempt is still to come has no result yet.
        Event::Banner { .. }
        | Event::Retrying { .. }
        | Event::Display { .. }
        | Event::UnreachableIgnored { .. } => true,
        // `Event::Warning` falls here with the rest: a warning goes to stderr, beside the
        // display rather than in it, so it never conjures the banner of a step that shows
        // nothing on stdout.
        _ => false,
    }
}

/// Writes one event the moment it arrives, behind the banner its task needs.
#[expect(
    clippy::too_many_arguments,
    reason = "what a banner and a line read: the batch, the books and the terminal"
)]
pub(super) fn show_now(
    event: Event,
    last: &mut Option<usize>,
    plan: &PlayPlan,
    progress_tx: &watch::Sender<Progress>,
    state: &RunState,
    names: &HashMap<(String, usize), String>,
    stats: &mut Stats,
    out: &mut Renderer,
) {
    let at = match &event {
        Event::Result { host, index, .. }
        | Event::Retrying { host, index, .. }
        | Event::Display { host, index, .. }
        | Event::UnreachableIgnored { host, index, .. }
        | Event::Banner { host, index } => Some((host.clone(), *index)),
        _ => None,
    };
    if let Some((host, index)) = at
        && shows_a_line(&event)
    {
        switch_header(last, index, &host, plan, progress_tx, state, names, out);
    }
    report_result(event, stats, out);
}

/// Whether step `index` opens the prompt behind a result of `kind`.
pub(super) fn opens_at(plan: &PlayPlan, index: usize, kind: debugger::Kind) -> bool {
    let by_default = plan.strategy == Strategy::Debug || debugger::enabled_by_environment();
    plan.steps()
        .steps
        .get(index)
        .is_some_and(|step| debugger::opens(step.task.debugger, by_default, kind))
}

/// What an event asks of the debugger: the result of a whole task, loop items left out, when
/// its step opens the prompt for it.
pub(super) fn debug_ask(
    event: &Event,
    plan: &PlayPlan,
) -> Option<(debugger::Kind, Map<String, Value>)> {
    match event {
        Event::Result {
            index,
            outcome,
            result,
            counts: true,
            ..
        } => {
            let kind = debugger::Kind::of(*outcome);
            opens_at(plan, *index, kind).then(|| (kind, result.0.clone()))
        }
        _ => None,
    }
}

/// What an unreachable host asks of the debugger: its result reads `unreachable`.
pub(super) fn unreachable_ask() -> (debugger::Kind, Map<String, Value>) {
    (
        debugger::Kind::Unreachable,
        Map::from_iter([("unreachable".to_string(), Value::Bool(true))]),
    )
}

/// Opens the debugger prompt behind a result of step `index` on `host`, when the step's
/// `debugger`, the play's strategy or the environment says to. The coordinator is the one that
/// waits, which is what the reference does too: nothing else is written while the prompt is
/// open. Nothing is asked once the run is stopping, an earlier prompt's `q` included.
#[expect(
    clippy::too_many_arguments,
    reason = "what a prompt reads: the step, the batch, the books and the terminal"
)]
pub(super) async fn debugging(
    (kind, result): (debugger::Kind, Map<String, Value>),
    host: &str,
    index: usize,
    plan: &PlayPlan,
    progress_tx: &watch::Sender<Progress>,
    state: &RunState,
    names: &HashMap<(String, usize), String>,
    out: &mut Renderer,
    options: &RunOptions,
) {
    if *options.stop.borrow() || !opens_at(plan, index, kind) {
        return;
    }
    let compiled = plan.steps();
    let Some(step) = compiled.steps.get(index) else {
        return;
    };
    let live = progress_tx.borrow().clone();
    let spoken = names.get(&(host.to_string(), index));
    let title = task_name(step, host, plan, &live, state, spoken);
    let playbook_dir = state
        .vars
        .lock()
        .expect("vars lock")
        .playbook_dir()
        .to_path_buf();
    let vars = host_vars(
        host,
        plan,
        &step.task.vars,
        step.role,
        step.include_params.as_deref(),
        &live,
        &state.templar,
        &state.vars,
        &step.origin,
        &playbook_dir,
    );
    let mut session = debugger::Session {
        host: host.to_string(),
        title,
        name: step.task.name.clone(),
        action: step.task.module.clone(),
        raw_args: step.task.args.clone(),
        args: step.task.args.clone(),
        vars,
        result,
        templar: Arc::clone(&state.templar),
        locals: Map::new(),
        last: String::new(),
    };
    // The task ran with these arguments rendered; one that no longer renders shows as written.
    if let Ok(args) = session.render_args() {
        session.args = args;
    }
    debugger::ask(&mut session, out, &options.abort, &mut options.stop.clone()).await;
}
