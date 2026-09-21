// SPDX-License-Identifier: GPL-3.0-or-later
//! The coordinator: the serialised view of a batch's progress, and everything that reaches
//! the terminal.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Map, Value};
use tokio::sync::{Semaphore, mpsc, watch};
use volant_protocol::TaskResult;

use crate::agent::{AgentLink, AgentSource};
use crate::compile::{Compiled, Step, StepKind};
use crate::inventory::Host;
use crate::playbook::Play;
use crate::render::Renderer;
use crate::stats::{Outcome, Stats};
use crate::template::Templar;

use super::driver::drive_host;
use super::include::IncludeGroup;
use super::prepare::{PlayPlan, host_vars};
use super::{LinkKey, RunOptions, RunState};

/// What the coordinator knows about the batch's shared progress and every host driver may wait
/// on. Republished after every event that can change the live set, so a driver blocked on it is
/// never blocked on a host that has left the batch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    /// Highest task index every live host of the batch has finished, if any.
    pub completed_through: Option<usize>,
    /// Hosts of **this batch** still in the play, in inventory order. Every wait in this file
    /// opens on this list and on no other: the hosts of another batch have no driver running
    /// here - the batch before this one has joined, the batch after it has not started - so a
    /// barrier that counted them would wait for a report nobody is going to send.
    pub live_hosts: Vec<String>,
    /// Hosts of the **whole play** that have not failed, the batches still to come included.
    /// This is what `ansible_play_hosts` reports, and it is read for that and for nothing else:
    /// no barrier, no splice and no frontier may open on it.
    pub play_hosts_left: Vec<String>,
    /// The last flush point the coordinator has spliced the handler steps in behind. A driver
    /// that has reported a flush point waits for this to reach it before it reads the step list
    /// again: until then the list still ends that flush where the compilation ended it, and the
    /// driver would walk past the handlers instead of into them.
    pub spliced_through: Option<usize>,
    /// One entry per `run_once` step the elected host has finished, saying whether its task
    /// failed. The other hosts of the batch wait for their own index to appear here: until it
    /// does, what the one runner registered or set as a fact is not in the store yet, and a
    /// host reading it would read nothing.
    ///
    /// Both halves are needed. Measured on ansible-core 2.19.12: a `run_once` task that fails
    /// takes every other host of the play out of it - with no recap line, since they ran
    /// nothing - and it does so whether or not a `rescue` catches the failure for the host that
    /// ran it. So the failure has to travel; the runner leaving the live set does not carry it.
    pub run_once: BTreeMap<usize, bool>,
    /// Which host runs each `run_once` step, decided here and read by every driver.
    ///
    /// The coordinator holds the only serialised view of the live set, so it is the only party
    /// that can decide this once for everybody. A driver electing from its own read of
    /// `live_hosts` elects from whatever that list happened to say at the moment it looked: a
    /// host woken by the publish that carries the runner's own failure or unreachability reads a
    /// list the runner has already left and elects **itself**, so one `run_once` step runs twice.
    ///
    /// An entry is written once and never rewritten, which is what makes two reads of it agree.
    /// That is also what the reference does: measured on ansible-core 2.19.12, a `run_once`
    /// runner that dies is not replaced and the hosts waiting on it leave the play.
    ///
    /// A step whose mask no live host satisfies gets no entry, which reads the way an election
    /// that found nobody always read: every live host is outside the mask, reports the step and
    /// moves on.
    pub elected: BTreeMap<usize, String>,
}

