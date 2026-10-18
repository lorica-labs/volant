// SPDX-License-Identifier: GPL-3.0-or-later
//! Which events reach the terminal or the debugger, and writing one the moment it arrives.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Map, Value};
use tokio::sync::{oneshot, watch};

use crate::playbook::PlayTask;
use crate::render::Renderer;
use crate::stats::{Outcome, Stats};

use super::display::{report_result, switch_header};
use super::publish::{Coordinator, task_name};
use super::types::{Event, Progress, Round, Unreached};
use crate::executor::debugger;
use crate::executor::prepare::{PlayPlan, host_vars};
use crate::executor::{RunOptions, RunState};

/// A result a driver holds at the debugger prompt, from its `Event::Debug` to the answer.
pub(super) struct Held {
    pub(super) reply: oneshot::Sender<debugger::Answer>,
    /// Whether one of the task's results failed the host, or failed it into a rescue: what
    /// the batch learns once the operator keeps the result.
    pub(super) lost: bool,
    pub(super) sank: bool,
    /// The counted result, once it arrived: its outcome, its `changed`, and what `result`
    /// reads at the prompt. The recap counts it once the operator keeps it.
    pub(super) counted: Option<(Outcome, bool, Map<String, Value>)>,
    /// Or the host could not be reached: it leaves the play once the operator keeps that.
    pub(super) unreached: Option<Unreached>,
}

impl Held {
    pub(super) fn new(reply: oneshot::Sender<debugger::Answer>) -> Self {
        Held {
            reply,
            lost: false,
            sank: false,
            counted: None,
            unreached: None,
        }
    }

    /// Whether the prompt can open: every result of the task is in.
    pub(super) fn ready(&self) -> bool {
        self.counted.is_some() || self.unreached.is_some()
    }

    /// Takes in one result of the held task: what it would change is kept for the answer, and
    /// the counted one is shown without being counted yet.
    pub(super) fn hold(&mut self, mut event: Event, lost: bool, sank: bool) -> Event {
        self.lost |= lost;
        self.sank |= sank;
        if let Event::Result {
            outcome,
            result,
            counts,
            ..
        } = &mut event
            && *counts
        {
            *counts = false;
            self.counted = Some((*outcome, result.changed(), result.0.clone()));
        }
        event
    }
}

/// What a failed result of step `at` on `host` changes in the batch: the host leaves the live
/// set, and a `run_once` or `any_errors_fatal` step tells the others. Done on arrival, or once
/// the operator answers for a result held at the prompt.
pub(super) fn book(
    plan: &PlayPlan,
    coordinator: &mut Coordinator<'_>,
    round: &mut Round,
    failed_hosts: &mut HashSet<String>,
    (host, at): (&str, usize),
    lost: bool,
    sank: bool,
) {
    let step = plan.steps().steps.get(at).map(|s| s.task.clone());
    // A `run_once` task that failed, whether or not a `rescue` caught it for the host that ran
    // it: measured on ansible-core 2.19.12, the rescue keeps that one host in the play
    // (`rescued=1`, exit 2) and the others still leave. `Ignored` is not a failure and is left
    // out, so a `run_once` under `ignore_errors` carries the whole batch past it.
    let announced = sank && step.as_ref().is_some_and(PlayTask::bypasses_host_loop);
    if announced {
        coordinator.run_once.insert(at, true);
    }
    // A failed result is the host leaving the play, and it arrives before that task's
    // `TaskDone`. Taking it out of the live set here, rather than waiting for its `Finished`, is
    // what lets the next task read the shrunken host list without racing the driver that is
    // shutting down.
    if lost {
        round.failed(failed_hosts, host);
        coordinator.failed_here.insert(host.to_string());
        coordinator.last_failure = coordinator.last_failure.max(Some(at));
    }
    // A failure `any_errors_fatal` covers, rescued or not: the verdict is published with the
    // failing host's change of state, before any other host can see the step finished.
    let fatal = sank && step.as_ref().is_some_and(PlayTask::fails_all);
    if fatal {
        coordinator.fatal.insert(at);
    }
    if lost || announced || fatal {
        coordinator.publish(failed_hosts, &plan.steps().steps);
    }
}

