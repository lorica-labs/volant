// SPDX-License-Identifier: GPL-3.0-or-later
//! The coordinator: the serialised view of a batch's progress, and everything that reaches
//! the terminal.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};
use tokio::sync::{Semaphore, mpsc, watch};

use crate::agent::AgentSource;
use crate::compile::{Compiled, StepKind};
use crate::inventory::Host;
use crate::playbook::Play;
use crate::render::Renderer;
use crate::stats::{Outcome, Stats};

use super::debugger;
use super::driver::{Definitions, drive_host, mark_boundaries};
use super::include::IncludeGroup;
use super::prepare::{PlayPlan, Strategy};
use super::{RunOptions, RunState};

mod display;
mod events;
mod meta;
mod publish;
mod types;

use display::{header, header_for, report_result, switch_header};
use events::{
    Held, answer, book, debug_ask, debugging, show_now, shows_a_line, unreachable_ask,
    unreachable_left,
};
use meta::{MetaContext, apply_meta};
use publish::{Coordinator, keep_links, take_links, task_name};
pub(in crate::executor) use publish::{escalated_links, render_name};
pub use types::Progress;
pub(in crate::executor) use types::{BatchEnd, Event, MetaEffect};
use types::{Round, Unreached};

/// Plays one batch of hosts: the coordinator, its drivers, and everything the play's step list
/// does between them. Without `serial` a play is one batch and this is the whole of it.
///
/// `hosts` are the ones that run, `whole` the batch as the inventory cut it - failed hosts
/// included, which is what `clear_facts` and `clear_host_errors` reach - and `dormant` the ones of
/// it that were unreachable before the play began, which `clear_host_errors` brings back at the
/// play's first step. `resumed` is `Some` for a round that brings hosts back: the step each picks
/// the play up at, on the list the round before left, which is why nothing is spliced again.
#[expect(
    clippy::too_many_arguments,
    reason = "the batch's whole context: the coordinator loop, its drivers and the step list it walks"
)]
#[expect(clippy::too_many_lines)]
pub(super) async fn run_batch(
    play: &Play,
    compiled: &Compiled,
    hosts: &[Host],
    whole: &[String],
    dormant: &[String],
    all: &[String],
    vars_files: &HashMap<String, Vec<Map<String, Value>>>,
    max_fail: Option<(f64, usize)>,
    resumed: Option<&HashMap<String, usize>>,
    agents: &AgentSource,
    options: &RunOptions,
    state: &mut RunState,
    out: &mut Renderer,
    stats: &mut Stats,
) -> anyhow::Result<BatchEnd> {
    let play_hosts: Vec<String> = hosts.iter().map(|h| h.name.clone()).collect();
    // The step list, and the only thing in the play that changes while it runs. This loop owns
    // the sender; every driver reads through its own receiver. One copy per batch: the handlers
    // a batch splices in behind a flush point belong to that batch alone, and the next one starts
    // from the list the compiler produced.
    //
    // Marked here rather than at compile time because what a variable name can read depends on
    // the inventory, the play's `vars` and `vars_files`, which the compiler never sees; and per
    // batch because each batch starts again from the compiler's list.
    let mut definitions = Definitions::default();
    let playbook_dir = {
        let store = state.vars.lock().expect("vars lock");
        store.static_maps().for_each(|m| definitions.add(m));
        store.playbook_dir().to_path_buf()
    };
    definitions.add(&play.vars);
    vars_files
        .values()
        .flatten()
        .for_each(|m| definitions.add(m));
    let mut first = compiled.clone();
    mark_boundaries(&mut first, &definitions, &playbook_dir);
    let (plan_tx, plan_rx) = watch::channel(Arc::new(first));
    let plan = Arc::new(PlayPlan {
        plan: plan_rx,
        force_handlers: play.force_handlers.unwrap_or(options.force_handlers),
        // Under `--step` the hosts meet in front of every task, so the question is asked once
        // and the banner of the task before it has been shown.
        lockstep: max_fail.is_some() || options.step.is_some(),
        strategy: Strategy::named(play.strategy.as_deref()),
        throttles: Mutex::default(),
        queue: if play.strategy.as_deref() == Some("host_pinned") {
            play_hosts.clone()
        } else {
            Vec::new()
        },
        turn: watch::channel(0).0,
        play_vars: play.vars.clone(),
        vars_files: vars_files.clone(),
        all_play_hosts: all.to_vec(),
        r#become: play.r#become,
        become_user: play.become_user.clone(),
        python: state.python.clone(),
        step: options.step.clone().map(debugger::Stepper::new),
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
        fatal: BTreeSet::new(),
        aborted: false,
        aborted_at: None,
        failed_here: HashSet::new(),
        last_failure: None,
        limit: max_fail.map(|(limit, _)| limit),
        batch_size: max_fail.map_or(hosts.len(), |(_, size)| size),
        removed: HashSet::new(),
        ending: BTreeSet::new(),
        dismissed: BTreeSet::new(),
    };
    let mut round = Round {
        dormant: dormant.to_vec(),
        ..Round::default()
    };
    if resumed.is_some() {
        // Nothing is spliced in a round that brings hosts back: the list already holds what the
        // round before spliced, so a driver waiting on a splice finds every one landed.
        coordinator.spliced_through = Some(usize::MAX);
    }
    // Whether the two `NO MORE HOSTS LEFT` of `max_fail_percentage` are out. They follow the last
    // line of the step the batch was lost at.
    let mut said_no_more = false;
    let mut marked_failed = false;
    let mut done: HashSet<(String, usize)> = HashSet::new();
    let mut index = 0;
    if let Some(at) = resumed {
        // The hosts this round brings back are past every step in front of the one they resume
        // at: filed as done, so the walk below does not wait for a word about them, and as the
        // frontier their barriers open on.
        index = at.values().copied().min().unwrap_or(0);
        for (host, start) in at {
            for step in 0..*start {
                done.insert((host.clone(), step));
            }
            if let Some(last) = start.checked_sub(1) {
                coordinator.frontier.insert(host.clone(), last);
            }
        }
    }
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
        let start = resumed.and_then(|at| at.get(&host.name).copied());
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
                        progress, tx, start,
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
                            origin: None,
                            index: None,
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

    // Under `free` and `host_pinned` a line is written as it arrives, and a banner goes in front
    // of it whenever the step differs from the one the last line belonged to.
    let free = plan.strategy.free();
    let mut last_banner: Option<usize> = None;
    let mut pending: HashMap<(String, usize), Vec<Event>> = HashMap::new();
    let mut gone: HashSet<String> = HashSet::new();
    // The templated names the drivers rendered at their own step, by host and step.
    let mut names: HashMap<(String, usize), String> = HashMap::new();
    // What each host asked for at each include step, in arrival order. Keyed by index because a
    // host reports its request and then blocks, which it may do while this loop is still holding
    // an earlier index for a slower host.
    let mut includes: HashMap<usize, Vec<(String, Vec<IncludeGroup>)>> = HashMap::new();
    // What a host that could not be reached owes the terminal, by host and the step its batch
    // began at. The host leaves the play on arrival; its block and line print where the host
    // stands in the step's order, like a result, instead of in front of the hosts before it.
    let mut unreached: HashMap<(String, usize), Unreached> = HashMap::new();
    // The results a driver holds at the debugger prompt, by host and step, until the operator
    // answers. See `Event::Debug`.
    let mut held: HashMap<(String, usize), Held> = HashMap::new();
    // The prompt behind a result a driver holds, true for `r`, and what a failed result or a
    // host that could not be reached changes in the batch: see the functions they call.
    #[rustfmt::skip]
    macro_rules! answer {
        ($held:expr, $host:expr, $at:expr) => {
            answer($held, ($host, $at), &plan, &mut coordinator, &mut round, &mut gone,
                &progress_tx, state, &names, stats, out, options).await
        };
    }
    #[rustfmt::skip]
    macro_rules! book {
        ($key:expr, $lost:expr, $sank:expr) => {
            book(&plan, &mut coordinator, &mut round, &mut state.failed_hosts, $key, $lost, $sank)
        };
    }
    #[rustfmt::skip]
    macro_rules! unreachable_left {
        ($host:expr, $at:expr) => {
            unreachable_left(&plan, &mut coordinator, &mut round, stats, &mut state.failed_hosts,
                $host, $at)
        };
    }
    // The prompt a result no driver holds opens when its step's `debugger` says so.
    macro_rules! prompt {
        ($asked:expr, $host:expr, $at:expr) => {
            debugging(
                $asked,
                $host,
                $at,
                false,
                &plan,
                &progress_tx,
                state,
                &names,
                out,
                options,
            )
            .await
        };
    }
    // Not `for .. in steps.iter()`: the list grows at a flush point, so its length is read again
    // each time round. The step is cloned rather than borrowed for the same reason - the splice
    // below replaces the value the borrow would point into.
    while let Some(step) = plan.steps().steps.get(index).cloned() {
        let mut header_shown = false;
        #[rustfmt::skip]
        macro_rules! step_header {
            ($host:expr) => {
                if !header_shown {
                    let live = progress_tx.borrow().clone();
                    let spoken = names.get(&($host.to_string(), index));
                    header(&step, &task_name(&step, $host, &plan, &live, state, spoken), out);
                    header_shown = true;
                }
            };
        }
        for host in &play_hosts {
            loop {
                // The batch is lost: every host of it is out of the play, the way the reference
                // marks them all failed.
                if coordinator.aborted && !marked_failed {
                    marked_failed = true;
                    state.failed_hosts.extend(play_hosts.iter().cloned());
                }
                let key = (host.clone(), index);
                let answering = held.get(&key).is_some_and(Held::ready);
                if done.contains(&key) || answering {
                    // The header goes in front of the first event that shows something, not in
                    // front of the first host that finished: a step every host stepped over
                    // has no header at all, and a `meta`, which shows no result line, prints
                    // one of its own per live host.
                    for event in pending.remove(&key).unwrap_or_default() {
                        let banner = matches!(event, Event::Banner { .. });
                        // A banner always brings its header.
                        header_shown &= !banner;
                        if banner || shows_a_line(&event) {
                            step_header!(host);
                        }
                        let asks = debug_ask(&event, &plan);
                        report_result(event, stats, out);
                        if let Some(kind) = asks {
                            debugging(
                                kind,
                                host,
                                index,
                                false,
                                &plan,
                                &progress_tx,
                                state,
                                &names,
                                out,
                                options,
                            )
                            .await;
                        }
                    }
                    // `r`: the host runs the step again, and its new results are shown here,
                    // under the same banner, before the next host's.
                    if answering && let Some(waiting) = held.remove(&key) {
                        if waiting.unreached.is_some() {
                            step_header!(host);
                        }
                        if answer!(waiting, host, index) {
                            continue;
                        }
                    }
                    break;
                }
                if gone.contains(host) {
                    if let Some(blame) = unreached.remove(&key) {
                        step_header!(host);
                        blame.show(host, out);
                        prompt!(unreachable_ask(), host, key.1);
                    }
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
                        let event = if let Some(waiting) = held.get_mut(&key) {
                            waiting.hold(event, lost, sank)
                        } else {
                            book!((&key.0, key.1), lost, sank);
                            event
                        };
                        if free {
                            let asks = debug_ask(&event, &plan);
                            show_now(
                                event,
                                &mut last_banner,
                                &plan,
                                &progress_tx,
                                state,
                                &names,
                                stats,
                                out,
                            );
                            if let Some(kind) = asks {
                                debugging(
                                    kind,
                                    &key.0,
                                    key.1,
                                    false,
                                    &plan,
                                    &progress_tx,
                                    state,
                                    &names,
                                    out,
                                    options,
                                )
                                .await;
                            }
                            if held.get(&key).is_some_and(Held::ready)
                                && let Some(waiting) = held.remove(&key)
                            {
                                answer!(waiting, &key.0, key.1);
                            }
                        } else {
                            pending.entry(key).or_default().push(event);
                        }
                    }
                    // Queued behind the same key as a result, rather than printed on arrival,
                    // so a host racing ahead cannot put its banner above the step before it.
                    Some(
                        event @ (Event::Banner { .. }
                        | Event::Retrying { .. }
                        | Event::Warning { .. }
                        | Event::Display { .. }
                        | Event::UnreachableIgnored { .. }),
                    ) => {
                        let (host, index) = match &event {
                            Event::Banner { host, index }
                            | Event::Retrying { host, index, .. }
                            | Event::Warning { host, index, .. }
                            | Event::Display { host, index, .. }
                            | Event::UnreachableIgnored { host, index, .. } => {
                                (host.clone(), *index)
                            }
                            _ => unreachable!(),
                        };
                        if free {
                            show_now(
                                event,
                                &mut last_banner,
                                &plan,
                                &progress_tx,
                                state,
                                &names,
                                stats,
                                out,
                            );
                        } else {
                            pending.entry((host, index)).or_default().push(event);
                        }
                    }
                    Some(Event::Include {
                        host,
                        index,
                        groups,
                    }) => {
                        includes.entry(index).or_default().push((host, groups));
                    }
                    Some(Event::Debug { host, index, reply }) => {
                        held.insert((host, index), Held::new(reply));
                    }
                    Some(Event::Named { host, index, name }) => {
                        names.insert((host, index), name);
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
                                .is_some_and(|s| s.task.bypasses_host_loop())
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
                        if round.files_failure(&host, failed) {
                            // The host is leaving the run for good: keeping its connection open
                            // would just idle until the run ends.
                            state.failed_hosts.insert(host.clone());
                        }
                        keep_links(&mut state.links, links, failed).await;
                        gone.insert(host);
                        coordinator.publish(&state.failed_hosts, &plan.steps().steps);
                    }
                    Some(Event::Meta {
                        host,
                        index: at,
                        effect,
                    }) => {
                        let warned = apply_meta(
                            effect,
                            &mut round,
                            &mut coordinator,
                            &MetaContext {
                                host: &host,
                                at,
                                whole,
                                pattern: &play.hosts,
                                plan: &plan,
                                abort: &options.abort,
                                first: crate::compile::first(&plan.steps()),
                            },
                            state,
                            stats,
                        );
                        for message in warned {
                            out.warning(&message, false);
                        }
                    }
                    Some(Event::Unreachable {
                        host,
                        msg,
                        censored,
                        delegate,
                        origin,
                        index: at,
                    }) => {
                        let blame = Unreached {
                            msg,
                            censored,
                            delegate,
                            origin,
                        };
                        // A host its driver holds at the prompt leaves the play only once the
                        // operator keeps the result: `r` tries to reach it again.
                        let waiting = at.and_then(|at| held.remove(&(host.clone(), at)));
                        if let Some(at) = at
                            && !free
                        {
                            if let Some(mut waiting) = waiting {
                                waiting.unreached = Some(blame);
                                held.insert((host, at), waiting);
                                continue;
                            }
                            unreached.insert((host.clone(), at), blame);
                        } else if free {
                            switch_header(
                                &mut last_banner,
                                at.unwrap_or(index),
                                &host,
                                &plan,
                                &progress_tx,
                                state,
                                &names,
                                out,
                            );
                            if let Some(at) = at
                                && let Some(mut waiting) = waiting
                            {
                                waiting.unreached = Some(blame);
                                answer!(waiting, &host, at);
                                continue;
                            }
                            blame.show(&host, out);
                            if let Some(at) = at {
                                prompt!(unreachable_ask(), &host, at);
                            }
                        } else {
                            step_header!(&host);
                            blame.show(&host, out);
                        }
                        unreachable_left!(&host, at);
                        gone.insert(host);
                    }
                    None => break,
                }
            }
        }
        // The reference says so once for the task and once for the play, behind the last line the
        // task printed.
        if !said_no_more && coordinator.aborted_at.is_some_and(|at| at <= index) {
            said_no_more = true;
            out.no_more_hosts();
            out.no_more_hosts();
        }
        // Every host is now either done with this step or out of the play, which is the moment
        // the plan's one dynamic primitive is safe: no driver can be past this index, whether it
        // reported the splice point or stepped over it. A play with no handlers splices nothing
        // and still publishes, because the drivers waiting on it have no way to know that and
        // would wait for ever.
        if crate::compile::is_splice_point(&step.kind) && resumed.is_none() {
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
                // `usize::MAX` rather than `None` for a host the batch does not hold, which
                // would otherwise sort in front of every rank and take the first line.
                arrived.sort_by_key(|(host, _)| {
                    play_hosts
                        .iter()
                        .position(|h| h == host)
                        .unwrap_or(usize::MAX)
                });
                let mut asking: Vec<String> =
                    arrived.iter().map(|(host, _)| host.clone()).collect();
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
                    if free {
                        let against = hosts.first().map(String::as_str).unwrap_or_default();
                        switch_header(
                            &mut last_banner,
                            index,
                            against,
                            &plan,
                            &progress_tx,
                            state,
                            &names,
                            out,
                        );
                    } else {
                        step_header!(hosts.first().map(String::as_str).unwrap_or_default());
                    }
                    // The prompt of each host's include result, in front of the first line.
                    for host in asking.drain(..) {
                        prompt!((debugger::Kind::Passed, Map::new()), &host, index);
                    }
                    // Under `free` each host's include is a result of its own, with its line.
                    for those in hosts.chunks(if free { 1 } else { hosts.len().max(1) }) {
                        out.included(&what, those, label.as_deref());
                    }
                    // Measured on ansible-core 2.19.12: one `ok` per host per item, so a
                    // two-item loop over two hosts counts four. The statement's own aggregate
                    // counts nothing, which is what keeps `h1 ok=10` at ten.
                    for host in &hosts {
                        stats.record(host, Outcome::Ok, false);
                    }
                }
            }
            // What was spliced in is marked with the rest, and a role it brought carries
            // variables that can change the mark of a step further down.
            mark_boundaries(&mut next, &definitions, &playbook_dir);
            // A step a host that left will pick the play up at moves with the list, if it sits
            // behind the splice.
            let grown = next.steps.len() - plan.steps().steps.len();
            for at in round
                .resume
                .values_mut()
                .chain(round.revived.iter_mut().map(|(_, at)| at))
            {
                if *at > index {
                    *at += grown;
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
    // A block the step loop never reached for a host that left: it still prints, in the order it
    // would have, in front of what the drain below finds.
    let mut missed: Vec<((String, usize), Unreached)> = unreached.into_iter().collect();
    missed.sort_by_key(|((host, at), _)| (*at, play_hosts.iter().position(|h| h == host)));
    for ((host, at), blame) in missed {
        header_for(at, &plan, &play_hosts, &progress_tx, state, &names, out);
        blame.show(&host, out);
        prompt!(unreachable_ask(), &host, at);
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
                origin,
                index: at,
            } => {
                let blame = Unreached {
                    msg,
                    censored,
                    delegate,
                    origin,
                };
                blame.show(&host, out);
                if let Some(at) = at {
                    prompt!(unreachable_ask(), &host, at);
                }
                unreachable_left!(&host, at);
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
                if round.files_failure(&host, failed) {
                    state.failed_hosts.insert(host);
                }
                keep_links(&mut state.links, links, failed).await;
                coordinator.publish(&state.failed_hosts, &plan.steps().steps);
            }
            // A `meta` action whose step the loop above never reached still did what it did.
            Event::Meta {
                host,
                index: at,
                effect,
            } => {
                let warned = apply_meta(
                    effect,
                    &mut round,
                    &mut coordinator,
                    &MetaContext {
                        host: &host,
                        at,
                        whole,
                        pattern: &play.hosts,
                        plan: &plan,
                        abort: &options.abort,
                        first: crate::compile::first(&plan.steps()),
                    },
                    state,
                    stats,
                );
                for message in warned {
                    out.warning(&message, false);
                }
            }
            // Nobody is left to show the result to: the host goes on with it.
            Event::Debug { reply, .. } => {
                let _ = reply.send(debugger::Answer::Continue);
            }
            Event::Named { host, index, name } => {
                names.insert((host, index), name);
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
                        .is_some_and(|s| s.task.bypasses_host_loop())
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
                header_for(index, &plan, &play_hosts, &progress_tx, state, &names, out);
            }
            // An include whose request arrives here is one the step loop will never reach: it
            // has already ended, because every host of the batch has left it. There is nothing
            // left to splice the steps in for, and nothing left to run them.
            Event::Include { .. } => {}
            event @ (Event::Result { .. }
            | Event::Retrying { .. }
            | Event::Warning { .. }
            | Event::Display { .. }
            | Event::UnreachableIgnored { .. }) => {
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
                header_for(*index, &plan, &play_hosts, &progress_tx, state, &names, out);
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
    let revived = std::mem::take(&mut round.revived);
    Ok(BatchEnd {
        end_play: round.end_play,
        steps: (!revived.is_empty()).then(|| (**plan_tx.borrow()).clone()),
        revived,
        dark: round.dark.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests;