pub(super) enum Event {
    /// One result line. `counts` says whether it enters the recap (loop items do not, their
    /// aggregate does); `show` whether a line is printed (a loop aggregate that succeeded is not).
    Result {
        host: String,
        index: usize,
        label: Option<String>,
        outcome: Outcome,
        result: TaskResult,
        dump: bool,
        show: bool,
        counts: bool,
        /// The task's `no_log`. Carried on the event rather than applied to `result`, because
        /// the recap and the registered variable read the real result and only the terminal
        /// line is censored.
        censored: bool,
        /// The host this task's `delegate_to` sent it to, for the `[h1 -> h3]` the reference
        /// prints. Carried beside the host rather than folded into it because the recap counts
        /// the host the task was written for: measured on ansible-core 2.19.12, a play over h1
        /// and h2 delegating to h3 recaps h1 and h2 and shows no h3 line at all.
        delegate: Option<String>,
    },
    /// One attempt of a task with `until` or `retries` failed and another one is coming, or this
    /// was the last: the reference prints this line after every failed attempt, the last
    /// included. It is queued behind the same key as the results of the item it belongs to, so
    /// it reads where the reference prints it - in front of that item's own line.
    Retrying {
        host: String,
        index: usize,
        name: String,
        left: u32,
    },
    /// One `[WARNING]` a step produced: an `environment` layer that did not render to a mapping,
    /// or a line the agent asked to show while the step's batch was running. Queued behind the
    /// same key as that step's results so it reads where the step reads, and carrying `censored`
    /// so the `no_log` policy reaches it the way it reaches a result.
    ///
    /// `censored` is false for the `environment` warning, which quotes the playbook's own source
    /// and never a rendered value, and is the batch's `no_log` for an agent line - see
    /// [`Renderer::warning`](crate::render::Renderer::warning).
    Warning {
        host: String,
        index: usize,
        message: String,
        censored: bool,
    },
    /// What `host` asked for at the include step `index`, already read and compiled. Empty when
    /// every item was skipped or failed to resolve, which still has to be said: the coordinator
    /// splices at every include point whether or not anybody asked for anything, and a driver
    /// waiting for that splice has no way to learn there was nothing to wait for.
    ///
    /// The expansion is done by the driver rather than by the coordinator so that everything that
    /// can go wrong with it - a file that is not there, a file that is not a list of tasks, a
    /// role nobody can find, an argument this release refuses - fails **that host's** task,
    /// through the ordinary failure path: a `rescue` around the statement takes it, `ignore_errors`
    /// swallows it, and the other hosts carry on. A coordinator that read the file itself would
    /// have to invent a way to fail some of its hosts and release the rest.
    Include {
        host: String,
        index: usize,
        groups: Vec<IncludeGroup>,
    },
    /// Every result of task `index` on `host` has been sent.
    TaskDone { host: String, index: usize },
    /// This host reached step `index` and has no result to show for it. It is the one event
    /// that prints a header of its own every time: measured on ansible-core 2.19.12, a `meta`
    /// shows one `TASK [meta]` banner per live host with nothing underneath, two banners in a
    /// row for two hosts, and none at all for a host that has already left the play.
    Banner { host: String, index: usize },
    /// A host is done with this play and hands back the connections it wants kept open.
    Finished {
        host: String,
        failed: bool,
        /// This host failed without ever reporting a task: the one runner of a `run_once` step
        /// failed and took the rest of the batch out of the play with it. Measured on
        /// ansible-core 2.19.12: those hosts have no recap line, because they ran nothing, and
        /// the run still exits 2. So the exit code has to hear about it and the recap must not.
        silent_failure: bool,
        links: Vec<(LinkKey, AgentLink)>,
    },
    Unreachable {
        host: String,
        msg: String,
        /// The `no_log` of the task whose batch could not run. Measured on ansible-core
        /// 2.19.12: the reference censors the `UNREACHABLE!` line too, reason and all.
        censored: bool,
        /// The delegate the batch could not reach. Measured on ansible-core 2.19.12: a
        /// `delegate_to` naming a host nothing answers for prints
        /// `fatal: [h2 -> h1]: UNREACHABLE!` and takes the **delegating** host out of the run,
        /// which is the host the recap counts.
        delegate: Option<String>,
    },
}

