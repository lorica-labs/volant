// SPDX-License-Identifier: GPL-3.0-or-later
//! Which events reach the terminal or the debugger, and writing one the moment it arrives.

use std::collections::HashMap;

use tokio::sync::watch;

use crate::render::Renderer;
use crate::stats::{Outcome, Stats};

use super::display::{report_result, switch_header};
use super::publish::task_name;
use super::types::{Event, Progress};
use crate::executor::debugger;
use crate::executor::prepare::{PlayPlan, Strategy};
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

/// What an event says to the debugger: the result of a whole task, loop items left out.
pub(super) fn debug_kind(event: &Event) -> Option<debugger::Kind> {
    match event {
        Event::Result {
            outcome,
            counts: true,
            ..
        } => Some(debugger::Kind::of(*outcome)),
        _ => None,
    }
}

/// Opens the debugger prompt behind a result of step `index` on `host`, when the step's
/// `debugger` or the play's strategy says to. The coordinator is the one that waits, which is
/// what the reference does too: nothing else is written while the prompt is open.
#[expect(
    clippy::too_many_arguments,
    reason = "what a prompt reads: the step, the batch, the books and the terminal"
)]
pub(super) async fn debugging(
    kind: debugger::Kind,
    host: &str,
    index: usize,
    plan: &PlayPlan,
    progress_tx: &watch::Sender<Progress>,
    state: &RunState,
    names: &HashMap<(String, usize), String>,
    out: &mut Renderer,
    options: &RunOptions,
) {
    let compiled = plan.steps();
    let Some(step) = compiled.steps.get(index) else {
        return;
    };
    if !debugger::opens(step.task.debugger, plan.strategy == Strategy::Debug, kind) {
        return;
    }
    let live = progress_tx.borrow().clone();
    let spoken = names.get(&(host.to_string(), index));
    let name = task_name(step, host, plan, &live, state, spoken);
    debugger::ask(host, &name, out, &options.abort, &mut options.stop.clone()).await;
}
