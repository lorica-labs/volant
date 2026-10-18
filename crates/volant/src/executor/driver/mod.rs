// SPDX-License-Identifier: GPL-3.0-or-later
//! One host's run through a play, from its first step to the moment it leaves the batch.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use volant_protocol::modules::short_name;
use volant_protocol::{BatchOutcome, TaskResult};

use crate::action_plugins::Kind;
use crate::agent::{AgentLink, AgentSource};
use crate::compile::{StepKind, after_failure, first, rescue_target};
use crate::inventory::Host;
use crate::playbook::PlayTask;
use crate::profile::{Phase, micros};
use crate::python::ModulePayload;
use crate::render::Dump;
use crate::template::{Templar, TemplateError};
use crate::transport::{ConnectError, Escalation, Transport};
use crate::vars::{HostVars, VarStore};

use super::coordinator::{Event, MetaEffect, Progress, escalated_links, render_name};
use super::debugger::{self, Hold};
use super::include::{report_include, resolve_include};
use super::prepare::{Item, PlayPlan, Prepared, failure_text, pause_of, prepare, retry_name};
use super::report::report_task;
use super::run::{
    Attempt, DISCOVERED_INTERPRETER, DISCOVERY_HELP, ItemRetry, PluginStart, Polled, Relinker,
    RetryLine, announce_retry, any_retried, break_after, broke_before, chosen_interpreter,
    discovers_interpreter, discovery_warning, fact_targets, failed_task_value, finish,
    judge_attempt, local_timed_out, notified_handlers, pause_lines, poll_job, polls, python_for,
    record_facts, record_inventory_results, record_local_facts, record_registered, refusal,
    requested_interpreter, retry_plans, reuse_or_connect, run_agent_batch, run_local,
    run_plugin_attempts, running_host_vars, silent_discovery, stamp_discovery, step_tasks,
    take_warnings,
};
use super::{LinkKey, RunOptions};

mod boundaries;
mod messages;
mod redo;
mod throttle;
mod timings;
mod unreachable;
mod waits;

pub(in crate::executor) use boundaries::{Definitions, mark_boundaries};
use boundaries::{calls_lookup, meets, strict, writes_facts};
pub(crate) use boundaries::{expressions, strings, task_strings};
pub(in crate::executor) use throttle::Turn as ThrottleTurn;
use timings::file_timings;
use unreachable::{ignore_the_batch, ignore_unreachable};
use waits::RunOnce;

/// The `meta` actions whose `when` the reference does not read: it warns that the task does not
/// support one and runs the action all the same.
const NO_WHEN: [&str; 3] = ["noop", "refresh_inventory", "reset_connection"];