/// Plays one batch of hosts: the coordinator, its drivers, and everything the play's step list
/// does between them. Without `serial` a play is one batch and this is the whole of it.
#[expect(
    clippy::too_many_arguments,
    reason = "the batch's whole context: the coordinator loop, its drivers and the step list it walks"
)]
pub(super) async fn run_batch(
    play: &Play,
    compiled: &Compiled,
    hosts: &[Host],
    all: &[String],
    vars_files: &HashMap<String, Vec<Map<String, Value>>>,
    agents: &AgentSource,
    options: &RunOptions,
    state: &mut RunState,
    out: &mut Renderer,
    stats: &mut Stats,
) -> anyhow::Result<()> {
    let play_hosts: Vec<String> = hosts.iter().map(|h| h.name.clone()).collect();
    // The step list, and the only thing in the play that changes while it runs. This loop owns
    // the sender; every driver reads through its own receiver. One copy per batch: the handlers
    // a batch splices in behind a flush point belong to that batch alone, and the next one starts
    // from the list the compiler produced.
    let (plan_tx, plan_rx) = watch::channel(Arc::new(compiled.clone()));
    let plan = Arc::new(PlayPlan {
        plan: plan_rx,
        force_handlers: play.force_handlers.unwrap_or(options.force_handlers),
        play_vars: play.vars.clone(),
        vars_files: vars_files.clone(),
        all_play_hosts: all.to_vec(),
        r#become: play.r#become,
        become_user: play.become_user.clone(),
        python: state.python.clone(),
    });

    let (tx, mut rx) = mpsc::channel::<Event>(64);
    let (progress_tx, progress_rx) = watch::channel(Progress::default());
    let mut coordinator = Coordinator {
        tx: &progress_tx,
        batch_hosts: &play_hosts,
        all_hosts: all,
        frontier: HashMap::new(),
        spliced_through: None,
        run_once: BTreeMap::new(),
        elected: BTreeMap::new(),
    };
    // This first publish is what decides the runner of a `run_once` step at index 0, before any
    // driver exists to disagree about it. There is no barrier in front of index 0 - nobody has
    // anything to wait for there - so nothing else could make that election unanimous.
    coordinator.publish(&state.failed_hosts, &compiled.steps);
    // Ansible's `forks`, as permits. More permits than hosts would only raise the ceiling
    // above what this play can use, and `Semaphore` refuses a count near `usize::MAX`. Written
    // as `min` then `max` rather than `clamp(1, hosts.len())`: `clamp` panics whenever its
    // minimum exceeds its maximum, which an empty `hosts` would trigger here, and the only thing
    // preventing that today is the `is_empty` return above, invisible from this line.
    let forks = Arc::new(Semaphore::new(options.forks.min(hosts.len()).max(1)));
    let mut workers = Vec::new();
    for host in hosts {
        let tx = tx.clone();
        let watchdog_tx = tx.clone();
        // Connections kept from an earlier play. Taken out of the map for the duration of the
        // play so the driver owns them, and handed back with `Finished`.
        let existing = take_links(&mut state.links, &host.name);
        let (host, plan, agents, options) = (
            host.clone(),
            Arc::clone(&plan),
            agents.clone(),
            options.clone(),
        );
        let (templar, vars) = (Arc::clone(&state.templar), Arc::clone(&state.vars));
        let verbosity = state.verbosity;
        let forks = Arc::clone(&forks);
        let progress = progress_rx.clone();
        let name = host.name.clone();
        let reported = name.clone();
        workers.push((
            name,
            tokio::spawn(async move {
                // A driver that panics would never report, so it would stay in the live set
                // for good and a host waiting at a barrier would wait for a host that is gone.
                // Reporting the panic from outside the driver keeps that promise: every host
                // in the live set either reports or leaves it.
                let driver = tokio::spawn(async move {
                    drive_host(
                        host, plan, agents, options, templar, vars, verbosity, existing, forks,
                        progress, tx,
                    )
                    .await;
                });
                if let Err(err) = driver.await {
                    let _ = watchdog_tx
                        .send(Event::Unreachable {
                            host: reported.clone(),
                            msg: format!("driver panicked: {err}"),
                            censored: false,
                            delegate: None,
                        })
                        .await;
                    let _ = watchdog_tx
                        .send(Event::Finished {
                            host: reported,
                            failed: true,
                            silent_failure: false,
                            links: Vec::new(),
                        })
                        .await;
                }
            }),
        ));
    }
    drop(tx);

    let mut pending: HashMap<(String, usize), Vec<Event>> = HashMap::new();
    let mut done: HashSet<(String, usize)> = HashSet::new();
    let mut gone: HashSet<String> = HashSet::new();
    // What each host asked for at each include step, in arrival order. Keyed by index because a
    // host reports its request and then blocks, which it may do while this loop is still holding
    // an earlier index for a slower host.
    let mut includes: HashMap<usize, Vec<(String, Vec<IncludeGroup>)>> = HashMap::new();
    let mut index = 0;
    // Not `for .. in steps.iter()`: the list grows at a flush point, so its length is read again
    // each time round. The step is cloned rather than borrowed for the same reason - the splice
    // below replaces the value the borrow would point into.
    while let Some(step) = plan.steps().steps.get(index).cloned() {
        let mut header_shown = false;
        for host in &play_hosts {
            loop {
                let key = (host.clone(), index);
                if done.contains(&key) {
                    // The header goes in front of the first event that shows something, not in
                    // front of the first host that finished: a step every host stepped over
                    // has no header at all, and a `meta`, which shows no result line, prints
                    // one of its own per live host.
                    for event in pending.remove(&key).unwrap_or_default() {
                        let banner = matches!(event, Event::Banner { .. });
                        if banner || (!header_shown && shows_a_line(&event)) {
                            let live = progress_tx.borrow().clone();
                            header(&step, &task_name(&step, host, &plan, &live, state), out);
                            header_shown = true;
                        }
                        report_result(event, stats, out);
                    }
                    break;
                }
                if gone.contains(host) {
                    break;
                }
                match rx.recv().await {
                    Some(event @ Event::Result { .. }) => {
                        let (key, lost, sank) = if let Event::Result {
                            host,
                            index,
                            outcome,
                            ..
                        } = &event
                        {
                            (
                                (host.clone(), *index),
                                *outcome == Outcome::Failed,
                                matches!(outcome, Outcome::Failed | Outcome::Rescued),
                            )
                        } else {
                            unreachable!()
                        };
                        // A `run_once` task that failed, whether or not a `rescue` caught it
                        // for the host that ran it: measured on ansible-core 2.19.12, the
                        // rescue keeps that one host in the play (`rescued=1`, exit 2) and the
                        // others still leave. `Ignored` is not a failure and is left out, so a
                        // `run_once` under `ignore_errors` carries the whole batch past it.
                        let announced = sank
                            && plan
                                .steps()
                                .steps
                                .get(key.1)
                                .is_some_and(|s| s.task.runs_once());
                        if announced {
                            coordinator.run_once.insert(key.1, true);
                        }
                        // A failed result is the host leaving the play, and it arrives before
                        // that task's `TaskDone`. Taking it out of the live set here, rather
                        // than waiting for its `Finished`, is what lets the next task read the
                        // shrunken host list without racing the driver that is shutting down.
                        if lost {
                            state.failed_hosts.insert(key.0.clone());
                        }
                        if lost || announced {
                            coordinator.publish(&state.failed_hosts, &plan.steps().steps);
                        }
                        pending.entry(key).or_default().push(event);
                    }
                    // Queued behind the same key as a result, rather than printed on arrival,
                    // so a host racing ahead cannot put its banner above the step before it.
                    Some(
                        event @ (Event::Banner { .. }
                        | Event::Retrying { .. }
                        | Event::Warning { .. }),
                    ) => {
                        let (host, index) = match &event {
                            Event::Banner { host, index }
                            | Event::Retrying { host, index, .. }
                            | Event::Warning { host, index, .. } => (host.clone(), *index),
                            _ => unreachable!(),
                        };
                        pending.entry((host, index)).or_default().push(event);
                    }
                    Some(Event::Include {
                        host,
                        index,
                        groups,
                    }) => {
                        includes.entry(index).or_default().push((host, groups));
                    }
                    Some(Event::TaskDone { host, index }) => {
                        coordinator.finished(&mut done, &host, index);
                        // `or_insert` keeps the verdict of the host that ran the task, which is
                        // the first to report the step. A host that has already failed is the
                        // exception to that order: it is out of the live set, so it was never a
                        // candidate for the election, and under `force_handlers` it walks the
                        // rest of the play reporting every step it passes on its way to its
                        // handlers. Its report is not a verdict, and taking it as one would
                        // release the waiters before the elected runner had run.
                        if !state.failed_hosts.contains(&host)
                            && plan
                                .steps()
                                .steps
                                .get(index)
                                .is_some_and(|s| s.task.runs_once())
                        {
                            coordinator.run_once.entry(index).or_insert(false);
                        }
                        coordinator.publish(&state.failed_hosts, &plan.steps().steps);
                    }
                    Some(Event::Finished {
                        host,
                        failed,
                        silent_failure,
                        links,
                    }) => {
                        if silent_failure {
                            stats.failed_unreported(&host);
                        }
                        if failed {
                            // The host is leaving the run for good: keeping its connection open
                            // would just idle until the run ends.
                            state.failed_hosts.insert(host.clone());
                        }
                        keep_links(&mut state.links, links, failed).await;
                        gone.insert(host);
                        coordinator.publish(&state.failed_hosts, &plan.steps().steps);
                    }
                    Some(Event::Unreachable {
                        host,
                        msg,
                        censored,
                        delegate,
                    }) => {
                        if !header_shown {
                            let live = progress_tx.borrow().clone();
                            header(&step, &task_name(&step, &host, &plan, &live, state), out);
                            header_shown = true;
                        }
                        stats.unreachable(&host);
                        out.unreachable(&host, &msg, censored, delegate.as_deref());
                        state.failed_hosts.insert(host.clone());
                        gone.insert(host);
                        coordinator.publish(&state.failed_hosts, &plan.steps().steps);
                    }
                    None => break,
                }
            }
        }
        // Every host is now either done with this step or out of the play, which is the moment
        // the plan's one dynamic primitive is safe: no driver can be past this index, whether it
        // reported the splice point or stepped over it. A play with no handlers splices nothing
        // and still publishes, because the drivers waiting on it have no way to know that and
        // would wait for ever.
        if crate::compile::is_splice_point(&step.kind) {
            let mut next = { (**plan_tx.borrow()).clone() };
            if let StepKind::Flush { .. } = step.kind {
                let steps = crate::compile::handler_steps(&next, index);
                next.splice(index + 1, steps);
            } else {
                // The requests of every host, grouped so two hosts that asked for the same thing
                // share one `included:` line and one copy of the steps behind it. First
                // appearance decides the order, walked in the play's host order and then in each
                // host's item order, which is the order the reference prints them in.
                let mut arrived = includes.remove(&index).unwrap_or_default();
                // `usize::MAX` rather than `None` for a host the batch does not hold: `None`
                // sorts in front of every rank, so a name that is not in `play_hosts` would
                // take the first `included:` line instead of the last.
                arrived.sort_by_key(|(host, _)| {
                    play_hosts
                        .iter()
                        .position(|h| h == host)
                        .unwrap_or(usize::MAX)
                });
                let mut order: Vec<String> = Vec::new();
                let mut grouped: HashMap<String, (IncludeGroup, Vec<String>)> = HashMap::new();
                for (host, groups) in arrived {
                    for group in groups {
                        match grouped.entry(group.key.clone()) {
                            Entry::Vacant(slot) => {
                                order.push(group.key.clone());
                                slot.insert((group, vec![host.clone()]));
                            }
                            Entry::Occupied(mut slot) => slot.get_mut().1.push(host.clone()),
                        }
                    }
                }
                let mut lines: Vec<(String, Vec<String>, Option<String>)> = Vec::new();
                let mut expansions: Vec<crate::compile::Grafted> = Vec::new();
                for key in order {
                    let (group, hosts) = grouped.remove(&key).expect("a key just inserted");
                    lines.push((group.what, hosts.clone(), group.label));
                    expansions.push(crate::compile::Grafted {
                        expanded: group.expanded,
                        params: group.params,
                        hosts: hosts.into(),
                    });
                }
                crate::compile::graft(&mut next, index + 1, &step, expansions);
                for (what, hosts, label) in lines {
                    // The banner goes in front of the first line this step shows, and for an
                    // include that is often this one: every host resolved its file, so none
                    // of them printed a result.
                    if !header_shown {
                        let live = progress_tx.borrow().clone();
                        let against = hosts.first().map(String::as_str).unwrap_or_default();
                        header(&step, &task_name(&step, against, &plan, &live, state), out);
                        header_shown = true;
                    }
                    out.included(&what, &hosts, label.as_deref());
                    // Measured on ansible-core 2.19.12: one `ok` per host per item, so a
                    // two-item loop over two hosts counts four. The statement's own aggregate
                    // counts nothing, which is what keeps `h1 ok=10` at ten.
                    for host in &hosts {
                        stats.record(host, Outcome::Ok, false);
                    }
                }
            }
            plan_tx.send_replace(Arc::new(next));

            // The splice moved every index past this point, so an election decided for the step
            // that used to sit at `index + 1` now names a different step, possibly under a
            // different mask. Nobody has read it yet, and dropping it rather than shifting it
            // lets the publish below re-decide it against the list that now exists.
            coordinator.forget_elections_past(index);
            coordinator.spliced_through = Some(index);
            coordinator.publish(&state.failed_hosts, &plan.steps().steps);
        }
        if gone.len() == play_hosts.len() && pending.is_empty() {
            break;
        }
        index += 1;
    }
    // The channel is bounded, so a host still owing a send would block forever if reading
    // stopped here: the coordinator's own `worker.await` below would then wait on a task that
    // is waiting on the coordinator. Every host owes at least its `Finished`, and a host whose
    // driver panicked owes the watchdog's `Unreachable` too. Drain until every worker has
    // dropped its sender.
    while let Some(event) = rx.recv().await {
        match event {
            Event::Unreachable {
                host,
                msg,
                censored,
                delegate,
            } => {
                stats.unreachable(&host);
                out.unreachable(&host, &msg, censored, delegate.as_deref());
                state.failed_hosts.insert(host);
                coordinator.publish(&state.failed_hosts, &plan.steps().steps);
            }
            Event::Finished {
                host,
                failed,
                silent_failure,
                links,
            } => {
                if silent_failure {
                    stats.failed_unreported(&host);
                }
                if failed {
                    state.failed_hosts.insert(host);
                }
                keep_links(&mut state.links, links, failed).await;
                coordinator.publish(&state.failed_hosts, &plan.steps().steps);
            }
            Event::TaskDone { host, index } => {
                coordinator.finished(&mut done, &host, index);
                // A host already in `failed_hosts` is walking the rest of the play for its
                // handlers alone; its report is not a verdict, the same way it is not one above.
                if !state.failed_hosts.contains(&host)
                    && plan
                        .steps()
                        .steps
                        .get(index)
                        .is_some_and(|s| s.task.runs_once())
                {
                    coordinator.run_once.entry(index).or_insert(false);
                }
                coordinator.publish(&state.failed_hosts, &plan.steps().steps);
            }
            // A driver sends a result and the `TaskDone` behind it over the same channel, so a
            // result reaching here is one the task loop above never read - which happens when
            // its host entered `gone` before that task's `TaskDone`. Report it rather than drop
            // it: a task missing from the recap is the one failure this file cannot afford.
            Event::Banner { index, .. } => {
                header_for(index, &plan, &play_hosts, &progress_tx, state, out);
            }
            // An include whose request arrives here is one the step loop will never reach: it
            // has already ended, because every host of the batch has left it. There is nothing
            // left to splice the steps in for, and nothing left to run them.
            Event::Include { .. } => {}
            event @ (Event::Result { .. } | Event::Retrying { .. } | Event::Warning { .. }) => {
                report_result(event, stats, out);
            }
        }
    }
    // Results the task loop left behind for the same reason: a host that entered `gone` between
    // a result and that task's `TaskDone` breaks the inner loop without draining what it had
    // already queued. Sorted by task and then by the play's own host order, so a leftover reads
    // where it would have read.
    let mut leftover: Vec<((String, usize), Vec<Event>)> = pending.into_iter().collect();
    leftover.sort_by_key(|((host, index), _)| (*index, play_hosts.iter().position(|h| h == host)));
    for (_, events) in leftover {
        for event in events {
            if let Event::Banner { index, .. } = &event {
                header_for(*index, &plan, &play_hosts, &progress_tx, state, out);
                continue;
            }
            report_result(event, stats, out);
        }
    }
    for (host, worker) in workers {
        if let Err(err) = worker.await
            && !gone.contains(&host)
        {
            stats.unreachable(&host);
            out.unreachable(&host, &format!("driver panicked: {err}"), false, None);
            state.failed_hosts.insert(host);
        }
    }
    Ok(())
}

