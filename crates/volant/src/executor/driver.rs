// SPDX-License-Identifier: GPL-3.0-or-later
//! One host's run through a play, from its first step to the moment it leaves the batch.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use volant_protocol::modules::short_name;
use volant_protocol::{BatchOutcome, TaskResult};

use crate::agent::{AgentLink, AgentSource};
use crate::compile::{
    Compiled, StepKind, after, after_failure, after_pending, first, rescue_target,
};
use crate::inventory::Host;
use crate::playbook::PlayTask;
use crate::template::{Templar, TemplateError};
use crate::transport::{ConnectError, Escalation, Transport};
use crate::vars::VarStore;

use super::coordinator::{Event, Progress, escalated_links};
use super::include::{report_include, resolve_include};
use super::prepare::{Item, PlayPlan, Prepared, prepare, retry_name};
use super::report::report_task;
use super::run::{
    Retry, conditional_error, fact_targets, failed_task_value, finish, notify, protocol_task,
    registered_value, retry_plan, reuse_or_connect, run_agent_batch, run_local, until_holds,
};
use super::{LinkKey, RunOptions};

/// Names whose value depends on what the other hosts have done: another host's variables, and
/// the play's own live host list. A task whose raw text mentions one of them must not run ahead
/// of the others, so `linear` puts a boundary in front of it.
const CROSS_HOST_NAMES: [&str; 3] = ["hostvars", "play_hosts", "play_batch"];

/// Whether the hosts of a play meet in front of this step. Without `batching`, every step is
/// such a point: that is what `linear` means, and a task's text cannot show a dependency that
/// runs through a file, a database or a service. With `batching`, the keyword table and the
/// textual scan decide, and a host carries on through the steps in between.
///
/// The step loop asks it twice on the way through an iteration, once to give back a fork permit
/// kept from the batch before and once to stop and wait for the other hosts. What a batch end
/// asks instead is whether this driver carries straight on, which is a different question.
fn is_boundary(task: &PlayTask, batching: bool) -> bool {
    if !batching {
        return true;
    }
    task.barrier() || reads_across_hosts(task)
}