#[expect(
    clippy::too_many_arguments,
    reason = "the driver's whole context; drive_host has a single call site, spawned once per host by the loop above"
)]
#[expect(clippy::too_many_lines)]
pub(super) async fn drive_host(
    host: Host,
    plan: Arc<PlayPlan>,
    agents: AgentSource,
    options: RunOptions,
    templar: Arc<Templar>,
    store: Arc<Mutex<VarStore>>,
    verbosity: u8,
    existing: Vec<(LinkKey, AgentLink)>,
    forks: Arc<Semaphore>,
    progress: watch::Receiver<Progress>,
    tx: mpsc::Sender<Event>,
    start: Option<usize>,
) {
    let name = host.name.clone();
    let mut driver = Driver {
        tx: &tx,
        name: &name,
        plan: &plan,
        progress,
        stop: options.stop.clone(),
        stop_broken: false,
        cleanup: None,
        peer_fatal: None,
    };
    // One entry per target user on this host: the connecting user's own agent, and one more
    // for each `become_user` the play escalates to. Connections this play never uses itself
    // stay in here untouched and go straight back with `Finished`.
    let mut links: HashMap<LinkKey, AgentLink> = existing.into_iter().collect();
    // Which of them this play has already proved alive, with the host's reboot count when it
    // did: one liveness check per connection per play, and one more after each reboot.
    let mut checked: HashMap<LinkKey, u64> = HashMap::new();
    // Ansible's `forks`: one permit per host, held from the connection to the results of a
    // batch. It lives in this binding and releases itself when dropped, so no way out of this
    // function can leak it, whether that is a return, an error, a cancellation or a panic.
    let mut permit: Option<OwnedSemaphorePermit> = None;
    // `host_pinned`: the fork is taken before the first step and kept to the last, so the worker
    // that starts a host is the one that finishes it. Every release below is skipped for it.
    let mut turn = Turn {
        gate: &plan.turn,
        mine: plan
            .queue
            .iter()
            .position(|h| h == &name)
            .unwrap_or_default(),
        passed: !plan.strategy.pinned(),
    };
    if plan.strategy.pinned() {
        if driver.wait_turn(turn.mine).await.is_some() {
            permit = driver.acquire(Arc::clone(&forks)).await;
        }
        turn.pass();
    }
    // Set when the host leaves the run without finishing: the message the recap shows, and
    // whether the task it happened under censors its output.
    let mut unreachable: Option<String> = None;
    let mut unreachable_censored = false;
    let mut unreachable_origin: Option<crate::playbook::Origin> = None;
    // The delegate of the task whose batch could not be reached, for the `[h2 -> h1]` the
    // `UNREACHABLE!` line carries. Measured on ansible-core 2.19.12: the delegating host is the
    // one the recap counts, and the delegate only shows in the line.
    let mut unreachable_delegate: Option<String> = None;
    let mut unreachable_index: Option<usize> = None;
    let mut failed = false;
    // Set when this host leaves the play with nothing to show for it: the one runner of a
    // `run_once` step failed, so this host never ran the task and has no line, and the run
    // still has to exit 2 for it. Measured on ansible-core 2.19.12.
    let mut silent_failure = false;
    // Not always step 0: a block with an empty `block:` list lays its rescue out there, and
    // nothing has failed. The steps in front of the first one are stepped over like any other.
    let mut pos = start.unwrap_or_else(|| first(&plan.steps()));
    // An interrupt here leaves `pos` where it is; the run loop's own `stop` test below is what
    // ends the play for this host, and the `Finished` at the end of this function still goes.
    //
    // A host brought back by `clear_host_errors` starts at the step it left off, and the
    // coordinator has already filed everything in front of it: reporting those steps again would
    // wait at splice points that never come in a round that splices nothing.
    if start.is_none()
        && let Some(grown) = driver.stepped_over(0..pos).await
    {
        pos += grown;
    }
    let mut batch_id = first_batch_id();
    // The handlers this host has asked for and not yet run, as indices into `compiled.handlers`.
    // Never twice: measured, a handler notified by two tasks runs once.
    let mut notified: Vec<usize> = Vec::new();
    // Set while this host is walking the handler steps of one flush point. What it is for is the
    // end of that walk: measured on ansible-core 2.19.12, a handler that notifies a handler
    // **defined before it** runs nothing - neither in that flush nor in the next one - so the
    // notifications a flush raised die with it.
    let mut in_flush = false;
    // Set when a host that failed carries on for its handlers alone, which is what
    // `force_handlers` asks for. Every step but a notified handler is reported and not run.
    let mut handlers_only = false;
    // The step a terminal failure was raised at, read once the host has finished whatever
    // cleanup it owed: it is where `force_handlers` picks the walk back up.
    let mut failed_index: Option<usize> = None;
    // The step a failure was just reported at, with the result it failed with. The `always`
    // section this host is draining on its way out is the other half of a failure's state, and
    // it lives on `Driver::cleanup`; the coordinator still knows one thing about a host, that
    // it failed.
    let mut failed_at: Option<(usize, TaskResult)> = None;
    // Set when `failed_at` is another host's failure under `any_errors_fatal`: this host has no
    // failed task, so a rescue it enters has no `ansible_failed_task` to read.
    let mut peer_failure = false;
    // What `failed` was when the current batch began, which `r` at the debugger prompt puts back,
    // and the arguments the operator edited there for the step it runs again.
    let mut failed_before = failed;
    let mut edited: Option<(usize, debugger::Edits)> = None;

    'run: loop {
        // A host that could not be reached leaves the play, unless the operator asks at the
        // debugger prompt to run the step again.
        if let Some(msg) = unreachable.take() {
            // Whatever went wrong, none of these connections is one to hand to anyone.
            for (_, link) in links.drain() {
                link.shutdown().await;
            }
            if !plan.strategy.pinned() {
                permit = None;
            }
            let gone = Event::Unreachable {
                host: name.clone(),
                msg,
                censored: unreachable_censored,
                delegate: unreachable_delegate.take(),
                origin: unreachable_origin.take(),
                index: unreachable_index,
            };
            let Some((at, edits)) = redo::unreachable(&tx, &plan, gone).await else {
                failed = true;
                break 'run;
            };
            failed = failed_before;
            edited = edits.map(|edits| (at, edits));
            pos = at;
            continue 'run;
        }
        let c = plan.steps();
        let n = c.steps.len();
        // Another host failed at an `any_errors_fatal` step this host has just finished: this
        // one fails there too, through the ordinary failure path - a `rescue` of the enclosing
        // block takes it, and `always` runs - without a line or a recap entry of its own.
        let fatal_at = driver.peer_fatal.take();
        if failed_at.is_none()
            && !handlers_only
            && let Some(at) = fatal_at
        {
            // The batch was lost at that step: nothing rescues, the hosts just leave.
            if driver.progress.borrow().aborted {
                failed = true;
                break 'run;
            }
            failed |= rescue_target(&c, at).is_none();
            failed_at = Some((at, TaskResult::default()));
            peer_failure = true;
        }
        // The fork permit goes back before either failure arm below waits at a splice point: the
        // step loop's own release reads the success successor and never the range a host that
        // has just failed steps over. Only on the failure paths, so a batch that ended cleanly
        // still carries its permit into the next one.
        //
        // The escalated links go back here as well, and here alone among the releases. A host
        // that has just failed is on its way through a rescue or an `always` and out of the
        // play, and what it escalated to is far less likely to be what it needs next than it is
        // on the straight path; it is also the state most likely to be left waiting a long time
        // at a splice point. So this is where an escalated link stops being worth its process on
        // the target.
        if failed_at.is_some() || failed_index.is_some() {
            if !plan.strategy.pinned() {
                permit = None;
            }
            for key in escalated_links(&links) {
                if let Some(link) = links.remove(&key) {
                    tokio::spawn(link.shutdown());
                }
            }
        }
        if let Some((index, result)) = failed_at.take() {
            // `r` at the debugger prompt: the step runs again from where the host stood before
            // it, with the arguments the operator edited, and its first result counts for
            // nothing.
            if let Some(args) = debugger::redo_of(&result) {
                failed = failed_before;
                edited = args.map(|args| (index, args));
                pos = index;
                continue 'run;
            }
            // A handler that failed under `force_handlers` stops the rest: measured on
            // ansible-core 2.19.12, `good handler` does not run behind a `bad handler` that
            // failed, with the flag and without it.
            if handlers_only {
                break 'run;
            }
            // A `rescue` takes this failure: the host jumps into it and stays in the play,
            // which is why `failed` was never set for it. The two variables the recovery
            // reads are written on the way in, and they outlive the block and the play -
            // measured, a task after the block and a task in the next play both still read
            // `ansible_failed_task`.
            if let Some(next) = rescue_target(&c, index) {
                let task = &c.steps[index].task;
                if !std::mem::take(&mut peer_failure) {
                    let mut vars = store.lock().expect("vars lock");
                    vars.set_untrusted_fact(&name, "ansible_failed_task", failed_task_value(task));
                    vars.set_untrusted_fact(
                        &name,
                        "ansible_failed_result",
                        Value::Object(result.0.clone()),
                    );
                }
                let Some(grown) = driver.stepped_over(index + 1..next).await else {
                    break 'run;
                };
                // A rescue written **inside** the `always` this host was draining leaves the
                // drain where it is, moved by whatever the splices grew in front of its end;
                // one written outside ends it. Measured: `rest of the always` runs behind a
                // rescued block written in a cleanup, and stops as soon as an include between
                // the failure and that rescue grows the list.
                driver.cleanup = match driver.cleanup {
                    Some(end) if next < end => Some(end + grown),
                    _ => None,
                };
                pos = next + grown;
            } else {
                peer_failure = false;
                // Measured on ansible-core 2.19.12: a task failing inside a nested block runs
                // the inner `always`, then the outer one, and only then leaves the play. A
                // `cleanup` already in hand is left where it is: a failure raised while
                // draining one `always` still has to finish leaving through the ones outside.
                failed_index = Some(index);
                match after_failure(&c, index) {
                    Some((next, end)) => {
                        let Some(grown) = driver.stepped_over(index + 1..next).await else {
                            break 'run;
                        };
                        pos = next + grown;
                        driver.cleanup = Some(end + grown);
                    }
                    // Nothing left to clean up. Where the host goes from here is the single
                    // test below, so `force_handlers` changes it in one place rather than
                    // two.
                    None => pos = n,
                }
            }
        }
        // The host has run everything it owed and failed on the way. `force_handlers` sends it
        // back over the rest of the play for its handlers alone: measured, the notified handler
        // runs and the recap still counts the failure (`h2 ok=7 failed=1` against `ok=6` without
        // the flag).
        if pos >= n
            && plan.force_handlers
            && !handlers_only
            && let Some(index) = failed_index.take()
        {
            handlers_only = true;
            driver.cleanup = None;
            let Some(next) = driver.advance(index).await else {
                break 'run;
            };
            pos = next;
            continue 'run;
        }
        if pos >= n || driver.stopped() {
            break 'run;
        }
        failed_before = failed;
        // Collect a batch of remote tasks up to the next boundary; report skips and run local
        // tasks as they come, in order.
        let mut batch: Vec<(usize, Vec<Item>)> = Vec::new();
        // The module payload of each step of the batch that has one, by step. Per task rather
        // than per item, because the module is the task's: a loop varies the arguments. Empty
        // for every batch this release runs today, a Python module being refused before the
        // first connection.
        let mut payloads: HashMap<usize, (Box<ModulePayload>, Option<String>)> = HashMap::new();
        // The steps of the batch whose host picks its interpreter itself, each with whether the
        // reference warns about the choice. Those results carry the discovered interpreter.
        let mut discovering: HashMap<usize, bool> = HashMap::new();
        // How long each step of the batch took to render, for the profile.
        let mut prepare_micros: HashMap<usize, u64> = HashMap::new();
        // The escalation every task of the batch shares. A batch is one message to one agent,
        // so it cannot span two target users.
        let mut batch_escalation: Option<Escalation> = None;
        // The host every task of the batch runs on, when a `delegate_to` moved it off this one:
        // the name for the `[h1 -> h3]` its lines carry, and the host the link is opened to and
        // keyed by. One batch is one link, so a second delegate ends the batch like a second
        // target user does.
        let mut batch_delegate: Option<String> = None;
        // The connection every task of the batch shares, resolved from each task's own effective
        // variables. One batch is one link, so a task that resolves a different address, port,
        // user or key ends the batch the way a second target user does. Set on every push, so it
        // holds a value exactly when `batch` does.
        let mut batch_transport: Option<Transport> = None;
        // Set when the batch's one task retries. A retried task is alone in its batch, so this
        // says how the whole batch runs: item by item, attempt by attempt, rather than in one
        // trip to the agent.
        let mut batch_retry: Option<Vec<ItemRetry>> = None;
        // Set when the batch's one task is backed by an action plugin, which is alone in its
        // batch the way a retried task is.
        let mut batch_action: Option<PluginBatch> = None;
        let mut batch_throttle: Option<u64> = None;
        let mut deferred_error: Option<(usize, TemplateError)> = None;
        // The last step of the batch, when where the host goes after it is not yet decided.
        // See the `Prepared::Remote` arm: a step a rescue would catch cannot say what it steps
        // over until its own result is in.
        let mut undecided: Option<usize> = None;
        while pos < n {
            // The step this host just finished failed on another host, and `any_errors_fatal`
            // makes it fail here: nothing else is collected.
            if driver.peer_fatal.is_some() {
                break;
            }
            // The list grew behind this host at a flush point it stepped over, so every index
            // past that point has moved: go back and read the list again rather than walk the
            // one it used to be. Nothing collected so far moved - a flush ends a batch, so every
            // index in `batch` sits in front of it.
            if plan.steps().steps.len() != n {
                break;
            }
            let step = &c.steps[pos];
            let task = &step.task;
            // A permit kept from the batch before goes back in front of every wait this loop can
            // reach, because a permit held across one is a permit the hosts that have to reach
            // that same point cannot have. The batch being empty is what says this is the kept
            // permit and not the one this batch works under: no wait is reached with a batch in
            // hand.
            //
            // The escalated link does **not** go back with it. A permit is the run's own token
            // and another host is waiting for it; an escalated link is a process and a
            // connection on this host alone, which no other host is waiting for, so holding one
            // across a barrier delays nobody. Releasing it costs an `ssh`, a `sudo` and a
            // handshake at the next escalated task, and under the strict `linear` every
            // task raises a barrier. What the links are bounded by is `keep_links` at the end of
            // the play, the failure arm at the top of this loop, and the rule below that a host
            // holds at most one of them at a time.
            if batch.is_empty()
                && permit.is_some()
                && !plan.strategy.pinned()
                && (meets(
                    &plan,
                    &c,
                    pos,
                    strict(options.batching, &store) || plan.lockstep,
                ) || crate::compile::is_splice_point(&step.kind)
                    || driver.steps_over_a_splice_point(&c, pos))
            {
                permit = None;
            }
            // A step an include spliced in for other hosts. This host reports it and shows
            // nothing, exactly as it does for a handler it never notified: the coordinator's
            // barriers open on hosts that have passed a step, not on hosts that had a reason to
            // run it. Measured on ansible-core 2.19.12 with an include a `when` left out for one
            // of two hosts - the other's tasks run with its line alone under their banners.
            let masked_out = step
                .hosts
                .as_ref()
                .is_some_and(|only| !only.iter().any(|h| h == &name));
            // A handler nothing notified is a step this host has nothing to do at: it reports
            // that it is done with it, shows nothing and moves on. Measured on ansible-core
            // 2.19.12 with two hosts and one notifying: the banner shows once, with the line of
            // the host that notified and nothing for the other, whose recap counts no `ok`.
            if let StepKind::Handler(i) = step.kind
                && !notified.contains(&i)
            {
                if !batch.is_empty() {
                    break;
                }
                driver.task_done(pos).await;
                let Some(next) = driver.advance(pos).await else {
                    break 'run;
                };
                pos = next;
                continue;
            }
            // `pause` and `add_host` under `free` and `host_pinned`: measured, the reference ends
            // the run at the first host that reaches one, before its banner.
            if plan.strategy.free()
                && !masked_out
                && let Some(why) = crate::executor::free::bypass_error(task)
            {
                if !batch.is_empty() {
                    break;
                }
                options.abort.raise_plain(why);
                break 'run;
            }
            // A step the hosts of the batch meet in front of: `run_once`, or a text naming another
            // host's state. In front of the flush, include and mask arms rather than behind them,
            // because a host that walked into one of those without stopping here would report a
            // step the others have not reached, and every wait in this file opens on the
            // coordinator's frontier.
            if meets(
                &plan,
                &c,
                pos,
                strict(options.batching, &store) || plan.lockstep,
            ) && pos > 0
            {
                if !batch.is_empty() {
                    break;
                }
                let waiting = Instant::now();
                let waited = driver.wait_for_barrier(pos).await;
                options
                    .profile
                    .phase(Some(&name), Phase::BarrierWait, micros(waiting));
                if waited.is_none() {
                    break 'run;
                }
                // The batch lost more hosts than `max_fail_percentage` allows while this host
                // was waiting: it leaves with the others, and the coordinator has already
                // counted it among the failed.
                if driver.progress.borrow().aborted {
                    failed = true;
                    break 'run;
                }
            }
            // `--step`: one answer for the whole batch, asked in front of the task. A task the
            // operator declined is stepped over like one a mask left this host out of, and shows
            // nothing, not even in the recap.
            let mut declined = false;
            if let Some(stepper) = plan.step.as_ref()
                && matches!(step.kind, StepKind::Task | StepKind::Include(_))
                && !masked_out
                && !handlers_only
            {
                if !batch.is_empty() {
                    break;
                }
                let label = {
                    let live = driver.progress.borrow().clone();
                    let shown = render_name(step, &name, &plan, &live, &templar, &store);
                    match step.role.and_then(|i| c.roles.get(i)) {
                        Some(role) => format!("{} : {shown}", role.name),
                        None => shown,
                    }
                };
                let Some(runs) = stepper
                    .runs(
                        pos,
                        &format!("{}|{:?}", task.name, step.role),
                        &label,
                        &mut driver.stop,
                    )
                    .await
                else {
                    if !driver.stopped() {
                        options
                            .abort
                            .raise_with(250, "EOF when reading a line".to_string());
                    }
                    break 'run;
                };
                declined = !runs;
            }
            let masked_out = masked_out || declined;
            // Who runs a `run_once` step: read, never decided here, because one value written
            // once is what makes every host agree on it. See `Progress::elected`. With `serial`
            // each batch elects its own, which falls out of the coordinator's `live_hosts` being
            // the batch's list and not the play's.
            let runner = driver.progress.borrow().elected.get(&pos).cloned();
            // Measured on ansible-core 2.19.12: `changed: [h1]` alone under the banner, the
            // registered variable and the facts readable on h2 as well, and h2 counting no `ok`
            // for it. Measured again with an include a `when` kept one of three hosts out of:
            // the two the mask holds both read the registered value back, and the third runs
            // only what follows.
            let follower = task.bypasses_host_loop()
                && !declined
                && !handlers_only
                && runner.as_deref().is_some_and(|h| h != name);
            // Leaving a flush point's handlers behind: every index the flush was asked for goes,
            // reached or not, so a handler runs once per notification. Measured on ansible-core
            // 2.19.12, both halves - `first handler` notified twice runs once and does **not**
            // come back at the next flush, and a handler notified by a handler defined **before**
            // it runs in neither flush.
            if in_flush && !matches!(step.kind, StepKind::Handler(_)) {
                notified.clear();
                in_flush = false;
            }
            // A flush point. This is the one step whose successors are not known yet, so the
            // host reports it and then waits for the coordinator to put the handler steps in
            // behind it. Reading the list before that would walk straight past them.
            if let StepKind::Flush { explicit } = step.kind {
                if !batch.is_empty() {
                    break;
                }
                // Measured: an explicit `meta: flush_handlers` shows one `TASK [meta]` per live
                // host with nothing under it, and the three the compiler adds show nothing at
                // all. A host already out of the play shows nothing either.
                //
                // Its `when` is read per host: a host it leaves out shows `skipping:` under the
                // banner and runs no handler at this flush - the ones it notified wait for the
                // next.
                let mut skipped_flush = false;
                if explicit && !handlers_only && !masked_out {
                    if !task.when.is_empty() {
                        let live = driver.progress.borrow().clone();
                        let mut ignored = Vec::new();
                        match prepare(
                            step,
                            &name,
                            &plan,
                            &live,
                            &templar,
                            &store,
                            &options.defaults,
                            &mut ignored,
                        ) {
                            Ok(Prepared::Skipped(_)) => skipped_flush = true,
                            Ok(_) => {}
                            // A `when` that cannot be read fails the statement for this host,
                            // after the splice it still owes the others, like a failed include.
                            Err(err) => {
                                let results =
                                    vec![(None, TaskResult::failed_with(failure_text(&err)))];
                                let rescuable = rescue_target(&c, pos).is_some();
                                if let Some(result) = report_task(
                                    &tx,
                                    &name,
                                    pos,
                                    task,
                                    &results,
                                    &[None],
                                    &[],
                                    &[],
                                    &[],
                                    Dump::No,
                                    rescuable,
                                    None,
                                    Hold::new(&plan, &mut permit),
                                )
                                .await
                                {
                                    failed |= !rescuable;
                                    failed_at = Some((pos, result));
                                }
                                if driver.wait_for_splice(pos, n).await.is_none() {
                                    break 'run;
                                }
                                if failed_at.is_some() {
                                    break;
                                }
                                let Some(next) = driver.advance(pos).await else {
                                    break 'run;
                                };
                                pos = next;
                                break;
                            }
                        }
                    }
                    driver.banner(pos).await;
                    if skipped_flush {
                        driver.meta_skipped(pos, "flush_handlers", None).await;
                    }
                }
                driver.task_done(pos).await;
                // `n` is the length of the list this host is walking, read before the word it
                // just sent could let the coordinator grow it. See `wait_for_splice`. Under
                // `free` a flush no handler can run behind is passed without waiting: the
                // splice would add nothing, and a host kept by its fork must not wait for one
                // that has not started.
                if !driver.nothing_to_splice(&c, pos)
                    && driver.wait_for_splice(pos, n).await.is_none()
                {
                    break 'run;
                }
                // A host the flush's own mask leaves out reports the step and waits for the
                // splice like everyone else, but it has not performed a flush: the handler steps
                // behind it carry that same mask, so it runs none of them, and the line below
                // would otherwise throw away the notifications it is still carrying to the next
                // flush it does reach. A host whose `when` left it out is in the same position,
                // and walks past the handler steps the splice put behind the flush by reporting
                // them.
                if skipped_flush {
                    let grown = plan.steps().steps.len() - n;
                    driver.skipped(pos + 1..pos + 1 + grown).await;
                    let Some(next) = driver.advance(pos + grown).await else {
                        break 'run;
                    };
                    pos = next;
                    break;
                }
                if !masked_out {
                    in_flush = true;
                }
                let Some(next) = driver.advance(pos).await else {
                    break 'run;
                };
                pos = next;
                break;
            }
            // An include statement. Like a flush point its successors are not known yet, so the
            // shape is the same: report, wait for the coordinator to publish the splice, and only
            // then read the list again. The batch has to be empty to get here, which is the
            // liveness half of the splice rule - a host blocked at an index while a step in front
            // of it is still unreported waits for an index the coordinator can never reach.
            if let StepKind::Include(kind) = step.kind {
                if !batch.is_empty() {
                    break;
                }
                let live = driver.progress.borrow().clone();
                // A host outside the mask, running for its handlers alone, or that lost the
                // `run_once` election asks for nothing and shows nothing, and still reports the
                // step and waits: the steps go in behind this index for everyone's list. Measured
                // on ansible-core 2.19.12, `include_tasks` under `run_once: true`: `included:
                // inc.yml for h1` names the one host, the tasks run for h1 alone, h2 counts none.
                let asked = if masked_out || handlers_only || follower {
                    Vec::new()
                } else {
                    let mut warnings = Vec::new();
                    let (groups, shown, waited) = resolve_include(
                        &c,
                        step,
                        kind,
                        &name,
                        &plan,
                        &live,
                        &templar,
                        &store,
                        &options.defaults,
                        &mut warnings,
                        &options.abort,
                    );
                    if !waited.is_zero() && driver.sleep_between(waited).await.is_none() {
                        break 'run;
                    }
                    if options.abort.reason().is_some() {
                        // The statement's banner is the last line the reference prints.
                        let _ = tx
                            .send(Event::Banner {
                                host: name.clone(),
                                index: pos,
                            })
                            .await;
                        break 'run;
                    }
                    for message in warnings {
                        let _ = tx
                            .send(Event::Warning {
                                host: name.clone(),
                                index: pos,
                                message,
                                censored: false,
                            })
                            .await;
                    }
                    let rescuable = !handlers_only && rescue_target(&c, pos).is_some();
                    if let Some(result) =
                        report_include(&tx, &name, pos, task, &shown, rescuable, groups.is_empty())
                            .await
                    {
                        failed |= !rescuable;
                        failed_at = Some((pos, result));
                    }
                    groups
                };
                let _ = tx
                    .send(Event::Include {
                        host: name.clone(),
                        index: pos,
                        groups: asked,
                    })
                    .await;
                driver.task_done(pos).await;
                // Waited for even by a host whose own include just failed: the splice grows every
                // index past this one, and `rescue_target` and `after` are about to be asked for
                // this step on the list the splice leaves behind. A host that skipped the wait
                // would jump to a target computed against the list as it used to be.
                if driver.wait_for_splice(pos, n).await.is_none() {
                    break 'run;
                }
                if failed_at.is_some() {
                    break;
                }
                let Some(next) = driver.advance(pos).await else {
                    break 'run;
                };
                pos = next;
                continue;
            }
            // A host carrying on for its handlers alone runs nothing else: it reports each step
            // it walks past so the others' barriers open, and shows nothing for it. A handler
            // is the exception, and a handler it never notified has already been reported and
            // stepped over above, so one reaching here is one it asked for.
            if handlers_only && !matches!(step.kind, StepKind::Handler(_)) {
                if !batch.is_empty() {
                    break;
                }
                driver.task_done(pos).await;
                let Some(next) = driver.advance(pos).await else {
                    break 'run;
                };
                pos = next;
                continue;
            }
            // A step an include brought in for other hosts: reported, shown nothing, the way a
            // handler this host never notified is. After the flush and include arms on purpose,
            // because those are splice points a host outside the mask still has to report and
            // wait at. A `run_once` step takes the follower arm below instead: reporting it here
            // would publish a verdict for the hosts waiting on a runner that has not run.
            if masked_out && !follower {
                if !batch.is_empty() {
                    break;
                }
                driver.task_done(pos).await;
                let Some(next) = driver.advance(pos).await else {
                    break 'run;
                };
                pos = next;
                continue;
            }
            // A host that lost the `run_once` election waits here for the coordinator to publish
            // what the runner made of the step: what that host registered or set as a fact is
            // written for the whole batch, and reading it a moment too early reads nothing. Below
            // the include and flush arms on purpose - a follower still reports those and waits
            // for the splice, it just asks for nothing.
            if follower {
                if !batch.is_empty() {
                    break;
                }
                let runner = runner.clone().unwrap_or_default();
                let Some(verdict) = driver.wait_for_run_once(&runner, pos).await else {
                    break 'run;
                };
                match verdict {
                    // Measured on ansible-core 2.19.12: a `run_once` task that failed takes
                    // every other host of the play out of it, with no line and no recap entry
                    // of their own - they ran nothing - while the run still exits 2. It holds
                    // with a `rescue` around the task as well: the one host that ran it is
                    // rescued (`rescued=1`) and the others still leave.
                    RunOnce::Failed => {
                        failed = true;
                        silent_failure = true;
                        break 'run;
                    }
                    // The one runner left the play without a verdict, which is what an
                    // unreachable host does. Measured: `fatal: [h1]: UNREACHABLE!` with no
                    // arrow, h2 absent from the recap, exit **4** and not 6 - so this departure
                    // counts for nothing of its own.
                    RunOnce::Gone => {
                        failed = true;
                        break 'run;
                    }
                    RunOnce::Done => {}
                }
                // The runner of a `meta: end_batch` or `end_play` found its condition true: the
                // play is over for this host as well, and neither its `always` sections nor
                // its handlers run.
                if driver.progress.borrow().ending.contains(&pos) {
                    break 'run;
                }
                // The inventory a `meta: refresh_inventory` read no longer holds this host for the
                // play: it leaves where it stands, not failed.
                if driver.progress.borrow().dismissed.contains(&name) {
                    break 'run;
                }
                driver.task_done(pos).await;
                let Some(next) = driver.advance(pos).await else {
                    break 'run;
                };
                pos = next;
                continue;
            }
            let live = driver.progress.borrow().clone();
            // Where a registered variable lands. A `run_once` step is run by one host for the
            // whole batch, so what it registered is written for every live host of the batch:
            // measured on ansible-core 2.19.12, `register: o` under `run_once: true` on h1
            // reads back as `o.stdout` on h2 as well. Every other step writes for its own host.
            let register_hosts: Vec<String> = fact_targets(task, &name, &live.live_hosts);
            // A step calling a lookup starts its own batch. Every step of a batch is rendered
            // before the batch goes out, so a lookup rendered behind a batch in hand would read
            // the controller before the tasks in front of it had run - a file one of them
            // writes, a command's output - where the strict `linear` reads it after them.
            if !batch.is_empty() && calls_lookup(task) {
                break;
            }
            // The banner's name, rendered here, at the step's own point, against the variables
            // this host has now; the coordinator prints it rather than rendering its own later.
            if Templar::is_template(&task.name) {
                let spoken = render_name(step, &name, &plan, &live, &templar, &store);
                let _ = tx
                    .send(Event::Named {
                        host: name.clone(),
                        index: pos,
                        name: spoken,
                    })
                    .await;
            }
            let mut warnings = Vec::new();
            let preparing = Instant::now();
            // The reference never reads the `when` of these, so a condition that cannot be
            // evaluated must not fail the host either.
            let unconditioned;
            let step_read = if step.kind == StepKind::Meta
                && NO_WHEN.contains(&crate::compile::meta_action(task))
            {
                unconditioned = {
                    let mut copy = step.clone();
                    copy.task.when.clear();
                    copy
                };
                &unconditioned
            } else {
                step
            };
            let prepared = prepare(
                step_read,
                &name,
                &plan,
                &live,
                &templar,
                &store,
                &options.defaults,
                &mut warnings,
            );
            let prepared = match debugger::invalid(&step.task) {
                Some(err) => Err(err),
                None => debugger::with_edited(prepared, pos, &mut edited),
            };
            let took = micros(preparing);
            options.profile.phase(Some(&name), Phase::Prepare, took);
            prepare_micros.insert(pos, took);
            // Not censored: measured on ansible-core 2.19.12, the one warning `prepare` can
            // raise quotes the playbook's own `environment` source and never a rendered value,
            // and the reference leaves it in plain sight under `no_log`.
            for message in warnings {
                let _ = tx
                    .send(Event::Warning {
                        host: name.clone(),
                        index: pos,
                        message,
                        censored: false,
                    })
                    .await;
            }
            let Some((prepared, throttle)) = driver
                .read_throttle(step, pos, prepared, &templar, &options.abort)
                .await
            else {
                break 'run;
            };
            match prepared {
                Err(err) => {
                    deferred_error = Some((pos, err));
                    break;
                }
                // A `meta` asks the engine for something rather than the host: it shows a banner
                // and, when its condition does not hold, a `skipping:` line that the recap does
                // not count. Which of the two the reference reads the condition of is
                // `NO_WHEN`'s: for those three it says so and runs the action anyway.
                Ok(prepared) if step.kind == StepKind::Meta => {
                    if !batch.is_empty() {
                        break;
                    }
                    let action = crate::compile::meta_action(task);
                    let ignores_when = NO_WHEN.contains(&action);
                    if ignores_when && !task.when.is_empty() {
                        let _ = tx
                            .send(Event::Warning {
                                host: name.clone(),
                                index: pos,
                                message: format!("{action} task does not support when conditional"),
                                censored: false,
                            })
                            .await;
                    }
                    driver.banner(pos).await;
                    let held = ignores_when || !matches!(prepared, Prepared::Skipped(_));
                    let role = step
                        .role
                        .and_then(|i| c.roles.get(i))
                        .map(|role| role.name.as_str());
                    let mut leaves = false;
                    let mut jump = false;
                    if !held {
                        driver.meta_skipped(pos, action, role).await;
                    } else {
                        match action {
                            "end_host" => {
                                driver.meta(pos, MetaEffect::EndHost).await;
                                leaves = true;
                            }
                            "end_batch" => {
                                driver.meta(pos, MetaEffect::EndBatch).await;
                                leaves = true;
                            }
                            "end_play" => {
                                driver.meta(pos, MetaEffect::EndPlay).await;
                                leaves = true;
                            }
                            "clear_facts" => driver.meta(pos, MetaEffect::ClearFacts).await,
                            "clear_host_errors" => {
                                driver.meta(pos, MetaEffect::ClearHostErrors).await;
                            }
                            "refresh_inventory" => {
                                driver.meta(pos, MetaEffect::RefreshInventory).await;
                            }
                            // The host's links go, the shared connection's master with them: the
                            // next task opens both again.
                            "reset_connection" => {
                                let keys: Vec<LinkKey> =
                                    links.keys().filter(|k| k.host == name).cloned().collect();
                                for key in keys {
                                    checked.remove(&key);
                                    if let Some(link) = links.remove(&key) {
                                        link.shutdown().await;
                                    }
                                    key.transport.stop_shared().await;
                                }
                            }
                            "end_role" => jump = true,
                            // `noop` and `flush_handlers`, whose flush point is not this arm's.
                            _ => {}
                        }
                    }
                    driver.task_done(pos).await;
                    if leaves {
                        break 'run;
                    }
                    // The runner of a `refresh_inventory` learns from the verdict, as the others
                    // do, whether the new inventory still holds it.
                    if held
                        && action == "refresh_inventory"
                        && (driver.wait_for_run_once(&name, pos).await.is_none()
                            || driver.progress.borrow().dismissed.contains(&name))
                    {
                        break 'run;
                    }
                    if jump {
                        // Every step of the role behind this one is stepped over, an `always`
                        // included: measured, the reference skips them all up to the role's end.
                        let Some(role) = step.role else {
                            options.abort.raise(
                                "Cannot execute 'end_role' from outside of a role".to_string(),
                            );
                            break 'run;
                        };
                        let mut target = (pos + 1..c.steps.len())
                            .find(|&i| c.steps[i].role != Some(role))
                            .unwrap_or(c.steps.len());
                        // A host draining an `always` after a failure stops at the end of it.
                        if let Some(end) = driver.cleanup {
                            target = target.min(end);
                        }
                        let Some(grown) = driver.stepped_over(pos + 1..target).await else {
                            break 'run;
                        };
                        if let Some(end) = driver.cleanup.as_mut() {
                            *end += grown;
                        }
                        pos = target + grown;
                    } else {
                        let Some(next) = driver.advance(pos).await else {
                            break 'run;
                        };
                        pos = next;
                    }
                }
                Ok(Prepared::Skipped(mut items)) => {
                    if !batch.is_empty() {
                        break;
                    }
                    // A `break_when` ends the loop at a skipped item as at any other.
                    let mut decided = vec![None; items.len()];
                    if let (Some(at), _) =
                        broke_before(task, &items, &mut decided, &mut 0, items.len(), &templar)
                    {
                        items.truncate(at + 1);
                    }
                    let labels: Vec<Option<String>> =
                        items.iter().map(|i| i.label.clone()).collect();
                    let results: Vec<(Option<Value>, TaskResult)> = items
                        .into_iter()
                        .zip(decided)
                        .map(|(i, r)| (i.element, r.or(i.skipped).unwrap_or_default()))
                        .collect();
                    // A skipped task fails nothing, so no rescue is in question for it, and it
                    // made no attempt to retry.
                    // No delegate on a skipped task: measured on ansible-core 2.19.12, a
                    // `delegate_to` a `when` left out prints `skipping: [h1]` with no arrow.
                    // The one answer it can have is `r` at an `on_skipped` prompt.
                    if let Some(redo) = report_task(
                        &tx,
                        &name,
                        pos,
                        task,
                        &results,
                        &labels,
                        &[],
                        &[],
                        &[],
                        Dump::No,
                        false,
                        None,
                        Hold::new(&plan, &mut permit),
                    )
                    .await
                    {
                        failed_at = Some((pos, redo));
                        continue 'run;
                    }
                    record_registered(
                        &mut store.lock().expect("vars lock"),
                        task,
                        &register_hosts,
                        &results,
                        &[],
                    );
                    let Some(next) = driver.advance(pos).await else {
                        break 'run;
                    };
                    pos = next;
                }
                Ok(Prepared::Local(items, delegate)) => {
                    if !batch.is_empty() {
                        break;
                    }
                    // Held for the task's items and given back before the host waits for anyone:
                    // a verdict it is about to wait for may depend on a host this one is holding
                    // up.
                    let Some(gate) = driver.throttled(pos, throttle).await else {
                        break 'run;
                    };
                    // A controller-side module runs here whatever `delegate_to` says - measured
                    // on ansible-core 2.19.12, a delegated `set_fact` and a delegated `debug`
                    // both run on the controller - so the delegate decides two things and no
                    // more: the name the line shows, and, under `delegate_facts`, whose facts
                    // the module writes.
                    let fact_hosts: Vec<String> = match (task.delegates_facts(), &delegate) {
                        (true, Some(to)) => vec![to.clone()],
                        _ => register_hosts.clone(),
                    };
                    let plans = retry_plans(task, &items, &templar);
                    let pause = pause_of(task, &items, &templar);
                    let mut results = Vec::new();
                    let mut labels = Vec::new();
                    let mut lefts: Vec<Vec<RetryLine>> = Vec::new();
                    let mut names: Vec<String> = Vec::new();
                    for (ii, (item, plan)) in items.iter().zip(&plans).enumerate() {
                        if ii > 0 && driver.pause_for(pause, 1).await.is_none() {
                            break 'run;
                        }
                        names.push(retry_name(task, &item.vars, &templar));
                        let mut mine = Vec::new();
                        let r = if let Some(s) = &item.skipped {
                            s.clone()
                        } else if let Some(refused) = refusal(task) {
                            refused
                        } else if let Err(failed) = plan {
                            failed.clone()
                        } else {
                            let retry = plan.as_ref().ok().and_then(Option::as_ref);
                            let mut attempt = 0;
                            loop {
                                attempt += 1;
                                // The `timeout` keyword holds on the controller as it does
                                // on the agent, per attempt, and zero is no timeout: measured
                                // on ansible-core 2.19.12. A `pause` is the one local module
                                // that can run into it.
                                if short_name(&task.module) == "pause" {
                                    for line in pause_lines(&task.module, item) {
                                        let _ = tx
                                            .send(Event::Display {
                                                host: name.clone(),
                                                index: pos,
                                                line,
                                                debug: false,
                                            })
                                            .await;
                                    }
                                }
                                let ran = run_local(
                                    task,
                                    item,
                                    step,
                                    &templar,
                                    &store,
                                    verbosity,
                                    &mut driver.stop,
                                );
                                let ran = match task.timeout.filter(|t| *t > 0) {
                                    Some(t) => tokio::time::timeout(Duration::from_secs(t), ran)
                                        .await
                                        .unwrap_or_else(|_| Some(local_timed_out(t))),
                                    None => ran.await,
                                };
                                // A pause the run's stop ended: no line and no count, like
                                // every other wait the stop ends.
                                let Some(ran) = ran else {
                                    break 'run;
                                };
                                let Some(retry) = retry else {
                                    break finish(task, item, ran, &templar);
                                };
                                match judge_attempt(task, item, ran, attempt, retry, &templar) {
                                    Attempt::Done(r) => break r,
                                    Attempt::Again(left, failed) => mine.push((left, failed)),
                                }
                                if let Some(failed) = retry.sleep_failure() {
                                    break failed;
                                }
                                if driver.sleep_between(retry.delay).await.is_none() {
                                    break 'run;
                                }
                            }
                        };
                        let mut r = r;
                        let broke = break_after(task, item, &mut r, &templar);
                        results.push((item.element.clone(), r));
                        labels.push(item.label.clone());
                        lefts.push(mine);
                        if broke {
                            break;
                        }
                    }
                    drop(gate);
                    let to_notify = match notified_handlers(&c, task, &items, &templar, &results) {
                        Ok((found, notes)) => {
                            for message in notes {
                                let _ = tx
                                    .send(Event::Warning {
                                        host: name.clone(),
                                        index: pos,
                                        message,
                                        censored: false,
                                    })
                                    .await;
                            }
                            found
                        }
                        Err(reason) => {
                            // The task's banner is the last line the reference prints.
                            driver.banner(pos).await;
                            options.abort.raise(reason);
                            break 'run;
                        }
                    };
                    let warnings = take_warnings(&mut results);
                    for message in &warnings {
                        let _ = tx
                            .send(Event::Warning {
                                host: name.clone(),
                                index: pos,
                                message: message.clone(),
                                censored: task.censors(),
                            })
                            .await;
                    }
                    {
                        let mut vars = store.lock().expect("vars lock");
                        record_registered(&mut vars, task, &register_hosts, &results, &warnings);
                        record_local_facts(
                            &mut vars,
                            task,
                            &items,
                            &fact_hosts,
                            &results,
                            &templar,
                        );
                        record_inventory_results(&mut vars, task, &name, &results);
                    }
                    let rescuable = !handlers_only && rescue_target(&c, pos).is_some();
                    // Whether a body shows at verbosity 0 is each result's own: see
                    // `VERBOSE_ALWAYS`. The task only says how a `debug`'s is cleaned.
                    let dump = dump_of(task);
                    if let Some(result) = report_task(
                        &tx,
                        &name,
                        pos,
                        task,
                        &results,
                        &labels,
                        &items.iter().map(|i| i.ignore_errors).collect::<Vec<_>>(),
                        &lefts,
                        &names,
                        dump,
                        rescuable,
                        delegate.as_deref(),
                        Hold::new(&plan, &mut permit),
                    )
                    .await
                    {
                        failed |= !rescuable;
                        failed_at = Some((pos, result));
                        break;
                    }
                    notified.extend(to_notify);
                    let Some(next) = driver.advance(pos).await else {
                        break 'run;
                    };
                    pos = next;
                }
                Ok(Prepared::Remote(items, escalation, delegate, payload, action, connect)) => {
                    let escalation = escalation.map(|e| *e);
                    // A different target user is a different agent on the host, a different
                    // delegate is a different host entirely, and a different connection is a
                    // different machine even under one name, so any of the three ends the batch
                    // and the next one opens its own link. `pos` does not move, so this task is
                    // the first of that batch.
                    //
                    // The connection is resolved here, from this task's own effective variables,
                    // and not once per batch afterwards: a task's `vars:` changes `ansible_host`,
                    // `ansible_port` or `ansible_connection` without being a boundary of any
                    // other kind, so a batch built from its first task's view alone would run it
                    // on the machine the first task chose.
                    let delegate_name = delegate.as_ref().map(|(name, _)| name.clone());
                    let target = delegate_name.clone().unwrap_or_else(|| name.clone());
                    let target_vars = running_host_vars(delegate.as_ref(), &items);
                    // The host's own map is enough here: every connection setting is a name a
                    // gather strips, so none of them is ever in the facts layer beside it.
                    let vars = &target_vars.map;
                    let transport =
                        match Transport::for_task(&target, vars, &options.defaults, &connect) {
                            Ok(transport) => transport.shared(
                                options.control_dir.as_deref(),
                                &name,
                                delegate_name.as_deref(),
                            ),
                            // The tasks in hand ran under a connection that was resolved; this
                            // one is the next batch's first, where the same failure ends the
                            // host with its own message.
                            Err(_) if !batch.is_empty() => break,
                            // A plugin nobody has fails the task itself, item by item, through
                            // the ordinary failure path: a `rescue` takes it, `ignore_errors`
                            // swallows it, and the host is not unreachable.
                            Err(ref err)
                                if let Some(missing) =
                                    err.downcast_ref::<crate::transport::ConnectionNotFound>() =>
                            {
                                let msg = format!("Task failed: {missing}");
                                let results: Vec<(Option<Value>, TaskResult)> = items
                                    .iter()
                                    .map(|item| {
                                        let mut r = TaskResult::failed_with(msg.clone());
                                        r.0.insert("changed".into(), Value::Bool(false));
                                        (item.element.clone(), r)
                                    })
                                    .collect();
                                let labels: Vec<Option<String>> =
                                    items.iter().map(|i| i.label.clone()).collect();
                                record_registered(
                                    &mut store.lock().expect("vars lock"),
                                    task,
                                    &register_hosts,
                                    &results,
                                    &[],
                                );
                                let rescuable = !handlers_only && rescue_target(&c, pos).is_some();
                                if let Some(result) = report_task(
                                    &tx,
                                    &name,
                                    pos,
                                    task,
                                    &results,
                                    &labels,
                                    &items.iter().map(|i| i.ignore_errors).collect::<Vec<_>>(),
                                    &[],
                                    &[],
                                    dump_of(task),
                                    rescuable,
                                    delegate_name.as_deref(),
                                    Hold::new(&plan, &mut permit),
                                )
                                .await
                                {
                                    failed |= !rescuable;
                                    failed_at = Some((pos, result));
                                    break;
                                }
                                let Some(next) = driver.advance(pos).await else {
                                    break 'run;
                                };
                                pos = next;
                                continue;
                            }
                            // The batch this task would have opened never formed, so the two
                            // fields the `UNREACHABLE!` line reads are set here rather than
                            // below: the arrow names the delegate this task asked for, and
                            // `no_log` censors the reason.
                            Err(err) => {
                                // The task said an unreachable host stays: the reason is
                                // shown and the host moves on to the next step.
                                if task.ignores_unreachable() {
                                    ignore_unreachable(
                                        &tx,
                                        &name,
                                        pos,
                                        task,
                                        format!("{err:#}"),
                                        delegate_name.as_deref(),
                                        &store,
                                        &register_hosts,
                                    )
                                    .await;
                                    let Some(next) = driver.advance(pos).await else {
                                        break 'run;
                                    };
                                    pos = next;
                                    continue;
                                }
                                unreachable_censored = task.censors();
                                unreachable_origin = task.origin.clone();
                                unreachable_delegate = delegate_name;
                                unreachable_index = Some(pos);
                                unreachable = Some(format!("{err:#}"));
                                continue 'run;
                            }
                        };
                    if !batch.is_empty()
                        && (escalation != batch_escalation
                            || delegate_name != batch_delegate
                            || Some(&transport) != batch_transport.as_ref())
                    {
                        break;
                    }
                    let plans = retry_plans(task, &items, &templar);
                    // A loop that pauses or can break runs its items one at a time, like a retried
                    // one: the agent cannot be told to stop after an item it has not answered yet.
                    // A job that is waited for is too: each look at it is a trip of its own.
                    let retried = any_retried(&plans)
                        || !task.break_when.is_empty()
                        || task.loop_pause.is_some()
                        || polls(task);
                    // A retried task runs its items one at a time, each on its own trip to the
                    // agent, so it is alone in its batch: the tasks in hand go out first and
                    // this one opens the next batch. It ends that batch too, through `boundary`.
                    // A task an action plugin backs does the same, one trip per sub-task.
                    let throttled = throttle.is_some();
                    if (retried || action.is_some() || task.ignores_unreachable() || throttled)
                        && !batch.is_empty()
                    {
                        break;
                    }
                    batch_action = action.map(|kind| PluginBatch {
                        kind,
                        asked: requested_interpreter(target_vars),
                        delegate: delegate.clone(),
                    });
                    batch_escalation = escalation;
                    batch_throttle = throttle;
                    batch_delegate = delegate_name;
                    batch_transport = Some(transport);
                    batch_retry = retried.then_some(plans);
                    // A looping task ends the batch because its items travel with
                    // `ignore_errors` set, so the agent runs all of them the way Ansible does.
                    // Only `report_task` may decide the task failed, from the aggregate, and
                    // nothing behind it in the same batch is allowed to run before it has.
                    //
                    // A task that writes facts ends it as well: every task behind it in the batch
                    // is rendered now, before those facts exist, and one that reads them would
                    // run with nothing or with the values from before.
                    let boundary = task.register.is_some()
                        || writes_facts(task)
                        || task.loop_items.is_some()
                        || !task.changed_when.is_empty()
                        || !task.failed_when.is_empty()
                        || batch_retry.is_some()
                        || batch_action.is_some()
                        || task.ignores_unreachable()
                        || throttled
                        || debugger::may_open(task, debugger::by_default(plan.strategy));
                    if let Some(module) = payload {
                        // Off the same variables the connection was resolved from, which is the
                        // point: the module runs where the link goes, so its interpreter has to
                        // come from that host and not from the one delegating the task. What the
                        // value is worth against the agent's list is settled once the link is up.
                        let asked = requested_interpreter(target_vars);
                        payloads.insert(pos, (module, asked));
                    }
                    // A task refused for its raw parameters fails before it is sent anywhere.
                    if refusal(task).is_none() && discovers_interpreter(&task.module, target_vars) {
                        discovering.insert(pos, !silent_discovery(target_vars));
                    }
                    batch.push((pos, items));
                    // Where this host goes after a step a `rescue` would catch depends on how that
                    // step ends, so the batch stops here: moving `pos` now would step over that
                    // very rescue and tell the coordinator so. A flush point between here and
                    // where the step leads ends the batch for its own reason - `advance` would
                    // wait for a splice the coordinator cannot publish without this step's word.
                    // An `any_errors_fatal` step waits for the batch's verdict before it leaves, which
                    // is a wait it can only make once its own result is in.
                    if rescue_target(&c, pos).is_some()
                        || task.fails_all()
                        || driver.steps_over_a_splice_point(&c, pos)
                    {
                        undecided = Some(pos);
                        break;
                    }
                    let Some(next) = driver.advance(pos).await else {
                        break 'run;
                    };
                    pos = next;
                    if boundary {
                        break;
                    }
                }
            }
        }

        if !batch.is_empty() {
            // Every way this batch can end the host's run reports the batch's first task's
            // `no_log`: the connection is opened for that task, and each of the four failures
            // below happens with it still unreported. Measured on ansible-core 2.19.12, the
            // reference censors the `UNREACHABLE!` line of a `no_log` task, reason and all.
            unreachable_censored = c.steps[batch[0].0].task.censors();
            unreachable_origin = c.steps[batch[0].0].task.origin.clone();
            unreachable_delegate = batch_delegate.clone();
            unreachable_index = Some(batch[0].0);
            // A throttled task is alone in its batch. Its host takes its turn before a fork, so
            // one waiting holds none the others need - unless `host_pinned` keeps the fork.
            if batch_throttle.is_some() && !plan.strategy.pinned() {
                permit = None;
            }
            let Some(mut gate) = driver.throttled(batch[0].0, batch_throttle).await else {
                break 'run;
            };
            if permit.is_none() {
                if let Ok(p) = Arc::clone(&forks).acquire_owned().await {
                    permit = Some(p);
                } else {
                    // Nothing in this run closes the semaphore, so this is a bug rather than
                    // a shutdown. Reporting it beats returning as if the host had run.
                    unreachable = Some("the run's fork limit is gone".to_string());
                    continue 'run;
                }
            }
            // The delegate's own connection, never the delegating host's. Measured on
            // ansible-core 2.19.12: a task delegated away from a host that answers nothing runs
            // perfectly well, so the delegating host's link is not opened for it at all.
            let target = batch_delegate.clone().unwrap_or_else(|| name.clone());
            // Resolved task by task inside the loop above, from each task's own effective
            // variables, and part of what ends the batch - so every task here shares this one
            // connection rather than inheriting the first task's. Set on every push, so the
            // `else` is unreachable and says so rather than guessing a connection.
            let Some(transport) = batch_transport.clone() else {
                unreachable = Some("the batch lost its resolved connection".to_string());
                continue 'run;
            };
            let key = LinkKey {
                host: target,
                become_user: batch_escalation.as_ref().map(|e| e.user.clone()),
                become_program: batch_escalation.as_ref().map(Escalation::program),
                transport,
            };
            // One escalated link at a time, now that they are no longer released at every
            // barrier: a batch escalating differently from the one in hand - another
            // `become_user`, another escalation program or flags, another transport - retires the
            // one it is replacing rather than
            // adding to it. So a host holds its own link plus at most one escalated link
            // whatever the number of users the play names, which is what the run was measured
            // for. Closed off-task: `shutdown` grants its agent two seconds and this batch has
            // no reason to wait for a connection it has finished with.
            if key.become_user.is_some() {
                for stale in escalated_links(&links).into_iter().filter(|k| k != &key) {
                    if let Some(link) = links.remove(&stale) {
                        tokio::spawn(link.shutdown());
                    }
                }
            }
            let connecting = Instant::now();
            let connected = reuse_or_connect(
                &mut links,
                &mut checked,
                &key,
                batch_escalation.as_ref(),
                &agents,
                &options,
            )
            .await;
            // `wait_for_connection` is for a host that may be down: it runs over a link that is
            // already lost, and its own loop reports the connection error, as the reference's
            // does, rather than the host being unreachable from the first connect.
            let connected = match connected {
                Err(ConnectError::Unreachable(_))
                    if crate::action_plugins::waits_for_connection(
                        &c.steps[batch[0].0].task.module,
                    ) =>
                {
                    match AgentLink::dead() {
                        Ok(dead) => {
                            links.insert(key.clone(), dead);
                            // Not proved alive: the next batch handshakes again.
                            checked.remove(&key);
                            Ok(links.get_mut(&key).expect("inserted just above"))
                        }
                        Err(err) => Err(ConnectError::Unreachable(err.to_string())),
                    }
                }
                other => other,
            };
            options
                .profile
                .phase(Some(&name), Phase::Connect, micros(connecting));
            let link = match connected {
                Ok(l) => {
                    options.profile.natives(&key.host, l.natives());
                    l
                }
                // The host answered and then refused to escalate, so this is the task failing and
                // not the host going away. `ignore_errors` is deliberately not honoured: the
                // batch never ran, and a run reporting success while having quietly skipped every
                // escalated task is the worst outcome here. Measured on ansible-core 2.19.12: a
                // refused `become_user` is a task failure, so a rescue takes it (`rescued=1`,
                // exit 0).
                Err(ConnectError::Become(msg)) => {
                    let index = batch[0].0;
                    let mut task = c.steps[index].task.clone();
                    task.ignore_errors = Some(crate::playbook::Flag::Fixed(false));
                    // Measured for `Incorrect su password` (scout case `c5w`): the reference
                    // raises it from its connection, so the result is `changed: false` and a
                    // `msg` that already says `Task failed: `. The other refusals keep their shape
                    // until they are measured too.
                    let refused = if msg == crate::transport::INCORRECT_SU_PASSWORD {
                        let mut shaped = TaskResult::failed_with(crate::render::task_failed(&msg));
                        shaped.0.insert("changed".into(), Value::Bool(false));
                        shaped
                    } else {
                        TaskResult::failed_with(msg)
                    };
                    let results = vec![(None, refused)];
                    let rescuable = !handlers_only && rescue_target(&c, index).is_some();
                    let result = report_task(
                        &tx,
                        &name,
                        index,
                        &task,
                        &results,
                        &[None],
                        &[],
                        &[],
                        &[],
                        Dump::No,
                        rescuable,
                        batch_delegate.as_deref(),
                        Hold::new(&plan, &mut permit).with_turn(&mut gate),
                    )
                    .await;
                    failed |= !rescuable;
                    failed_at = Some((index, result.unwrap_or_default()));
                    // Nothing ran, so the fork this host holds goes back before it carries on:
                    // the next round can block at a barrier or at a flush point, and a permit
                    // held across a wait is one the hosts that have to reach that same point
                    // cannot have. With `-f` under the number of live hosts, none of them ever
                    // would.
                    if !plan.strategy.pinned() {
                        permit = None;
                    }
                    continue 'run;
                }
                Err(err) => {
                    if c.steps[batch[0].0].task.ignores_unreachable() {
                        // The turn goes back before the host waits for anyone: `advance` can
                        // wait for a verdict that depends on a host this gate is holding up.
                        drop(gate);
                        if !plan.strategy.pinned() {
                            permit = None;
                        }
                        if !ignore_the_batch(
                            &mut driver,
                            &tx,
                            &name,
                            &c,
                            batch[0].0,
                            err.to_string(),
                            batch_delegate.as_deref(),
                            &store,
                            undecided,
                            &mut pos,
                        )
                        .await
                        {
                            break 'run;
                        }
                        continue 'run;
                    }
                    unreachable = Some(err.to_string());
                    continue 'run;
                }
            };
            let mut received: Vec<Vec<Option<TaskResult>>> = batch
                .iter()
                .map(|(_, items)| vec![None; items.len()])
                .collect();
            // The retry lines of each item of the retried task, empty for every other batch.
            let mut lefts: Vec<Vec<RetryLine>> = batch
                .first()
                .map(|(_, items)| vec![Vec::new(); items.len()])
                .unwrap_or_default();
            // Each item's templated task name, parallel to `lefts` and empty the same way: the
            // retry loop below is the only place with the item's own vars in hand.
            let mut names: Vec<String> = Vec::new();
            // Whether `received` already holds results the conditions have been applied to. The
            // retry loop has to apply them itself, since `until` reads what they decided.
            let mut decided = false;
            // Where `break_when` ended the first task's loop, when it did, and how far its
            // items have been checked for it.
            let mut broken_at: Option<usize> = None;
            let mut checked_to = 0;
            // Lines the agent asked to show while this batch ran. They are shown in full, even
            // under `no_log`, for the same reason the `environment` warning is: every emitter of
            // `FromAgent::Log` carries transport or protocol text and none can carry task data -
            // the agent's own `main.rs` writes two of them before a batch exists, and `runner.rs`
            // writes the third about a frame it could not read, naming the message kind alone.
            //
            // Hiding them would only make a broken control channel say
            // `the output has been hidden...` where it used to say why, which is the one moment
            // an operator most needs the cause. `FromAgent::Log` carries no task index, so the
            // day one of them can quote a task this becomes the batch's own `no_log` - exact
            // under the strict barrier, erring towards hiding under `[volant] batching`.
            let mut logs: Vec<String> = Vec::new();
            // The step each result of a flat batch belongs to, by its position in the batch. The
            // plugin and retry paths send one step, so an empty list means `batch[0]`.
            let mut flat_steps: Vec<usize> = Vec::new();
            // First, so a task a plugin backs can never fall into a path below that would send
            // the plugin's own name to the agent as a module.
            let ended = if let Some(PluginBatch {
                kind,
                asked,
                delegate,
            }) = batch_action.take()
            {
                // The batch rule above keeps a plugin task alone. Should that ever break, the host
                // stops here naming it, rather than running `batch[0]` and dropping the rest.
                let (index, items) = match plugin_step(&batch) {
                    Ok(step) => step,
                    Err(msg) => {
                        unreachable = Some(msg);
                        continue 'run;
                    }
                };
                let step = &c.steps[*index];
                let interpreters = link.interpreters().to_vec();
                // Out of the map while the plugin holds it, so that a plugin reconnecting can
                // drop the host's other links, and back in once the task is done, fresh or not.
                let Some(mut owned) = links.remove(&key) else {
                    unreachable = Some("the batch lost its connection".to_string());
                    continue 'run;
                };
                let mut relink = Relinker {
                    links: &mut links,
                    checked: &mut checked,
                    reboots: &options.reboots,
                    key: &key,
                    escalation: batch_escalation.as_ref(),
                    agents: &agents,
                    defaults: &options.defaults,
                };
                let playbook_dir = store
                    .lock()
                    .expect("vars lock")
                    .playbook_dir()
                    .to_path_buf();
                let mut stopped = false;
                let pause = pause_of(&step.task, items, &templar);
                let mut outcome = Ok(BatchOutcome::Completed);
                // Under `retries`/`until` the attempts judge each result themselves, since
                // `until` reads what `changed_when` and `failed_when` decided.
                let plans = batch_retry.clone();
                if plans.is_some() {
                    decided = true;
                    names = items
                        .iter()
                        .map(|item| retry_name(&step.task, &item.vars, &templar))
                        .collect();
                }
                for (ii, item) in items.iter().enumerate() {
                    let (at, passed) = broke_before(
                        &step.task,
                        items,
                        &mut received[0],
                        &mut checked_to,
                        ii,
                        &templar,
                    );
                    if driver.pause_for(pause, passed).await.is_none() {
                        stopped = true;
                        break;
                    }
                    if at.is_some() {
                        broken_at = at;
                        break;
                    }
                    if item.skipped.is_some() {
                        continue;
                    }
                    if let Some(refused) = refusal(&step.task) {
                        received[0][ii] = Some(refused);
                        continue;
                    }
                    let retry = match plans.as_ref().map(|p| &p[ii]) {
                        Some(Err(failed)) => {
                            received[0][ii] = Some(failed.clone());
                            continue;
                        }
                        Some(Ok(retry)) => retry.as_ref(),
                        None => None,
                    };
                    let mut warnings = Vec::new();
                    let start = PluginStart {
                        kind,
                        running_vars: running_host_vars(
                            delegate.as_ref(),
                            std::slice::from_ref(item),
                        ),
                        delegated: delegate.is_some(),
                        escalated: batch_escalation.is_some(),
                        local: matches!(key.transport, Transport::Local),
                        templar: &templar,
                        origin: &step.origin,
                        playbook_dir: &playbook_dir,
                        tx: &tx,
                        index: *index,
                    };
                    let ran = run_plugin_attempts(
                        &mut owned,
                        &mut relink,
                        &name,
                        &mut batch_id,
                        &start,
                        &step.task,
                        item,
                        retry,
                        plan.python.as_deref(),
                        &interpreters,
                        asked.as_deref(),
                        &mut driver.stop,
                        &mut driver.stop_broken,
                        &mut logs,
                        &mut warnings,
                        &mut lefts[ii],
                    )
                    .await;
                    match ran {
                        // Stopped between two attempts: nothing more runs, as for every other
                        // wait the stop ends.
                        Ok(None) => {
                            stopped = true;
                            break;
                        }
                        // An item that does not retry comes back as the module answered, and
                        // the other items' `decided` results have had their conditions.
                        Ok(Some(result)) if decided && retry.is_none() => {
                            received[0][ii] = Some(finish(&step.task, item, result, &templar));
                        }
                        Ok(Some(result)) => received[0][ii] = Some(result),
                        // The link lost or the run interrupted: this item has no result and the
                        // ones behind it do not run, exactly as for any batch the agent did not
                        // finish.
                        Err(ended) => {
                            outcome = ended;
                            break;
                        }
                    }
                    // Shown under this task, the way every other warning is. Not censored: the
                    // plugins raise nothing but the name of an argument they dropped.
                    for message in warnings {
                        let _ = tx
                            .send(Event::Warning {
                                host: name.clone(),
                                index: *index,
                                message,
                                censored: false,
                            })
                            .await;
                    }
                }
                if broken_at.is_none() && !stopped && outcome.is_ok() {
                    let (at, passed) = broke_before(
                        &step.task,
                        items,
                        &mut received[0],
                        &mut checked_to,
                        items.len(),
                        &templar,
                    );
                    broken_at = at;
                    // The pause after the last item is owed to no one.
                    let owed = if at.is_some() {
                        passed
                    } else {
                        passed.saturating_sub(1)
                    };
                    stopped = driver.pause_for(pause, owed).await.is_none();
                }
                // The stand-in for a host that never answered is not kept: the next batch connects.
                if owned.is_stand_in() {
                    tokio::spawn(owned.shutdown());
                } else {
                    links.insert(key.clone(), owned);
                }
                if stopped {
                    if let Some(link) = links.get_mut(&key) {
                        file_timings(
                            link,
                            &options.profile,
                            &name,
                            &c,
                            &[],
                            batch[0].0,
                            &prepare_micros,
                        );
                    }
                    break 'run;
                }
                outcome
            } else if let Some(plans) = batch_retry.clone() {
                decided = true;
                let (index, items) = &batch[0];
                let task = &c.steps[*index].task;
                names = items
                    .iter()
                    .map(|item| retry_name(task, &item.vars, &templar))
                    .collect();
                let mut outcome = Ok(BatchOutcome::Completed);
                let pause = pause_of(task, items, &templar);
                // Once for the step, never once per item: the payload and the interpreter are
                // read off the step and the host, and nothing about an item can change either.
                let chosen = payloads
                    .get(index)
                    .map(|(_, asked)| chosen_interpreter(asked.as_deref(), link.interpreters()));
                let python = python_for(
                    &task.module,
                    payloads.get(index).map(|(module, _)| &**module),
                    chosen.as_ref(),
                );
                // The same helper the batched path uses, for the same reason: a step that cannot
                // travel - no interpreter for its payload, or no payload for a module that needs
                // one - reports that per item from one place. Reported nowhere, an item leaves
                // its result `None`, the reporting loop below reads that as a host it never
                // reached, and the play finishes `ok` having run nothing.
                //
                // Item by item, in order, each one's attempts finished before the next one
                // starts: measured on ansible-core 2.19.12 with a two-item loop whose first item
                // passed and whose second needed two attempts - the retry lines of the second
                // sit between the two result lines, so the items do not retry together.
                'items: for (ii, built) in step_tasks(task, items, &python, &mut received[0]) {
                    let item = &items[ii];
                    let (at, passed) =
                        broke_before(task, items, &mut received[0], &mut checked_to, ii, &templar);
                    if driver.pause_for(pause, passed).await.is_none() {
                        file_timings(
                            link,
                            &options.profile,
                            &name,
                            &c,
                            &[],
                            batch[0].0,
                            &prepare_micros,
                        );
                        break 'run;
                    }
                    if at.is_some() {
                        broken_at = at;
                        break 'items;
                    }
                    let retry = match &plans[ii] {
                        Err(failed) => {
                            received[0][ii] = Some(failed.clone());
                            continue 'items;
                        }
                        Ok(retry) => retry.as_ref(),
                    };
                    let mut attempt = 0;
                    loop {
                        attempt += 1;
                        batch_id += 1;
                        let (mut flat, ended_one) = run_agent_batch(
                            link,
                            &name,
                            batch_id,
                            vec![built.clone()],
                            plan.python.as_deref(),
                            &[],
                            &mut driver.stop,
                            &mut driver.stop_broken,
                            &mut logs,
                        )
                        .await;
                        let raw = flat.pop().flatten();
                        if !matches!(
                            ended_one,
                            Ok(BatchOutcome::Completed | BatchOutcome::Failed { .. })
                        ) {
                            outcome = ended_one;
                            break 'items;
                        }
                        // The agent ended without a result for this item. Left unreported, the
                        // way the loop below leaves a task the agent never reached.
                        let Some(raw) = raw else { continue 'items };
                        let raw = if polls(task) {
                            match poll_job(
                                link,
                                &name,
                                &mut batch_id,
                                task,
                                item,
                                raw,
                                &mut driver.stop,
                                &mut driver.stop_broken,
                                &mut logs,
                                &tx,
                                *index,
                            )
                            .await
                            {
                                Polled::Done(result) => result,
                                Polled::Stopped => {
                                    file_timings(
                                        link,
                                        &options.profile,
                                        &name,
                                        &c,
                                        &[],
                                        batch[0].0,
                                        &prepare_micros,
                                    );
                                    break 'run;
                                }
                                Polled::Ended(ended) => {
                                    outcome = ended;
                                    break 'items;
                                }
                            }
                        } else {
                            raw
                        };
                        let Some(retry) = retry else {
                            received[0][ii] = Some(finish(task, item, raw, &templar));
                            continue 'items;
                        };
                        match judge_attempt(task, item, raw, attempt, retry, &templar) {
                            Attempt::Done(r) => {
                                received[0][ii] = Some(r);
                                continue 'items;
                            }
                            // As it happens for a job that is waited for: see the plugin path.
                            Attempt::Again(left, failed) if polls(task) => {
                                announce_retry(
                                    &tx,
                                    &name,
                                    *index,
                                    retry_name(task, &item.vars, &templar),
                                    task,
                                    left,
                                    failed,
                                )
                                .await;
                            }
                            Attempt::Again(left, failed) => lefts[ii].push((left, failed)),
                        }
                        if let Some(failed) = retry.sleep_failure() {
                            received[0][ii] = Some(failed);
                            continue 'items;
                        }
                        if driver.sleep_between(retry.delay).await.is_none() {
                            file_timings(
                                link,
                                &options.profile,
                                &name,
                                &c,
                                &[],
                                batch[0].0,
                                &prepare_micros,
                            );
                            break 'run;
                        }
                    }
                }
                if broken_at.is_none()
                    && matches!(
                        outcome,
                        Ok(BatchOutcome::Completed | BatchOutcome::Failed { .. })
                    )
                {
                    let (at, passed) = broke_before(
                        task,
                        items,
                        &mut received[0],
                        &mut checked_to,
                        items.len(),
                        &templar,
                    );
                    broken_at = at;
                    let owed = if at.is_some() {
                        passed
                    } else {
                        passed.saturating_sub(1)
                    };
                    if driver.pause_for(pause, owed).await.is_none() {
                        file_timings(
                            link,
                            &options.profile,
                            &name,
                            &c,
                            &[],
                            batch[0].0,
                            &prepare_micros,
                        );
                        break 'run;
                    }
                }
                outcome
            } else {
                batch_id += 1;
                // Flat list for the agent, with a map back to (task, item).
                let mut tasks = Vec::new();
                let mut origin = Vec::new();
                for (bi, (index, items)) in batch.iter().enumerate() {
                    let task = &c.steps[*index].task;
                    // Per step, never per batch: a task's own `vars:` may name an interpreter,
                    // and a batch holds whatever shares a connection, not whatever shares an
                    // interpreter. Reading the batch's first step for all of them would run one
                    // task's module under another task's Python.
                    let chosen = payloads.get(index).map(|(_, asked)| {
                        chosen_interpreter(asked.as_deref(), link.interpreters())
                    });
                    let python = python_for(
                        &task.module,
                        payloads.get(index).map(|(module, _)| &**module),
                        chosen.as_ref(),
                    );
                    // A step that cannot travel - no interpreter for its payload, or no payload
                    // for a module that needs one - reports that per item, from inside
                    // `step_tasks`, and the rest of the batch goes out as it would have.
                    for (ii, built) in step_tasks(task, items, &python, &mut received[bi]) {
                        tasks.push(built);
                        origin.push((bi, ii));
                    }
                }
                let (flat, ended) = run_agent_batch(
                    link,
                    &name,
                    batch_id,
                    tasks,
                    plan.python.as_deref(),
                    &[],
                    &mut driver.stop,
                    &mut driver.stop_broken,
                    &mut logs,
                )
                .await;
                for (k, result) in flat.into_iter().enumerate() {
                    if let Some(&(bi, ii)) = origin.get(k) {
                        received[bi][ii] = result;
                    }
                }
                flat_steps = origin.iter().map(|&(bi, _)| batch[bi].0).collect();
                ended
            };
            if let Some(link) = links.get_mut(&key) {
                file_timings(
                    link,
                    &options.profile,
                    &name,
                    &c,
                    &flat_steps,
                    batch[0].0,
                    &prepare_micros,
                );
            }
            // Queued under the batch's first step, in front of the results it belongs with,
            // rather than written as it arrived: the coordinator owns everything a task shows.
            for message in logs.drain(..) {
                let _ = tx
                    .send(Event::Warning {
                        host: name.clone(),
                        index: batch[0].0,
                        message,
                        censored: false,
                    })
                    .await;
            }
            // Report every task of the batch in order; tasks the agent never reached after a
            // failure are not reported at all, as in Ansible. Whether `undecided` was actually
            // reported is tracked rather than assumed, because the agent can end the batch `Ok`
            // without a result for it and advancing past a step with no `TaskDone` behind it
            // stalls every barrier behind it.
            let mut undecided_reported = false;
            let mut any_reported = false;
            let found = links
                .get(&key)
                .and_then(|link| link.interpreters().first().cloned());
            for (bi, (index, items)) in batch.iter().enumerate() {
                let task = &c.steps[*index].task;
                let mut results = Vec::new();
                let mut labels = Vec::new();
                let mut reached = true;
                for (ii, item) in items.iter().enumerate() {
                    if bi == 0 && broken_at.is_some_and(|at| ii > at) {
                        break;
                    }
                    let r = match (&item.skipped, received[bi][ii].take()) {
                        (Some(s), found) => found.unwrap_or_else(|| s.clone()),
                        // The retry loop has already applied `changed_when` and `failed_when`:
                        // `until` reads what they decided, so applying them twice would judge a
                        // result that is not the module's any more.
                        (None, Some(r)) if decided => r,
                        (None, Some(r)) => finish(task, item, r, &templar),
                        (None, None) => {
                            reached = false;
                            break;
                        }
                    };
                    results.push((item.element.clone(), r));
                    labels.push(item.label.clone());
                }
                if !reached && results.is_empty() {
                    break;
                }
                let to_notify = match notified_handlers(&c, task, items, &templar, &results) {
                    Ok((found, notes)) => {
                        for message in notes {
                            let _ = tx
                                .send(Event::Warning {
                                    host: name.clone(),
                                    index: *index,
                                    message,
                                    censored: false,
                                })
                                .await;
                        }
                        found
                    }
                    Err(reason) => {
                        // The task's banner is the last line the reference prints.
                        driver.banner(*index).await;
                        options.abort.raise(reason);
                        break 'run;
                    }
                };
                let live = driver.progress.borrow().live_hosts.clone();
                let targets = fact_targets(task, &name, &live);
                let fact_hosts = match (task.delegates_facts(), &batch_delegate) {
                    (true, Some(to)) => std::slice::from_ref(to),
                    _ => &targets[..],
                };
                // The host the module ran on decides whether anything is discovered: it is the
                // delegate's interpreter that is asked about, and that the fact is kept for.
                let ran_on = batch_delegate.as_deref().unwrap_or(&name);
                let due = match (discovering.get(index), &found) {
                    (Some(&warns), Some(path))
                        if {
                            let vars = store.lock().expect("vars lock");
                            // The reference gives the implicit `localhost` the controller's own
                            // Python as its interpreter, so nothing is discovered there.
                            !vars.is_implicit_localhost(ran_on)
                                && !vars.all_gathered(
                                    std::slice::from_ref(&ran_on.to_string()),
                                    DISCOVERED_INTERPRETER,
                                )
                        } =>
                    {
                        Some((warns, path))
                    }
                    _ => None,
                };
                // The first result that reached the host: a skipped item ran nothing.
                let first = results
                    .iter()
                    .zip(items)
                    .position(|(_, item)| item.skipped.is_none());
                let discovered = match (due, first) {
                    (Some((warns, path)), Some(i)) => {
                        // Measured on ansible-core 2.19.12: a delegated task returns and keeps the
                        // fact only under `delegate_facts`; the warning comes either way.
                        // And only on the first attempt: a retried task's final result is a later
                        // attempt's, which found the interpreter already known. The warning stays.
                        let retried = bi == 0 && lefts.get(i).is_some_and(|left| !left.is_empty());
                        // Nor on a polled job's, which is the job's last status: measured, it
                        // carries no fact and the next task discovers the interpreter again.
                        if (batch_delegate.is_none() || task.delegates_facts())
                            && !retried
                            && !polls(task)
                        {
                            stamp_discovery(&mut results[i].1, path);
                        }
                        Some(warns)
                    }
                    _ => None,
                };
                let mut warnings = take_warnings(&mut results);
                for message in &warnings {
                    let _ = tx
                        .send(Event::Warning {
                            host: name.clone(),
                            index: *index,
                            message: message.clone(),
                            censored: task.censors(),
                        })
                        .await;
                }
                // Not censored by `no_log`: measured on ansible-core 2.19.12, the line names the
                // host and the path and nothing the task returned, and it shows on a hidden task.
                // Registered without the pointer to the documentation the line ends with.
                if let (Some(true), Some(path)) = (discovered, &found) {
                    let text = discovery_warning(&name, path);
                    let _ = tx
                        .send(Event::Warning {
                            host: name.clone(),
                            index: *index,
                            message: format!("{text} {DISCOVERY_HELP}"),
                            censored: false,
                        })
                        .await;
                    warnings.push(text);
                }
                let removed = {
                    // One lock for both: a reader between them would see a host that had
                    // registered a result without holding the facts that came in it.
                    let mut vars = store.lock().expect("vars lock");
                    record_registered(&mut vars, task, &targets, &results, &warnings);
                    // Only here, and never on the `run_local` path above: this is where a
                    // managed host's own words arrive, and `set_fact` writes its own facts with
                    // the trust each of them earned. Under `delegate_facts` they are the
                    // delegate's, as on the controller-side path; the registered result stays
                    // with the host the task was written for.
                    let removed = record_facts(&mut vars, fact_hosts, &results);
                    // What the reference's `gather_facts` action adds, and what `gathering =
                    // smart` looks for in the cache.
                    if task.name == "Gathering Facts"
                        && task.module == "ansible.legacy.setup"
                        && !results.iter().any(|(_, r)| r.failed())
                    {
                        let mark = Map::from_iter([(
                            "_ansible_facts_gathered".to_string(),
                            Value::Bool(true),
                        )]);
                        for host in fact_hosts {
                            vars.gather_facts(host, &mark);
                        }
                    }
                    removed
                };
                // A name, never a value, so there is nothing in it for `no_log` to hide.
                for key in removed {
                    let _ = tx
                        .send(Event::Warning {
                            host: name.clone(),
                            index: *index,
                            message: format!("Removed restricted key from module data: {key}"),
                            censored: false,
                        })
                        .await;
                }
                let rescuable = !handlers_only && rescue_target(&c, *index).is_some();
                any_reported = true;
                let retried: &[Vec<RetryLine>] = if bi == 0 { &lefts } else { &[] };
                let retried_names: &[String] = if bi == 0 { &names } else { &[] };
                if let Some(result) = report_task(
                    &tx,
                    &name,
                    *index,
                    task,
                    &results,
                    &labels,
                    &items.iter().map(|i| i.ignore_errors).collect::<Vec<_>>(),
                    retried,
                    retried_names,
                    dump_of(task),
                    rescuable,
                    batch_delegate.as_deref(),
                    Hold::new(&plan, &mut permit).with_turn(&mut gate),
                )
                .await
                {
                    failed |= !rescuable;
                    failed_at = Some((*index, result));
                    break;
                }
                notified.extend(to_notify);
                if undecided == Some(*index) {
                    undecided_reported = true;
                }
            }
            // The results are in and reported, so the next host may start. Only the permit goes
            // back; the links stay, for the reason the release at the top of the step loop
            // gives. The three ways out of here each close the escalated ones on their own: a
            // failure through the arm at the top of this loop, the end of the play through
            // `keep_links`.
            drop(gate);
            let carries_on =
                failed_at.is_none() && deferred_error.is_none() && undecided.is_none() && pos < n;
            if !carries_on && !plan.strategy.pinned() {
                permit = None;
            }
            match ended {
                Err(msg) => {
                    // The link died with the task unanswered and the task said an unreachable
                    // host stays. The dead link is dropped so the next step connects again.
                    if !any_reported
                        && failed_at.is_none()
                        && c.steps[batch[0].0].task.ignores_unreachable()
                    {
                        links.remove(&key);
                        checked.remove(&key);
                        if !ignore_the_batch(
                            &mut driver,
                            &tx,
                            &name,
                            &c,
                            batch[0].0,
                            msg,
                            batch_delegate.as_deref(),
                            &store,
                            undecided,
                            &mut pos,
                        )
                        .await
                        {
                            break 'run;
                        }
                        continue 'run;
                    }
                    unreachable = Some(msg);
                    continue 'run;
                }
                Ok(BatchOutcome::Cancelled { .. }) => break 'run,
                Ok(_) => {}
            }
            // The step held back above did not fail, so it steps over its block's rescue after
            // all, and only now is that true enough to tell the coordinator. Guarded by
            // `undecided_reported`: a step this loop never actually reported has not told us
            // that, whatever `ended` says about the rest of the batch.
            if failed_at.is_none()
                && undecided_reported
                && let Some(step) = undecided
            {
                let Some(next) = driver.advance(step).await else {
                    break 'run;
                };
                pos = next;
            }
        }

        if failed_at.is_none()
            && let Some((index, err)) = deferred_error
        {
            let task = &c.steps[index].task;
            // The reference's own prefix on the `msg` of a task that dies before it runs - a
            // `when` it cannot evaluate, arguments it cannot render. Measured: a failing `when`
            // reports `Task failed: Error while evaluating conditional: ...`, so a playbook
            // testing `'Task failed' in result.msg` must still see it. `failed_when` does not get
            // it: there the reference leaves `msg` empty and fills `failed_when_result`.
            let mut result = TaskResult::failed_with(failure_text(&err));
            if let Some(causes) =
                debugger::causes(task, &err.0).or_else(|| throttle::causes(task, &err.0))
            {
                result.0.insert(crate::render::CHAIN_KEY.into(), causes);
            } else if let Some(detail) = err.0.strip_prefix("Task failed: ")
                && detail
                    .starts_with(super::prepare::NOTIFY_FAILED.trim_start_matches("Task failed: "))
            {
                // Measured: one cause, the keyword's sentence at where its value was written.
                result.0.insert(
                    crate::render::CHAIN_KEY.into(),
                    serde_json::json!([crate::render::cause(
                        detail,
                        task.value_origins.get("notify"),
                        None
                    )]),
                );
            }
            let results = vec![(None, result)];
            // Measured on ansible-core 2.19.12: an undefined variable in a task's arguments
            // fails that task, and a rescue around it takes the failure with the error's own
            // sentence in `ansible_failed_result.msg`.
            let rescuable = !handlers_only && rescue_target(&c, index).is_some();
            if let Some(result) = report_task(
                &tx,
                &name,
                index,
                task,
                &results,
                &[None],
                &[],
                &[],
                &[],
                dump_of(task),
                rescuable,
                None,
                Hold::new(&plan, &mut permit),
            )
            .await
            {
                failed |= !rescuable;
                failed_at = Some((index, result));
            } else {
                let Some(next) = driver.advance(index).await else {
                    break 'run;
                };
                pos = next;
            }
        }
    }
    drop(permit);
    // A link this driver opened to somebody else's host - a `delegate_to` - goes no further than
    // this play, because the run keeps exactly one link per host and a second one handed back for
    // a host whose own driver also handed one back would race for the same key. Awaited rather
    // than spawned: at the end of the last play the runtime can be dropped before a detached task
    // runs, leaving the delegate's agent waiting on a connection nobody closes.
    let mut closing = tokio::task::JoinSet::new();
    for key in links
        .keys()
        .filter(|k| k.host != name)
        .cloned()
        .collect::<Vec<_>>()
    {
        if let Some(link) = links.remove(&key) {
            closing.spawn(link.shutdown());
        }
    }
    while closing.join_next().await.is_some() {}
    // The healthy connection to the host itself outlives the play; `keep_links` decides which
    // of these that is and closes the rest. The run closes what is left once, before the recap.
    let _ = tx
        .send(Event::Finished {
            host: name,
            failed,
            silent_failure,
            links: links.into_iter().collect(),
        })
        .await;
}