/// Whether an event puts a line on the terminal, which is what a step's header goes in front
/// of. A result nobody shows still speaks when `ignore_errors` swallowed a failure: the
/// `...ignoring` line is the whole display of that step for that host.
fn shows_a_line(event: &Event) -> bool {
    match event {
        Event::Result { show, outcome, .. } => *show || *outcome == Outcome::Ignored,
        // A retry line is the first thing a retried task prints, and the reference shows the
        // task's banner above it: a task whose every attempt is still to come has no result yet.
        Event::Banner { .. } | Event::Retrying { .. } => true,
        // `Event::Warning` falls here with the rest: a warning goes to stderr, beside the
        // display rather than in it, so it never conjures the banner of a step that shows
        // nothing on stdout.
        _ => false,
    }
}

/// The header of step `index`, for the drain paths that reach a banner after the step loop has
/// moved on. The step's name is rendered against the first host of the batch, which is the same
/// choice the step loop makes for a header a later host would have printed - and a host of this
/// batch rather than of the play, so a name naming its own host names one that ran.
fn header_for(
    index: usize,
    plan: &PlayPlan,
    play_hosts: &[String],
    progress_tx: &watch::Sender<Progress>,
    state: &RunState,
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
    header(step, &task_name(step, host, plan, &live, state), out);
}