/// What a host that could not be reached at step `at` changes in the batch: the recap counts
/// it, it leaves the play, and an `any_errors_fatal` step takes the others with it.
pub(super) fn unreachable_left(
    plan: &PlayPlan,
    coordinator: &mut Coordinator<'_>,
    round: &mut Round,
    stats: &mut Stats,
    failed_hosts: &mut HashSet<String>,
    host: &str,
    at: Option<usize>,
) {
    stats.unreachable(host);
    round.unreachable(host, at, plan);
    failed_hosts.insert(host.to_string());
    if let Some(at) = at
        && plan
            .steps()
            .steps
            .get(at)
            .is_some_and(|s| s.task.fails_all() && !s.task.ignores_errors())
    {
        coordinator.fatal.insert(at);
    }
    coordinator.publish(failed_hosts, &plan.steps().steps);
}

/// The prompt behind a held result of step `at` on `host`, and the operator's answer sent to
/// its driver. A result the operator runs again is not counted and changes nothing; any other
/// answer counts it, and a host that could not be reached leaves. True for `r`.
#[expect(
    clippy::too_many_arguments,
    reason = "what a prompt reads and what its answer changes: the batch, the books and the terminal"
)]
pub(super) async fn answer(
    held: Held,
    (host, at): (&str, usize),
    plan: &PlayPlan,
    coordinator: &mut Coordinator<'_>,
    round: &mut Round,
    gone: &mut HashSet<String>,
    progress_tx: &watch::Sender<Progress>,
    state: &mut RunState,
    names: &HashMap<(String, usize), String>,
    stats: &mut Stats,
    out: &mut Renderer,
    options: &RunOptions,
) -> bool {
    let asked = match (&held.counted, &held.unreached) {
        (Some((outcome, _, result)), _) => (debugger::Kind::of(*outcome), result.clone()),
        (None, Some(blame)) => {
            blame.show(host, out);
            unreachable_ask()
        }
        // Only a held result that is ready reaches here.
        (None, None) => {
            let _ = held.reply.send(debugger::Answer::Continue);
            return false;
        }
    };
    let answer = debugging(
        asked,
        host,
        at,
        true,
        plan,
        progress_tx,
        state,
        names,
        out,
        options,
    )
    .await;
    let redo = matches!(answer, debugger::Answer::Redo(_));
    if !redo {
        if let Some((outcome, changed, _)) = held.counted {
            stats.record(host, outcome, changed);
            book(
                plan,
                coordinator,
                round,
                &mut state.failed_hosts,
                (host, at),
                held.lost,
                held.sank,
            );
        } else {
            unreachable_left(
                plan,
                coordinator,
                round,
                stats,
                &mut state.failed_hosts,
                host,
                Some(at),
            );
            gone.insert(host.to_string());
        }
    }
    let _ = held.reply.send(answer);
    redo
}
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
    let by_default = debugger::by_default(plan.strategy);
    plan.steps()
        .steps
        .get(index)
        .is_some_and(|step| debugger::opens_for(&step.task, by_default, kind))
}

/// What an event no driver holds asks of the debugger: the result of a whole task, loop items
/// left out, when its step opens the prompt for it. A held result is not counted on arrival, so
/// it never asks here.
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
/// open. Nothing is asked once the run is stopping, an earlier prompt's `q` included, and the
/// answer is then to go on. `waits` says a driver waits on the answer and runs the task again
/// for `r`; behind a result no driver waits on, an include's, `r` opens the prompt again, which
/// is what the reference shows: the include runs again to the same result and asks again.
#[expect(
    clippy::too_many_arguments,
    reason = "what a prompt reads: the step, the batch, the books and the terminal"
)]
pub(super) async fn debugging(
    (kind, result): (debugger::Kind, Map<String, Value>),
    host: &str,
    index: usize,
    waits: bool,
    plan: &PlayPlan,
    progress_tx: &watch::Sender<Progress>,
    state: &RunState,
    names: &HashMap<(String, usize), String>,
    out: &mut Renderer,
    options: &RunOptions,
) -> debugger::Answer {
    if *options.stop.borrow() || !opens_at(plan, index, kind) {
        return debugger::Answer::Continue;
    }
    let compiled = plan.steps();
    let Some(step) = compiled.steps.get(index) else {
        return debugger::Answer::Continue;
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
        ran: Map::new(),
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
    session.ran.clone_from(&session.args);
    loop {
        let answer =
            debugger::ask(&mut session, out, &options.abort, &mut options.stop.clone()).await;
        if waits || !matches!(answer, debugger::Answer::Redo(_)) {
            return answer;
        }
    }
}
