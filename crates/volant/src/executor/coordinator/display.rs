// SPDX-License-Identifier: GPL-3.0-or-later
//! Banners and result lines.

use std::collections::HashMap;

use tokio::sync::watch;

use crate::compile::{Step, StepKind};
use crate::render::Renderer;
use crate::stats::{Outcome, Stats};

use super::publish::task_name;
use super::types::{Event, Progress};
use crate::executor::RunState;
use crate::executor::prepare::PlayPlan;
/// The header of step `index`, for the drain paths that reach a banner after the step loop has
/// moved on. The step's name is rendered against the first host of the batch, which is the same
/// choice the step loop makes for a header a later host would have printed - and a host of this
/// batch rather than of the play, so a name naming its own host names one that ran.
pub(super) fn header_for(
    index: usize,
    plan: &PlayPlan,
    play_hosts: &[String],
    progress_tx: &watch::Sender<Progress>,
    state: &RunState,
    names: &HashMap<(String, usize), String>,
    out: &mut Renderer,
) {
    let compiled = plan.steps();
    let Some(step) = compiled.steps.get(index) else {
        return;
    };
    let Some(host) = play_hosts.first() else {
        return;
    };
    let live = progress_tx.borrow().clone();
    let spoken = names.get(&(host.clone(), index));
    header(
        step,
        &task_name(step, host, plan, &live, state, spoken),
        out,
    );
}

/// The banner one step gets: a handler says so, measured - `RUNNING HANDLER [second handler]`
/// where an ordinary task says `TASK [...]`.
pub(super) fn header(step: &Step, name: &str, out: &mut Renderer) {
    match step.kind {
        StepKind::Handler(_) => out.handler(name),
        _ => out.task(name),
    }
    if let Some(origin) = &step.task.origin {
        out.task_path(origin);
    }
}

/// The banner of step `index`, unless the last line printed already belonged to it: under `free`
/// and `host_pinned` a banner marks a switch from one task to another, and a run of lines for the
/// same task share the one.
#[expect(
    clippy::too_many_arguments,
    reason = "what a banner reads: the step, the host it is named against, the batch and the terminal"
)]
pub(super) fn switch_header(
    last: &mut Option<usize>,
    index: usize,
    host: &str,
    plan: &PlayPlan,
    progress_tx: &watch::Sender<Progress>,
    state: &RunState,
    names: &HashMap<(String, usize), String>,
    out: &mut Renderer,
) {
    if *last == Some(index) {
        return;
    }
    *last = Some(index);
    let compiled = plan.steps();
    let Some(step) = compiled.steps.get(index) else {
        return;
    };
    let live = progress_tx.borrow().clone();
    let spoken = names.get(&(host.to_string(), index));
    header(
        step,
        &task_name(step, host, plan, &live, state, spoken),
        out,
    );
}

/// Puts one result line in the recap and on the terminal. Anything but an `Event::Result` is
/// ignored, so both event loops can hand it whatever they hold.
pub(super) fn report_result(event: Event, stats: &mut Stats, out: &mut Renderer) {
    if let Event::Retrying {
        host,
        name,
        left,
        result,
        censored,
        ..
    } = &event
    {
        out.retrying(host, name, *left, result, *censored);
        return;
    }
    if let Event::Warning {
        message, censored, ..
    } = &event
    {
        out.warning(message, *censored);
        return;
    }
    if let Event::Display { line, debug, .. } = &event {
        if *debug {
            out.debug(line);
        } else {
            out.display(line);
        }
        return;
    }
    if let Event::UnreachableIgnored {
        host,
        msg,
        censored,
        delegate,
        origin,
        ..
    } = &event
    {
        stats.record(host, Outcome::Ignored, false);
        out.error(&crate::render::ErrorBlock::unreachable(
            msg,
            origin.as_ref(),
            *censored,
        ));
        out.unreachable(host, msg, *censored, delegate.as_deref());
        out.ignoring();
        return;
    }
    let Event::Result {
        host,
        outcome,
        result,
        label,
        dump,
        show,
        counts,
        censored,
        delegate,
        error,
        ..
    } = event
    else {
        return;
    };
    // The recap counts the host the task was written for, never the delegate: measured on
    // ansible-core 2.19.12, a play over h1 and h2 delegating every task to h3 recaps those two
    // and h3 is not in the recap at all.
    if counts {
        stats.record(&host, outcome, result.changed());
    }
    if show {
        if let Some(error) = &error {
            out.error(error);
        }
        out.result(
            &host,
            outcome,
            &result,
            label.as_deref(),
            dump,
            censored,
            delegate.as_deref(),
        );
    } else if outcome == Outcome::Ignored {
        // A loop's aggregate prints no line of its own, but the failure it swallowed still has
        // to say so.
        out.ignoring();
    }
}