/// The banner one step gets: a handler says so, measured - `RUNNING HANDLER [second handler]`
/// where an ordinary task says `TASK [...]`.
fn header(step: &Step, name: &str, out: &mut Renderer) {
    match step.kind {
        StepKind::Handler(_) => out.handler(name),
        _ => out.task(name),
    }
}

/// Puts one result line in the recap and on the terminal. Anything but an `Event::Result` is
/// ignored, so both event loops can hand it whatever they hold.
fn report_result(event: Event, stats: &mut Stats, out: &mut Renderer) {
    if let Event::Retrying {
        host, name, left, ..
    } = &event
    {
        out.retrying(host, name, *left);
        return;
    }
    if let Event::Warning {
        message, censored, ..
    } = &event
    {
        out.warning(message, *censored);
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

/// The coordinator's published state: everything a publish reads, held in one place because it
/// is read under one serialised view and nowhere else.
struct Coordinator<'a> {
    /// The batch's shared progress. Every driver reads it; only the coordinator writes it.
    tx: &'a watch::Sender<Progress>,
    /// The batch's own hosts, which is what `ansible_play_batch` is served from.
    batch_hosts: &'a [String],
    /// Every host of the play, which is what `ansible_play_hosts_all` is served from.
    all_hosts: &'a [String],
    /// The index each host has finished every task through. Not the highest index it has
    /// mentioned: a driver reports the steps it is about to step over as soon as it has decided
    /// to, which is before the batch in front of them has run, so its reports do arrive out of
    /// order. See `finished`.
    frontier: HashMap<String, usize>,
    /// The last splice point the coordinator has published a splice for, republished with every
    /// `Progress` so a driver waiting on one reads it whatever else moved.
    spliced_through: Option<usize>,
    /// What the one host elected for each `run_once` step made of it, for the hosts waiting
    /// behind it. Filled by the coordinator rather than by the elected driver because the failure
    /// has to be published **with** the live-set change it causes: a failed result takes its host
    /// out of `live_hosts` in the same arm, and a host that saw the shrunken list before the
    /// verdict would read a leader that is merely gone and leave the play the quiet way, at the
    /// wrong exit code.
    run_once: BTreeMap<usize, bool>,
    /// Who runs each `run_once` step. Written once per index and never rewritten. See
    /// `Progress::elected`.
    elected: BTreeMap<usize, String>,
}

impl Coordinator<'_> {
    /// Files one finished step and moves that host's frontier: the index it has finished every
    /// step through, which is the only figure a barrier may open on.
    ///
    /// The frontier is not the highest index a host has mentioned. A driver collecting a batch
    /// decides where it goes next before the batch has run, and tells the coordinator about the
    /// steps it stepped over there and then, so a report for step 9 can arrive while step 7 is
    /// still on the wire. Counting the highest would open another host's `hostvars` barrier on
    /// work this one has not started. A gap closes when the batch reports; a gap that never
    /// closes belongs to a host that failed or went unreachable, and such a host is out of the
    /// live set, so nothing waits on it.
    fn finished(&mut self, done: &mut HashSet<(String, usize)>, host: &str, index: usize) {
        done.insert((host.to_string(), index));
        let mut next = self.frontier.get(host).map_or(0, |reached| reached + 1);
        while done.contains(&(host.to_string(), next)) {
            self.frontier.insert(host.to_string(), next);
            next += 1;
        }
    }

    /// Drops every election past `index`, which a splice invalidated: the entries name steps
    /// that have moved.
    fn forget_elections_past(&mut self, index: usize) {
        self.elected.split_off(&(index + 1));
    }

    /// Republishes the batch's progress. `live_hosts` is the batch's own list minus the hosts
    /// that have failed or gone unreachable, which is what `ansible_play_batch` reports and the
    /// only list a wait may open on; `play_hosts_left` is the same subtraction over the whole
    /// play, which is what `ansible_play_hosts` reports. `completed_through` is the lowest
    /// frontier among the live hosts of the batch, so a driver waiting for it to reach `i - 1`
    /// is waiting only on hosts that are still expected to report - and never on a host of
    /// another batch, which has no driver here to report at all.
    ///
    /// It is also where a `run_once` step's runner is elected, for the reason this method
    /// exists: this is the one place the live set is read under a serial view. See
    /// `Progress::elected`.
    ///
    /// `lost` and `steps` stay parameters: the first is the run's own failed set, which the
    /// caller borrows mutably elsewhere in the same scope, and the second is a list that grows
    /// under the caller, read again at each call.
    fn publish(&mut self, lost: &HashSet<String>, steps: &[Step]) {
        let live_hosts: Vec<String> = self
            .batch_hosts
            .iter()
            .filter(|h| !lost.contains(*h))
            .cloned()
            .collect();
        let play_hosts_left: Vec<String> = self
            .all_hosts
            .iter()
            .filter(|h| !lost.contains(*h))
            .cloned()
            .collect();
        // `None` sorts below every `Some`, so a live host that has reported nothing yet holds the
        // minimum at `None` and no barrier opens on it.
        let completed_through = live_hosts
            .iter()
            .map(|h| self.frontier.get(h).copied())
            .min()
            .flatten();
        // The election, for the one index it can be needed at: `completed_through + 1` is where
        // the slowest live host stands, and `None` is the head of the list, which is how the
        // first call of all - made before a driver exists - decides index 0. The runner is the
        // first live host the step's mask includes, because one outside it runs nothing and
        // nobody would run the step. Never an overwrite; see `Progress::elected`.
        let at = completed_through.map_or(0, |c| c + 1);
        if let Some(step) = steps.get(at)
            && step.task.runs_once()
            && !self.elected.contains_key(&at)
            && let Some(runner) = live_hosts.iter().find(|h| {
                step.hosts
                    .as_ref()
                    .is_none_or(|only| only.iter().any(|m| m == *h))
            })
        {
            self.elected.insert(at, runner.clone());
        }
        self.tx.send_replace(Progress {
            completed_through,
            live_hosts,
            play_hosts_left,
            spliced_through: self.spliced_through,
            run_once: self.run_once.clone(),
            elected: self.elected.clone(),
        });
    }
}