/// The one step of a batch an action plugin backs, or the controller error that ends the host
/// when the batch holds anything else: the plugin branch runs one step, so a second one would be
/// reported by nobody and never run.
fn plugin_step<T>(batch: &[(usize, T)]) -> Result<&(usize, T), String> {
    match batch {
        [only] => Ok(only),
        _ => Err(format!(
            "a task backed by an action plugin has to be alone in its batch, and this one holds steps {:?}",
            batch.iter().map(|(index, _)| index).collect::<Vec<_>>()
        )),
    }
}

/// What a batch whose one task an action plugin backs needs to run it.
struct PluginBatch {
    kind: Kind,
    /// The interpreter the host the module runs on asked for, read off its variables.
    asked: Option<String>,
    /// That host's name and variables, when it is a delegate.
    delegate: Option<(String, HostVars)>,
}

/// Everything one host's driver carries from one step to the next: who it is, what it is
/// walking, and the watches every wait below opens on.
///
/// What a wait needs of this host is the same at every step, so it lives here rather than
/// travelling through each of them. Only `cleanup` moves underneath it.
struct Driver<'a> {
    /// Where every word this host owes the coordinator, and every line it shows, goes.
    tx: &'a mpsc::Sender<Event>,
    name: &'a str,
    plan: &'a PlayPlan,
    /// The batch's shared progress, written by the coordinator alone.
    progress: watch::Receiver<Progress>,
    stop: watch::Receiver<bool>,
    /// Set once the stop watch's sender is gone, so a dropped sender is never read as an
    /// interrupt and the selects below stop polling a branch that would otherwise resolve
    /// immediately forever.
    stop_broken: bool,
    /// The index the `always` section this host is draining ends at, when it is draining one.
    /// The one index it carries that sits past a splice point, so the one a splice moves.
    cleanup: Option<usize>,
    /// The `any_errors_fatal` step another host failed at, found when this host finished it. Read
    /// at the top of the host's loop, where it becomes a failure at that step.
    peer_fatal: Option<usize>,
}