/// Whether a task reads across hosts, decided once per play from its unrendered text.
fn reads_across_hosts(task: &PlayTask) -> bool {
    // `ansible_play_hosts_all` is a static copy of the play's starting host list: no host can
    // ever change it, so matching it buys no ordering guarantee and only costs a barrier. Strip
    // it before matching so it does not trip the `play_hosts` needle on its own; a task that
    // separately mentions `ansible_play_hosts` or `play_batch` still is a boundary.
    let mentions = |s: &str| {
        let s = s.replace("play_hosts_all", "");
        CROSS_HOST_NAMES.iter().any(|n| s.contains(n))
    };
    if mentions(&task.name) {
        return true;
    }
    if task
        .when
        .iter()
        .chain(&task.changed_when)
        .chain(&task.failed_when)
        .any(|s| mentions(s))
    {
        return true;
    }
    let mut text = serde_json::to_string(&task.args).unwrap_or_default();
    text.push_str(&serde_json::to_string(&task.vars).unwrap_or_default());
    if let Some(items) = &task.loop_items {
        text.push_str(&items.to_string());
    }
    mentions(&text)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the driver's whole context; drive_host has a single call site, spawned once per host by the loop above"
)]
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
    };
    // One entry per target user on this host: the connecting user's own agent, and one more
    // for each `become_user` the play escalates to. Connections this play never uses itself
    // stay in here untouched and go straight back with `Finished`.
    let mut links: HashMap<LinkKey, AgentLink> = existing.into_iter().collect();
    // Which of them this play has already proved alive: one liveness check per connection per
    // play, and never again.
    let mut checked: HashSet<LinkKey> = HashSet::new();
    // Ansible's `forks`: one permit per host, held from the connection to the results of a
    // batch. It lives in this binding and releases itself when dropped, so no way out of this
    // function can leak it, whether that is a return, an error, a cancellation or a panic.
    let mut permit: Option<OwnedSemaphorePermit> = None;
    // Set when the host leaves the run without finishing: the message the recap shows, and
    // whether the task it happened under censors its output.
    let mut unreachable: Option<String> = None;
    let mut unreachable_censored = false;
    // The delegate of the task whose batch could not be reached, for the `[h2 -> h1]` the
    // `UNREACHABLE!` line carries. Measured on ansible-core 2.19.12: the delegating host is the
    // one the recap counts, and the delegate only shows in the line.
    let mut unreachable_delegate: Option<String> = None;
    let mut failed = false;
    // Set when this host leaves the play with nothing to show for it: the one runner of a
    // `run_once` step failed, so this host never ran the task and has no line, and the run
    // still has to exit 2 for it. Measured on ansible-core 2.19.12.
    let mut silent_failure = false;
    // Not always step 0: a block with an empty `block:` list lays its rescue out there, and
    // nothing has failed. The steps in front of the first one are stepped over like any other.
    let mut pos = first(&plan.steps());
    // An interrupt here leaves `pos` where it is; the run loop's own `stop` test below is what
    // ends the play for this host, and the `Finished` at the end of this function still goes.
    if let Some(grown) = driver.stepped_over(0..pos).await {
        pos += grown;
    }
    let mut batch_id: u64 = 0;
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

    'run: loop {
        let c = plan.steps();
        let n = c.steps.len();
        // The fork permit and the escalated links go back before either failure arm below waits
        // at a splice point: the step loop's own release reads the success successor and never
        // the range a host that has just failed steps over. Only on the failure paths, so a batch
        // that ended cleanly still carries its permit into the next one.
        if failed_at.is_some() || failed_index.is_some() {
            permit = None;
            for key in escalated_links(&links) {
                if let Some(link) = links.remove(&key) {
                    tokio::spawn(link.shutdown());
                }
            }
        }
        if let Some((index, result)) = failed_at.take() {
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
                {
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
        // Collect a batch of remote tasks up to the next boundary; report skips and run local
        // tasks as they come, in order.
        let mut batch: Vec<(usize, Vec<Item>)> = Vec::new();
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
        let mut batch_retry: Option<Retry> = None;
        let mut deferred_error: Option<(usize, TemplateError)> = None;
        // The last step of the batch, when where the host goes after it is not yet decided.
        // See the `Prepared::Remote` arm: a step a rescue would catch cannot say what it steps
        // over until its own result is in.
        let mut undecided: Option<usize> = None;
        while pos < n {
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
            if batch.is_empty()
                && permit.is_some()
                && (is_boundary(task, options.batching)
                    || crate::compile::is_splice_point(&step.kind)
                    || driver.steps_over_a_splice_point(&c, pos))
            {
                permit = None;
                for key in escalated_links(&links) {
                    if let Some(link) = links.remove(&key) {
                        tokio::spawn(link.shutdown());
                    }
                }
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
            // A step the hosts of the batch meet in front of: `run_once`, or a text naming another
            // host's state. In front of the flush, include and mask arms rather than behind them,
            // because a host that walked into one of those without stopping here would report a
            // step the others have not reached, and every wait in this file opens on the
            // coordinator's frontier.
            if is_boundary(task, options.batching) && pos > 0 {
                if !batch.is_empty() {
                    break;
                }
                if driver.wait_for_barrier(pos).await.is_none() {
                    break 'run;
                }
            }
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
            let follower =
                task.runs_once() && !handlers_only && runner.as_deref().is_some_and(|h| h != name);
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
                if explicit && !handlers_only && !masked_out {
                    driver.banner(pos).await;
                }
                driver.task_done(pos).await;
                // `n` is the length of the list this host is walking, read before the word it
                // just sent could let the coordinator grow it. See `wait_for_splice`.
                if driver.wait_for_splice(pos, n).await.is_none() {
                    break 'run;
                }
                // A host the flush's own mask leaves out reports the step and waits for the
                // splice like everyone else, but it has not performed a flush: the handler steps
                // behind it carry that same mask, so it runs none of them, and the line below
                // would otherwise throw away the notifications it is still carrying to the next
                // flush it does reach.
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
                    let (groups, shown) = resolve_include(
                        &c,
                        step,
                        kind,
                        &name,
                        &plan,
                        &live,
                        &templar,
                        &store,
                        &options.defaults,
                    );
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
            match prepare(
                step,
                &name,
                &plan,
                &live,
                &templar,
                &store,
                &options.defaults,
            ) {
                Err(err) => {
                    deferred_error = Some((pos, err));
                    break;
                }
                Ok(Prepared::Skipped(items)) => {
                    if !batch.is_empty() {
                        break;
                    }
                    let labels: Vec<Option<String>> =
                        items.iter().map(|i| i.label.clone()).collect();
                    let results: Vec<(Option<Value>, TaskResult)> = items
                        .into_iter()
                        .map(|i| (i.element, i.skipped.unwrap_or_default()))
                        .collect();
                    // A skipped task fails nothing, so no rescue is in question for it, and it
                    // made no attempt to retry.
                    // No delegate on a skipped task: measured on ansible-core 2.19.12, a
                    // `delegate_to` a `when` left out prints `skipping: [h1]` with no arrow.
                    report_task(
                        &tx,
                        &name,
                        pos,
                        task,
                        &results,
                        &labels,
                        &[],
                        &[],
                        false,
                        false,
                        None,
                    )
                    .await;
                    if let Some(reg) = &task.register {
                        let mut vars = store.lock().expect("vars lock");
                        let value = registered_value(task, &results);
                        for target in &register_hosts {
                            vars.set_untrusted_fact(target, reg, value.clone());
                        }
                    }
                    let Some(next) = driver.advance(pos).await else {
                        break 'run;
                    };
                    pos = next;
                }
                // A `meta` asks the engine for something rather than the host: it shows a
                // banner, reports nothing and moves on. Every action that reaches here is one
                // this release honours by doing nothing; the pre-flight refused the rest.
                Ok(_) if step.kind == StepKind::Meta => {
                    if !batch.is_empty() {
                        break;
                    }
                    driver.banner(pos).await;
                    driver.task_done(pos).await;
                    let Some(next) = driver.advance(pos).await else {
                        break 'run;
                    };
                    pos = next;
                }
                Ok(Prepared::Local(items, delegate)) => {
                    if !batch.is_empty() {
                        break;
                    }
                    // A controller-side module runs here whatever `delegate_to` says - measured
                    // on ansible-core 2.19.12, a delegated `set_fact` and a delegated `debug`
                    // both run on the controller - so the delegate decides two things and no
                    // more: the name the line shows, and, under `delegate_facts`, whose facts
                    // the module writes.
                    let fact_hosts: Vec<String> = match (task.delegates_facts(), &delegate) {
                        (true, Some(to)) => vec![to.clone()],
                        _ => register_hosts.clone(),
                    };
                    let retry = match retry_plan(task, items.first(), &templar) {
                        Ok(retry) => retry,
                        Err(err) => {
                            deferred_error = Some((pos, err));
                            break;
                        }
                    };
                    let mut results = Vec::new();
                    let mut labels = Vec::new();
                    let mut lefts: Vec<Vec<u32>> = Vec::new();
                    let mut names: Vec<String> = Vec::new();
                    for item in &items {
                        names.push(retry_name(task, &item.vars, &templar));
                        let mut mine = Vec::new();
                        let r = if let Some(s) = &item.skipped {
                            s.clone()
                        } else {
                            let mut attempt = 0;
                            loop {
                                attempt += 1;
                                let mut r = finish(
                                    task,
                                    item,
                                    run_local(
                                        task,
                                        item,
                                        step,
                                        &fact_hosts,
                                        &templar,
                                        &store,
                                        verbosity,
                                    ),
                                    &templar,
                                );
                                let Some(retry) = &retry else { break r };
                                r.0.insert("attempts".into(), json!(attempt));
                                match until_holds(task, item, &r, retry, &templar) {
                                    // A condition that cannot be evaluated ends the task
                                    // there, with no further attempt and no `attempts` -
                                    // measured, and the reference's own prefix for a task
                                    // that dies rather than fails.
                                    Err(e) => {
                                        break TaskResult::failed_with(conditional_error(&e));
                                    }
                                    Ok(true) => break r,
                                    Ok(false) => {}
                                }
                                mine.push(retry.attempts - attempt + 1);
                                let last = attempt >= retry.attempts;
                                if last {
                                    // The loop ran out with the condition still false, which
                                    // is a failed task even when the module itself passed:
                                    // measured with a `changed_when: false` under
                                    // `until: r.changed`.
                                    r.0.insert("failed".into(), json!(true));
                                }
                                if driver.sleep_between(retry.delay).await.is_none() {
                                    break 'run;
                                }
                                if last {
                                    break r;
                                }
                            }
                        };
                        results.push((item.element.clone(), r));
                        labels.push(item.label.clone());
                        lefts.push(mine);
                    }
                    if let Some(reg) = &task.register {
                        let mut vars = store.lock().expect("vars lock");
                        let value = registered_value(task, &results);
                        for target in &register_hosts {
                            vars.set_untrusted_fact(target, reg, value.clone());
                        }
                    }
                    let rescuable = !handlers_only && rescue_target(&c, pos).is_some();
                    // A censored `debug` shows nothing at all at verbosity 0 and its censored
                    // body from `-v` on, measured: the dump is what puts the body on the line,
                    // so it is the dump that goes.
                    let dump =
                        short_name(&task.module) == "debug" && !(task.censors() && verbosity == 0);
                    if let Some(result) = report_task(
                        &tx,
                        &name,
                        pos,
                        task,
                        &results,
                        &labels,
                        &lefts,
                        &names,
                        dump,
                        rescuable,
                        delegate.as_deref(),
                    )
                    .await
                    {
                        failed |= !rescuable;
                        failed_at = Some((pos, result));
                        break;
                    }
                    notify(&c, task, &results, &mut notified);
                    let Some(next) = driver.advance(pos).await else {
                        break 'run;
                    };
                    pos = next;
                }
                Ok(Prepared::Remote(items, escalation, delegate)) => {
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
                    let target_vars = delegate
                        .as_ref()
                        .map_or(&items[0].vars.map, |(_, vars)| vars);
                    let transport =
                        match Transport::for_vars(&target, target_vars, &options.defaults) {
                            Ok(transport) => transport,
                            // The tasks in hand ran under a connection that was resolved; this
                            // one is the next batch's first, where the same failure ends the
                            // host with its own message.
                            Err(_) if !batch.is_empty() => break,
                            // The batch this task would have opened never formed, so the two
                            // fields the `UNREACHABLE!` line reads are set here rather than
                            // below: the arrow names the delegate this task asked for, and
                            // `no_log` censors the reason.
                            Err(err) => {
                                unreachable_censored = task.censors();
                                unreachable_delegate = delegate_name;
                                unreachable = Some(format!("{err:#}"));
                                break 'run;
                            }
                        };
                    if !batch.is_empty()
                        && (escalation != batch_escalation
                            || delegate_name != batch_delegate
                            || Some(&transport) != batch_transport.as_ref())
                    {
                        break;
                    }
                    let retry = match retry_plan(task, items.first(), &templar) {
                        Ok(retry) => retry,
                        Err(err) => {
                            deferred_error = Some((pos, err));
                            break;
                        }
                    };
                    // A retried task runs its items one at a time, each on its own trip to the
                    // agent, so it is alone in its batch: the tasks in hand go out first and
                    // this one opens the next batch. It ends that batch too, through `boundary`.
                    if retry.is_some() && !batch.is_empty() {
                        break;
                    }
                    batch_escalation = escalation;
                    batch_delegate = delegate_name;
                    batch_transport = Some(transport);
                    batch_retry = retry;
                    // A looping task ends the batch because its items travel with
                    // `ignore_errors` set, so the agent runs all of them the way Ansible does.
                    // Only `report_task` may decide the task failed, from the aggregate, and
                    // nothing behind it in the same batch is allowed to run before it has.
                    let boundary = task.register.is_some()
                        || task.loop_items.is_some()
                        || !task.changed_when.is_empty()
                        || !task.failed_when.is_empty()
                        || batch_retry.is_some();
                    batch.push((pos, items));
                    // Where this host goes after a step a `rescue` would catch depends on how that
                    // step ends, so the batch stops here: moving `pos` now would step over that
                    // very rescue and tell the coordinator so. A flush point between here and
                    // where the step leads ends the batch for its own reason - `advance` would
                    // wait for a splice the coordinator cannot publish without this step's word.
                    if rescue_target(&c, pos).is_some() || driver.steps_over_a_splice_point(&c, pos)
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
            unreachable_delegate = batch_delegate.clone();
            if permit.is_none() {
                if let Ok(p) = Arc::clone(&forks).acquire_owned().await {
                    permit = Some(p);
                } else {
                    // Nothing in this run closes the semaphore, so this is a bug rather than
                    // a shutdown. Reporting it beats returning as if the host had run.
                    unreachable = Some("the run's fork limit is gone".to_string());
                    break 'run;
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
                break 'run;
            };
            let key = LinkKey {
                host: target,
                become_user: batch_escalation.as_ref().map(|e| e.user.clone()),
                transport,
            };
            let link = match reuse_or_connect(
                &mut links,
                &mut checked,
                &key,
                batch_escalation.as_ref(),
                &agents,
                &options,
            )
            .await
            {
                Ok(l) => l,
                // The host answered and then refused to escalate, so this is the task failing and
                // not the host going away. `ignore_errors` is deliberately not honoured: the
                // batch never ran, and a run reporting success while having quietly skipped every
                // escalated task is the worst outcome here. Measured on ansible-core 2.19.12: a
                // refused `become_user` is a task failure, so a rescue takes it (`rescued=1`,
                // exit 0).
                Err(ConnectError::Become(msg)) => {
                    let index = batch[0].0;
                    let mut task = c.steps[index].task.clone();
                    task.ignore_errors = Some(false);
                    let results = vec![(None, TaskResult::failed_with(msg))];
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
                        false,
                        rescuable,
                        batch_delegate.as_deref(),
                    )
                    .await;
                    failed |= !rescuable;
                    failed_at = Some((index, result.unwrap_or_default()));
                    // Nothing ran, so the fork this host holds goes back before it carries on:
                    // the next round can block at a barrier or at a flush point, and a permit
                    // held across a wait is one the hosts that have to reach that same point
                    // cannot have. With `-f` under the number of live hosts, none of them ever
                    // would.
                    permit = None;
                    continue 'run;
                }
                Err(err) => {
                    unreachable = Some(err.to_string());
                    break 'run;
                }
            };
            let mut received: Vec<Vec<Option<TaskResult>>> = batch
                .iter()
                .map(|(_, items)| vec![None; items.len()])
                .collect();
            // The retry counts of each item of the retried task, empty for every other batch.
            let mut lefts: Vec<Vec<u32>> = batch
                .first()
                .map(|(_, items)| vec![Vec::new(); items.len()])
                .unwrap_or_default();
            // Each item's templated task name, parallel to `lefts` and empty the same way: the
            // retry loop below is the only place with the item's own vars in hand.
            let mut names: Vec<String> = Vec::new();
            // Whether `received` already holds results the conditions have been applied to. The
            // retry loop has to apply them itself, since `until` reads what they decided.
            let mut decided = false;
            let ended = if let Some(retry) = batch_retry.clone() {
                decided = true;
                let (index, items) = &batch[0];
                let task = &c.steps[*index].task;
                names = items
                    .iter()
                    .map(|item| retry_name(task, &item.vars, &templar))
                    .collect();
                let mut outcome = Ok(BatchOutcome::Completed);
                // Item by item, in order, each one's attempts finished before the next one
                // starts: measured on ansible-core 2.19.12 with a two-item loop whose first item
                // passed and whose second needed two attempts - the retry lines of the second
                // sit between the two result lines, so the items do not retry together.
                'items: for (ii, item) in items.iter().enumerate() {
                    if item.skipped.is_some() {
                        continue;
                    }
                    let mut attempt = 0;
                    loop {
                        attempt += 1;
                        batch_id += 1;
                        let (mut flat, ended_one) = run_agent_batch(
                            link,
                            &name,
                            batch_id,
                            vec![protocol_task(task, item)],
                            &mut driver.stop,
                            &mut driver.stop_broken,
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
                        let mut r = finish(task, item, raw, &templar);
                        r.0.insert("attempts".into(), json!(attempt));
                        match until_holds(task, item, &r, &retry, &templar) {
                            Err(e) => {
                                received[0][ii] =
                                    Some(TaskResult::failed_with(conditional_error(&e)));
                                continue 'items;
                            }
                            Ok(true) => {
                                received[0][ii] = Some(r);
                                continue 'items;
                            }
                            Ok(false) => {}
                        }
                        lefts[ii].push(retry.attempts - attempt + 1);
                        let last = attempt >= retry.attempts;
                        if last {
                            r.0.insert("failed".into(), json!(true));
                            received[0][ii] = Some(r);
                        }
                        if driver.sleep_between(retry.delay).await.is_none() {
                            break 'run;
                        }
                        if last {
                            continue 'items;
                        }
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
                    for (ii, item) in items.iter().enumerate() {
                        if item.skipped.is_some() {
                            continue;
                        }
                        tasks.push(protocol_task(task, item));
                        origin.push((bi, ii));
                    }
                }
                let (flat, ended) = run_agent_batch(
                    link,
                    &name,
                    batch_id,
                    tasks,
                    &mut driver.stop,
                    &mut driver.stop_broken,
                )
                .await;
                for (k, result) in flat.into_iter().enumerate() {
                    if let Some(&(bi, ii)) = origin.get(k) {
                        received[bi][ii] = result;
                    }
                }
                ended
            };
            // Report every task of the batch in order; tasks the agent never reached after a
            // failure are not reported at all, as in Ansible. Whether `undecided` was actually
            // reported is tracked rather than assumed, because the agent can end the batch `Ok`
            // without a result for it and advancing past a step with no `TaskDone` behind it
            // stalls every barrier behind it.
            let mut undecided_reported = false;
            for (bi, (index, items)) in batch.iter().enumerate() {
                let task = &c.steps[*index].task;
                let mut results = Vec::new();
                let mut labels = Vec::new();
                let mut reached = true;
                for (ii, item) in items.iter().enumerate() {
                    let r = match (&item.skipped, received[bi][ii].take()) {
                        (Some(s), _) => s.clone(),
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
                if let Some(reg) = &task.register {
                    let live = driver.progress.borrow().live_hosts.clone();
                    let mut vars = store.lock().expect("vars lock");
                    let value = registered_value(task, &results);
                    for target in fact_targets(task, &name, &live) {
                        vars.set_untrusted_fact(&target, reg, value.clone());
                    }
                }
                let rescuable = !handlers_only && rescue_target(&c, *index).is_some();
                let retried: &[Vec<u32>] = if bi == 0 { &lefts } else { &[] };
                let retried_names: &[String] = if bi == 0 { &names } else { &[] };
                if let Some(result) = report_task(
                    &tx,
                    &name,
                    *index,
                    task,
                    &results,
                    &labels,
                    retried,
                    retried_names,
                    false,
                    rescuable,
                    batch_delegate.as_deref(),
                )
                .await
                {
                    failed |= !rescuable;
                    failed_at = Some((*index, result));
                    break;
                }
                notify(&c, task, &results, &mut notified);
                if undecided == Some(*index) {
                    undecided_reported = true;
                }
            }
            // The results are in and reported, so the next host may start. The escalated links go
            // back with the permit, which is what makes `forks` bound the connections open as
            // well as the hosts working; the link to the host itself stays, being the one
            // persistence is for. Closed off-task rather than awaited: `shutdown` gives its agent
            // two seconds, and paying that here would serialise what the permit just freed.
            let carries_on =
                failed_at.is_none() && deferred_error.is_none() && undecided.is_none() && pos < n;
            if !carries_on {
                permit = None;
                for key in escalated_links(&links) {
                    if let Some(link) = links.remove(&key) {
                        tokio::spawn(link.shutdown());
                    }
                }
            }
            match ended {
                Err(msg) => {
                    unreachable = Some(msg);
                    break 'run;
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
            let results = vec![(
                None,
                TaskResult::failed_with(format!("Task failed: {}", err.0)),
            )];
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
                false,
                rescuable,
                None,
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
    if let Some(msg) = unreachable {
        // Whatever went wrong, none of these connections is one to hand to the next play.
        for (_, link) in links.drain() {
            link.shutdown().await;
        }
        failed = true;
        let _ = tx
            .send(Event::Unreachable {
                host: name.clone(),
                msg,
                censored: unreachable_censored,
                delegate: unreachable_delegate,
            })
            .await;
    }
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

/// What became of a `run_once` step, from the point of view of a host that did not run it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunOnce {
    /// The one runner finished the step without failing; whatever it registered or set as a
    /// fact is in the store for every host of the batch.
    Done,
    /// It failed. Measured on ansible-core 2.19.12: every other host leaves the play here, with
    /// no line of its own and no recap entry, and the run exits 2 - with a `rescue` around the
    /// task as well, which keeps the runner itself in the play.
    Failed,
    /// It left the play without a verdict, which is what an unreachable host does. Measured:
    /// exit 4 and no recap line for the hosts that were waiting, so their departure adds
    /// nothing of its own.
    Gone,
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
}

impl Driver<'_> {
    /// Whether the run has been interrupted.
    fn stopped(&self) -> bool {
        *self.stop.borrow()
    }

    /// Tells the coordinator this host is done with the step at `index`.
    async fn task_done(&self, index: usize) {
        let _ = self
            .tx
            .send(Event::TaskDone {
                host: self.name.to_string(),
                index,
            })
            .await;
    }

    /// Asks for the banner of the step at `index`.
    async fn banner(&self, index: usize) {
        let _ = self
            .tx
            .send(Event::Banner {
                host: self.name.to_string(),
                index,
            })
            .await;
    }

    /// Waits `delay`, or gives up when the run is interrupted.
    async fn sleep_between(&mut self, delay: Duration) -> Option<()> {
        if delay.is_zero() {
            return Some(());
        }
        let deadline = tokio::time::Instant::now() + delay;
        loop {
            if self.stopped() {
                return None;
            }
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => return Some(()),
                res = self.stop.changed(), if !self.stop_broken => {
                    if res.is_err() {
                        self.stop_broken = true;
                    } else {
                        return None;
                    }
                }
            }
        }
    }

    /// Waits until every other live host of the batch has finished the step in front of `pos`.
    ///
    /// This is what a step the batch meets in front of opens on, and its exit condition is its
    /// own: alone in the batch there is nobody left to wait for, which is also how the wait ends
    /// when every other host has died.
    ///
    /// `None` when the run was interrupted.
    async fn wait_for_barrier(&mut self, pos: usize) -> Option<()> {
        loop {
            if self.stopped() {
                return None;
            }
            let p = self.progress.borrow().clone();
            if p.live_hosts.len() <= 1 || p.completed_through.is_some_and(|c| c + 1 >= pos) {
                return Some(());
            }
            tokio::select! {
                changed = self.progress.changed() => {
                    // The coordinator is gone, so no further progress can be published
                    // and waiting on it would never end.
                    if changed.is_err() {
                        return Some(());
                    }
                }
                res = self.stop.changed(), if !self.stop_broken => {
                    if res.is_err() {
                        self.stop_broken = true;
                    } else {
                        return None;
                    }
                }
            }
        }
    }

    /// Waits for the one runner of the `run_once` step at `pos` to say what it made of it.
    ///
    /// `None` when the run was interrupted, the way every other wait in this file answers it.
    ///
    /// The verdict is read before the live host list, and that order is the whole of it: the
    /// coordinator publishes a failed result and the shrunken list in the same `Progress`, so a
    /// host that asked "is the runner still live?" first would see a runner that is merely gone
    /// and leave at exit 4 where the reference exits 2.
    async fn wait_for_run_once(&mut self, runner: &str, pos: usize) -> Option<RunOnce> {
        loop {
            if self.stopped() {
                return None;
            }
            let p = self.progress.borrow().clone();
            match p.run_once.get(&pos) {
                Some(true) => return Some(RunOnce::Failed),
                Some(false) => return Some(RunOnce::Done),
                None => {}
            }
            // The host this step was handed to has left the batch without a verdict, which is
            // what an unreachable host does. Asked of that one host and not of the list's
            // length: with three hosts waiting on a runner that died, a test for "anybody else
            // still live" would be true for ever and this wait would never end.
            if !p.live_hosts.iter().any(|h| h == runner) {
                return Some(RunOnce::Gone);
            }
            tokio::select! {
                changed = self.progress.changed() => {
                    // The coordinator is gone, so no verdict can ever be published.
                    if changed.is_err() {
                        return Some(RunOnce::Gone);
                    }
                }
                res = self.stop.changed(), if !self.stop_broken => {
                    if res.is_err() {
                        self.stop_broken = true;
                    } else {
                        return None;
                    }
                }
            }
        }
    }

    /// Whether `advance` from `pos` would walk past a splice point - a flush or an include - and
    /// so block waiting for the splice.
    ///
    /// A driver may only wait for a splice once it owes the coordinator nothing in front of it:
    /// the coordinator walks the step list in order and holds at each index until every host is
    /// done with it or gone, so a host waiting at a splice point while a step behind it is still
    /// unreported waits for an index the coordinator can never reach. The batch collection loop
    /// asks this before moving on from a step it has queued and not yet run.
    fn steps_over_a_splice_point(&self, compiled: &Compiled, pos: usize) -> bool {
        let next = match self.cleanup {
            Some(end) => match after_pending(compiled, pos, end) {
                Some((next, _)) => next,
                None => return false,
            },
            None => after(compiled, pos),
        };
        (pos + 1..next.min(compiled.steps.len()))
            .any(|i| crate::compile::is_splice_point(&compiled.steps[i].kind))
    }

    /// The step this host moves to after finishing `pos`, past the sections it has no reason to
    /// enter. A host draining an `always` section after a failure finishes that section instead,
    /// and `cleanup` - the index that section ends at - moves with it to the next one.
    ///
    /// Returns the length of the step list when the play is over for this host, which is what the
    /// driver's own bound reads as "done", and `None` when the run was interrupted on the way.
    async fn advance(&mut self, pos: usize) -> Option<usize> {
        let compiled = self.plan.steps();
        let next = match self.cleanup {
            Some(end) => after_pending(&compiled, pos, end).map(|(next, end)| {
                self.cleanup = Some(end);
                next
            }),
            None => Some(after(&compiled, pos)),
        };
        let Some(next) = next else {
            return Some(compiled.steps.len());
        };
        // Every flush point between here and there grows the list behind it, and every index past
        // it - this destination, and the end of the section this host may be draining - moves by
        // however much it grew.
        let grown = self.stepped_over(pos + 1..next).await?;
        if let Some(end) = self.cleanup.as_mut() {
            *end += grown;
        }
        Some(next + grown)
    }

    /// Tells the coordinator about every step of `range` this host stepped over, stopping at each
    /// splice point on the way, and answers how much longer the list is for it.
    ///
    /// A host has to stop at a splice point it steps over as surely as at one it runs. The
    /// coordinator inserts steps behind a flush or an include once every host has reported that
    /// index, and a host that read the list again before that would be holding an index into a
    /// list that has changed underneath it - the exact shape this file's barrier invariant exists
    /// to prevent. Measured on ansible-core 2.19.12: a `meta: flush_handlers` written inside a
    /// `rescue:` nobody entered runs nothing and shows nothing, and so does an `include_tasks`
    /// written there, which is what puts a live host in this position.
    async fn stepped_over(&mut self, range: std::ops::Range<usize>) -> Option<usize> {
        let mut grown = 0;
        let mut from = range.start;
        loop {
            let compiled = self.plan.steps();
            let end = range.end + grown;
            let splice = (from..end.min(compiled.steps.len()))
                .find(|&i| crate::compile::is_splice_point(&compiled.steps[i].kind));
            let Some(at) = splice else {
                self.skipped(from..end).await;
                return Some(grown);
            };
            self.skipped(from..at + 1).await;
            let before = compiled.steps.len();
            drop(compiled);
            // The bare wait, and not the one that moves `cleanup`: the growth of this splice is
            // part of what this function answers, and the caller moves `cleanup` by the whole of
            // that answer. Moving it here as well would move it twice and end the host's drain
            // past the section it is draining.
            self.wait_past_splice(at).await?;
            grown += self.plan.steps().steps.len() - before;
            // Back at the splice point's successor, which is now the first of the steps it just
            // grew by. This host steps over those too - they sit in the section the flush or the
            // include sat in, and it is not in that section - but it still owes the coordinator a
            // word about each of them, or the barrier behind them opens on a host that never said
            // it had passed them.
            from = at + 1;
        }
    }

    /// Waits until the coordinator has put the handler steps in behind the flush point at `at`,
    /// and moves the end of the `always` section this host may be draining by however much the
    /// splice grew the list - the one index it carries that sits past `at`.
    ///
    /// `before` is how long the list was when the host still owed `at` a `TaskDone`, and the
    /// caller has to read it before sending that word. Reading it here would be a race: the
    /// coordinator publishes the splice as soon as the last host reports the index, so the list
    /// can already have grown by the time this runs, and the growth would then measure as nothing
    /// at all - leaving `cleanup` where it was and ending the host's drain one step early. Rare,
    /// load dependent, and it reports success while a cleanup step nobody ran goes missing.
    ///
    /// `None` only when the run was interrupted. A coordinator that is gone answers `Some(())`:
    /// no splice will ever land, so `cleanup` stays where it is and the host carries on with the
    /// list it has, which is what `wait_past_splice` answers `false` for.
    async fn wait_for_splice(&mut self, at: usize, before: usize) -> Option<()> {
        if self.wait_past_splice(at).await?
            && let Some(end) = self.cleanup.as_mut()
        {
            *end += self.plan.steps().steps.len() - before;
        }
        Some(())
    }

    /// The wait alone, for the caller whose `cleanup` must not move here.
    ///
    /// This is the whole of the plan's one dynamic primitive on the driver's side: between
    /// reporting a splice point and this returning, the host reads nothing from the step list, so
    /// the index it holds cannot be invalidated by the splice.
    ///
    /// `true` when the splice landed, `false` when the coordinator is gone and none ever will
    /// land - which is why this is answered rather than assumed: an index that moves with the
    /// list must not move for a splice that was never published. `None` when the run was
    /// interrupted.
    async fn wait_past_splice(&mut self, at: usize) -> Option<bool> {
        loop {
            if self.stopped() {
                return None;
            }
            if self
                .progress
                .borrow()
                .spliced_through
                .is_some_and(|s| s >= at)
            {
                return Some(true);
            }
            tokio::select! {
                changed = self.progress.changed() => {
                    // The coordinator is gone, so no splice can be published and waiting on one
                    // would never end. Whatever the host has left to do, it does on the list it
                    // has.
                    if changed.is_err() {
                        return Some(false);
                    }
                }
                res = self.stop.changed(), if !self.stop_broken => {
                    if res.is_err() {
                        self.stop_broken = true;
                    } else {
                        return None;
                    }
                }
            }
        }
    }

    /// Tells the coordinator about every step in `range` that this host stepped over, so the
    /// shared progress it publishes counts this host as having reached them.
    ///
    /// It is what keeps a host that steps over a section from waiting on itself: the progress
    /// every barrier reads is the lowest step any live host has finished, so a host that jumped
    /// from 3 to 9 without saying so would hold that figure at 3 while waiting for it to reach 8.
    /// Both halves of that are live now: a host that failed steps over the rest of the body on
    /// its way to a cleanup, and a host that failed nothing steps over every `rescue` it walks
    /// past - the second with the whole play still waiting on it.
    async fn skipped(&self, range: std::ops::Range<usize>) {
        for index in range {
            self.task_done(index).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::task;
    use super::*;
    use serde_json::json;

    /// Everything a playbook can hide a cross-host read in has to be searched, or a barrier
    /// silently does nothing and the ordering it promised was never there.
    #[test]
    fn a_cross_host_read_is_found_wherever_the_task_spells_it() {
        let mut t = task("command");
        assert!(!reads_across_hosts(&t));
        t.args
            .insert("cmd".into(), json!("echo {{ hostvars['a'].x }}"));
        assert!(reads_across_hosts(&t), "arguments");
        let mut t = task("debug");
        t.when = vec!["inventory_hostname in ansible_play_hosts".into()];
        assert!(reads_across_hosts(&t), "when");
        let mut t = task("debug");
        t.name = "count {{ ansible_play_batch | length }}".into();
        assert!(reads_across_hosts(&t), "name");
        let mut t = task("debug");
        t.vars.insert("peer".into(), json!("{{ hostvars['a'].y }}"));
        assert!(reads_across_hosts(&t), "vars");
        let mut t = task("command");
        t.loop_items = Some(json!("{{ ansible_play_hosts }}"));
        assert!(reads_across_hosts(&t), "loop");
        let mut t = task("command");
        t.changed_when = vec!["hostvars['a'].rc == 0".into()];
        assert!(reads_across_hosts(&t), "changed_when");
        let mut t = task("command");
        t.failed_when = vec!["play_hosts | length > 1".into()];
        assert!(reads_across_hosts(&t), "failed_when");
    }

    /// `ansible_play_hosts_all` never changes once the play starts, so a task mentioning it and
    /// nothing else needs no barrier. The live list and the batch keyword still hold one.
    #[test]
    fn ansible_play_hosts_all_alone_is_not_a_boundary() {
        let mut t = task("debug");
        t.args.insert(
            "msg".into(),
            json!("{{ ansible_play_hosts_all | join(',') }}"),
        );
        assert!(!reads_across_hosts(&t), "the static list on its own");
        let mut t = task("debug");
        t.args.insert(
            "msg".into(),
            json!(
                "{{ ansible_play_hosts | join(',') }} of {{ ansible_play_hosts_all | join(',') }}"
            ),
        );
        assert!(reads_across_hosts(&t), "the live list is still present");
        let mut t = task("debug");
        t.args
            .insert("msg".into(), json!("{{ ansible_play_batch }}"));
        assert!(reads_across_hosts(&t), "play_batch is unaffected");
    }

    /// The step loop's boundary test is the union of two independent rules, and neither one
    /// covers the other: `run_once` is a boundary because the table says so and spells none of
    /// the cross-host names, a `hostvars` read is a boundary because of its text and carries no
    /// keyword, and `delegate_to` is neither - the delegating driver opens its own link to the
    /// delegate, so nothing is shared and nobody has to wait.
    ///
    /// This says nothing about who runs a `run_once` step. The election is the coordinator's,
    /// and `run_once_is_decided_once_and_survives_the_runner_leaving` is what guards it; the
    /// barrier here only keeps the hosts walking the list together.
    ///
    /// What would make this red: the `barrier` flag dropped from the table, which lets a
    /// `run_once` step be reported by a host while another is still behind it, so the verdict
    /// the waiting hosts read arrives before the runner has run; the textual
    /// scan dropped, which lets a `hostvars` read run ahead of the host it reads; or
    /// `delegate_to` made a boundary, which would serialise every delegated task on a wait that
    /// buys nothing.
    #[test]
    fn a_boundary_is_either_declared_by_a_keyword_or_found_in_the_text() {
        let mut t = task("command");
        assert!(!t.barrier() && !reads_across_hosts(&t));

        t.run_once = Some(true);
        assert!(t.barrier(), "the table declares run_once a barrier");
        assert!(
            !reads_across_hosts(&t),
            "and it spells none of the cross-host names, so the scan alone would miss it"
        );
        t.run_once = Some(false);
        assert!(!t.barrier(), "a task that says false is not one");
        t.run_once = None;

        t.delegate_to = Some("h3".into());
        t.delegate_facts = Some(true);
        assert!(
            !t.barrier() && !reads_across_hosts(&t),
            "a delegated task shares nothing: its driver opens its own link to the delegate"
        );

        let mut t = task("command");
        t.args
            .insert("cmd".into(), json!("echo {{ hostvars['a'].x }}"));
        assert!(
            reads_across_hosts(&t),
            "the scan catches what no keyword spells"
        );
        assert!(!t.barrier(), "and it needs no keyword to do it");
    }
}