/// Files the connections one host handed back, keeping only the one worth keeping.
///
/// What connection persistence buys is the connection to the host itself, and that is the
/// `become_user: None` link: it is the one whose `ssh`, host key exchange and authentication a
/// later play would otherwise pay for again. An escalated link is cheap to reopen on top of it,
/// because its bootstrap short-circuits on the agent already cached for that user, so it is
/// closed here instead of idling until the recap.
///
/// A driver holds its escalated link for as long as the play keeps escalating the same way, so
/// this is where most of them are closed: an escalated link lives for a play at the longest, and
/// the run carries one link per host to the recap. The other places are the failure arm of
/// `drive_host`, and a batch escalating differently from the one before it, which retires the
/// link it replaces.
async fn keep_links(
    links: &mut HashMap<LinkKey, AgentLink>,
    handed_back: Vec<(LinkKey, AgentLink)>,
    failed: bool,
) {
    let mut closing = tokio::task::JoinSet::new();
    for (key, link) in handed_back {
        if failed || key.become_user.is_some() {
            closing.spawn(link.shutdown());
        } else {
            links.insert(key, link);
        }
    }
    while closing.join_next().await.is_some() {}
}

/// The keys of the connections that escalate: the ones a driver retires when it fails, when it
/// escalates differently, and when it leaves the play.
pub(super) fn escalated_links(links: &HashMap<LinkKey, AgentLink>) -> Vec<LinkKey> {
    links
        .keys()
        .filter(|key| key.become_user.is_some())
        .cloned()
        .collect()
}

