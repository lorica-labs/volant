// SPDX-License-Identifier: GPL-3.0-or-later
//! Banners and result lines.

use std::collections::HashMap;

use tokio::sync::watch;

use crate::compile::{Step, StepKind};
use crate::render::Renderer;
use crate::stats::{Outcome, Stats};

use super::publish::task_name;
use super::types::{Event, Progress, Shade};
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
    out.set_action(&step.task.module);
    out.task_started(name, &step.task.module, step.task.origin.as_ref());
    match step.kind {
        StepKind::Handler(_) => out.handler(name),
        _ => out.task(name),
    }
    if let Some(origin) = &step.task.origin {
        out.task_path(origin);
    }
    // `profile_tasks` keys a task by its uuid, which a later `serial` batch runs again: the
    // step's place in the play stands for it.
    let path = step.task.origin.as_ref().map(|origin| {
        let file = std::path::absolute(&*origin.path)
            .unwrap_or_else(|_| std::path::PathBuf::from(&*origin.path));
        format!("{}:{}", file.display(), origin.line)
    });
    // `junit` names a task with its arguments as the task wrote them, `k=v` joined by `, `.
    let args = (!step.task.censors()).then(|| {
        step.task
            .args
            .iter()
            .map(|(key, value)| match value {
                serde_json::Value::String(text) => format!("{key}={text}"),
                other => format!("{key}={}", crate::playbook::python_repr(other)),
            })
            .collect::<Vec<_>>()
            .join(", ")
    });
    out.task_began(&crate::render::Begun {
        key: &format!("{}#{name}", path.as_deref().unwrap_or("")),
        name,
        path,
        args: args.as_deref(),
        action: &step.task.module,
    });
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

/// Files what the coordinator learns from a result when it reads it, and hands the event back: a
/// failed task that counts fails the host for the exit code. Done on arrival, not when the line
/// is shown, so that a `clear_host_errors` read in between forgives it. A result held at the
/// prompt counts nothing here, and the answer does it.
pub(super) fn arrived(event: Event, stats: &mut Stats) -> Event {
    if let Event::Result {
        host,
        outcome: Outcome::Failed,
        counts: true,
        ..
    } = &event
    {
        stats.fail(host);
    }
    event
}

/// A line a step printed as it ran. The lines under no banner are the `default` callback's own
/// `NOTIFIED HANDLER` lines.
fn display(line: &str, shade: Shade, banner: bool, out: &mut Renderer) {
    if !banner && out.shows_every_result() {
        return;
    }
    match shade {
        Shade::Plain => out.display(line),
        Shade::Debug => out.debug(line),
        Shade::Verbose(level) => out.verbose(level, line),
    }
}

/// Puts one result line in the recap and on the terminal. Anything but an `Event::Result` is
/// ignored, so both event loops can hand it whatever they hold.
#[expect(
    clippy::too_many_lines,
    reason = "one result's whole path to the recap, the callbacks and the terminal, in order"
)]
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
        // The `oneline` callback shows none of the notes a result carries.
        if !out.is_oneline() {
            out.warning(message, *censored);
        }
        return;
    }
    if let Event::Display {
        line,
        shade,
        banner,
        ..
    } = &event
    {
        display(line, *shade, *banner, out);
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
    watch(&event, stats);
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
        task_origin,
        ..
    } = event
    else {
        return;
    };
    // The recap counts the host the task was written for, never the delegate: measured on
    // ansible-core 2.19.12, a play over h1 and h2 delegating every task to h3 recaps those two
    // and h3 is not in the recap at all.
    if counts {
        stats.tally(&host, outcome, result.changed());
    }
    out.result_heard(
        &host,
        outcome,
        &result,
        censored,
        error.as_ref(),
        label.is_some(),
    );
    // `minimal` and `oneline` print every result the task hands over, a loop's aggregate too.
    if show || (label.is_none() && out.shows_every_result()) {
        if let Some(error) = &error {
            out.error(error);
        }
        // A loop item has its own `failed:` line and the reference gives it no path.
        if label.is_none()
            && matches!(
                outcome,
                Outcome::Failed | Outcome::Rescued | Outcome::Ignored
            )
        {
            out.failure_path(task_origin.as_ref());
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
    } else {
        // A failure with no line of its own can still have its block.
        if let Some(error) = &error {
            out.error(error);
        }
        // A loop's aggregate prints no line of its own, but the failure it swallowed still has
        // to say so.
        if outcome == Outcome::Ignored {
            out.ignoring();
        }
    }
}

/// Shows a result line to the replay this run answers for, under `volant shadow replay`.
fn watch(event: &Event, stats: &Stats) {
    if let (
        Some(replay),
        Event::Result {
            host,
            index,
            outcome,
            result,
            counts,
            task_origin,
            ..
        },
    ) = (&stats.replay, event)
    {
        replay.observe(
            host,
            task_origin.as_ref(),
            *index,
            !counts,
            *outcome,
            result,
        );
    }
}