impl Driver<'_> {
    /// Whether the run has been interrupted.
    fn stopped(&self) -> bool {
        *self.stop.borrow()
    }
}

/// A `host_pinned` host's place in the queue for forks. Dropping it hands the place to the next
/// host, so a host that leaves before it queued - interrupted, or panicked - holds nobody up.
struct Turn<'a> {
    gate: &'a watch::Sender<usize>,
    mine: usize,
    passed: bool,
}

impl Turn<'_> {
    fn pass(&mut self) {
        if !std::mem::replace(&mut self.passed, true) {
            let next = self.mine + 1;
            self.gate
                .send_modify(|reached| *reached = (*reached).max(next));
        }
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.pass();
    }
}

/// Where one host's batch ids start for one play: a range 2^32 wide, handed out once per
/// process, so no id is used twice on a link. Links outlive the play that opened them and are
/// shared by every driver that reaches the host, and a batch reads only the frames of its own id:
/// an id seen again would take a late frame left on the link for its own.
fn first_batch_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1 << 32, std::sync::atomic::Ordering::Relaxed)
}

/// The chain of a task whose arguments a plugin failed to render, as the reference prints it:
/// `Finalization of task args for '<action>' failed.` at the action's key, then the value it was
/// resolving at that value.
pub(super) fn finalization_causes(task: &PlayTask, message: &str) -> Option<Value> {
    let rest = message.strip_prefix(super::prepare::FINALIZING)?;
    let at = rest.find(&format!(": {}", crate::template::RESOLVING))?;
    let head = format!("{}{}.", super::prepare::FINALIZING, &rest[..at]);
    let resolving = &rest[at + 2..];
    let key = resolving[crate::template::RESOLVING.len()..]
        .strip_prefix('\'')?
        .split('\'')
        .next()?;
    let origin = |name: &str| task.value_origins.get(name);
    Some(Value::Array(vec![
        crate::render::cause(&head, origin("action"), None),
        crate::render::cause(resolving, origin(&format!("args.{key}")), None),
    ]))
}

/// How the reference's callback cleans this task's result lines.
fn dump_of(task: &PlayTask) -> Dump {
    match short_name(&task.module) {
        "debug" => Dump::Debug,
        "setup" | "gather_facts" => Dump::Setup,
        _ => Dump::No,
    }
}

#[cfg(test)]
mod tests;