/// Takes every connection belonging to one host out of the run's map.
fn take_links(links: &mut HashMap<LinkKey, AgentLink>, host: &str) -> Vec<(LinkKey, AgentLink)> {
    let keys: Vec<LinkKey> = links.keys().filter(|k| k.host == host).cloned().collect();
    keys.into_iter()
        .filter_map(|k| links.remove(&k).map(|link| (k, link)))
        .collect()
}

/// The banner of one step: the task's name, prefixed by the role it came from.
///
/// Measured on ansible-core 2.19.12: a task of a role shows as `TASK [base : base task]`, and an
/// unnamed one shows the module name behind the same prefix (`TASK [inner : debug]`).
fn task_name(
    step: &Step,
    host: &str,
    plan: &PlayPlan,
    live: &Progress,
    state: &RunState,
) -> String {
    let task = &step.task;
    let name = if Templar::is_template(&task.name) {
        let vars = host_vars(
            host,
            plan,
            &task.vars,
            step.role,
            step.include_params.as_deref(),
            live,
            state.templar.as_ref(),
            &state.vars,
        );
        state
            .templar
            .render(&task.name, &vars)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| task.name.clone())
    } else {
        task.name.clone()
    };
    match step.role.and_then(|i| plan.steps().roles.get(i).cloned()) {
        Some(role) => format!("{} : {name}", role.name),
        None => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playbook::PlayTask;

    /// A coordinator over one list of hosts, which the batch and the play share.
    ///
    /// `run_once` and `elected` start empty and then persist across every `publish` the test
    /// makes, so a test over a real step list reads the elections its earlier calls decided.
    fn coordinator<'a>(tx: &'a watch::Sender<Progress>, hosts: &'a [String]) -> Coordinator<'a> {
        Coordinator {
            tx,
            batch_hosts: hosts,
            all_hosts: hosts,
            frontier: HashMap::new(),
            spliced_through: None,
            run_once: BTreeMap::new(),
            elected: BTreeMap::new(),
        }
    }

    /// The barrier opens on the slowest live host, and a host that has left the play stops
    /// holding it: this is what keeps a wait from outliving the host it waits for.
    #[test]
    fn progress_follows_the_slowest_live_host_and_forgets_the_others() {
        let hosts: Vec<String> = vec!["alpha".into(), "beta".into()];
        let (tx, rx) = watch::channel(Progress::default());
        let mut co = coordinator(&tx, &hosts);
        let mut lost = HashSet::new();

        co.publish(&lost, &[]);
        assert_eq!(rx.borrow().completed_through, None, "nobody has reported");
        assert_eq!(rx.borrow().live_hosts, hosts);

        co.frontier.insert("beta".to_string(), 3);
        co.publish(&lost, &[]);
        assert_eq!(
            rx.borrow().completed_through,
            None,
            "alpha has reported nothing, so the barrier stays shut"
        );

        co.frontier.insert("alpha".to_string(), 1);
        co.publish(&lost, &[]);
        assert_eq!(rx.borrow().completed_through, Some(1));

        lost.insert("alpha".to_string());
        co.publish(&lost, &[]);
        assert_eq!(
            rx.borrow().completed_through,
            Some(3),
            "a host out of the play no longer holds the barrier"
        );
        assert_eq!(rx.borrow().live_hosts, vec!["beta".to_string()]);

        lost.insert("beta".to_string());
        co.publish(&lost, &[]);
        assert!(rx.borrow().live_hosts.is_empty());
        assert_eq!(rx.borrow().completed_through, None);
    }

    /// Who runs a `run_once` step is decided once, and a host that leaves afterwards does not
    /// move it. The mask is part of that decision, not a filter applied after it.
    ///
    /// This is the double election. Two drivers used to read `live_hosts` for themselves and take
    /// the first live host of it, and nothing made them read the same value: at index 0 there is
    /// no barrier at all, and further along the barrier only says everyone finished the step
    /// before - not that the live set will hold still across the two reads. A host woken by the
    /// publish that carries the runner's own failure or unreachability read a list the runner had
    /// already left, elected itself, and ran the step a **second** time. It was seen as a flake on
    /// `an_unreachable_run_once_runner_releases_the_hosts_waiting_on_it`, where the run still
    /// exits 4 and only the line the second runner prints gives it away.
    ///
    /// What would make this red: the election moved back into the drivers, which is the second
    /// read naming `h2`; an overwrite in place of the write-once shape, same thing; or the mask
    /// dropped from it, which elects a host that runs nothing so nobody runs the step.
    #[test]
    fn run_once_is_decided_once_and_survives_the_runner_leaving() {
        let hosts: Vec<String> = vec!["h1".into(), "h2".into()];
        let step = |mask: Option<Vec<String>>| Step {
            kind: StepKind::Task,
            task: PlayTask {
                name: "once".into(),
                module: "command".into(),
                run_once: Some(true),
                ..PlayTask::empty()
            },
            block: None,
            section: crate::compile::Section::Body,
            role: None,
            origin: Arc::default(),
            include_params: None,
            hosts: mask.map(Arc::from),
        };
        let steps = vec![step(None), step(Some(vec!["h2".into()]))];

        let (tx, rx) = watch::channel(Progress::default());
        let mut co = coordinator(&tx, &hosts);
        let mut lost: HashSet<String> = HashSet::new();

        // The publish `run_batch` makes before it spawns a single driver. Index 0 has no barrier
        // in front of it, so this is the only moment its runner can be fixed.
        co.publish(&lost, &steps);
        assert_eq!(
            rx.borrow().elected.get(&0).map(String::as_str),
            Some("h1"),
            "the first live host of the batch"
        );

        // h1 dies on the way, which is the publish that wakes h2.
        lost.insert("h1".to_string());
        co.publish(&lost, &steps);
        let p = rx.borrow().clone();
        assert_eq!(
            p.live_hosts,
            vec!["h2".to_string()],
            "the list a driver electing for itself would have read"
        );
        assert_eq!(
            p.elected.get(&0).map(String::as_str),
            Some("h1"),
            "and the election did not move with it"
        );

        // The step behind it is masked to h2, and both hosts are live again for it.
        let (tx, rx) = watch::channel(Progress::default());
        let mut co = coordinator(&tx, &hosts);
        for host in &hosts {
            co.frontier.insert(host.clone(), 0);
        }
        co.publish(&HashSet::new(), &steps);
        assert_eq!(
            rx.borrow().elected.get(&1).map(String::as_str),
            Some("h2"),
            "elected inside the step's mask, not filtered after it"
        );
    }

    /// A host is never counted past what it has actually finished, whatever order its reports
    /// arrive in: a driver publishes the steps it steps over before the batch in front of them
    /// has run, so the barrier reads the step every one before it has been reported through.
    ///
    /// What would make this red: taking the highest index a host has mentioned. The barrier
    /// would then open on work that host has not started, which is a `hostvars` read answered
    /// from a fact the other host has not gathered yet.
    #[test]
    fn a_gap_in_a_hosts_reports_holds_the_barrier_where_it_is() {
        let hosts: Vec<String> = vec!["alpha".into()];
        let (tx, rx) = watch::channel(Progress::default());
        let mut co = coordinator(&tx, &hosts);
        let mut done = HashSet::new();
        let lost = HashSet::new();

        for index in [0, 1] {
            co.finished(&mut done, "alpha", index);
        }
        // Steps 4 and 5 are stepped over: the driver says so while the batch holding 2 and 3 is
        // still running.
        for index in [4, 5] {
            co.finished(&mut done, "alpha", index);
        }
        co.publish(&lost, &[]);
        assert_eq!(
            rx.borrow().completed_through,
            Some(1),
            "the steps behind the gap are not finished yet"
        );

        co.finished(&mut done, "alpha", 2);
        co.publish(&lost, &[]);
        assert_eq!(rx.borrow().completed_through, Some(2), "the gap is smaller");

        co.finished(&mut done, "alpha", 3);
        co.publish(&lost, &[]);
        assert_eq!(
            rx.borrow().completed_through,
            Some(5),
            "the batch reported, so everything behind it counts"
        );
    }
}
