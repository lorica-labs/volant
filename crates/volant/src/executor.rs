// SPDX-License-Identifier: GPL-3.0-or-later
//! Runs one play on its hosts with the `linear` strategy. Each host renders its own tasks,
//! groups consecutive remote tasks into batches, and runs `set_fact` and `debug` locally.
//! Output is shown task by task, once every live host has reported that task.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use volant_protocol::modules::short_name;
use volant_protocol::{BatchOutcome, FromAgent, Task, TaskResult, ToAgent};

use crate::agent::{AgentLink, AgentSource};
use crate::compile::{
    Compiled, IncludeKind, IncludeRequest, IncludeTarget, Step, StepKind, after, after_failure,
    after_pending, first, rescue_target,
};
use crate::inventory::Host;
use crate::playbook::{Play, PlayTask};
use crate::render::{Renderer, ansible_json};
use crate::stats::{Outcome, Stats};
use crate::template::{Templar, TemplateError, Vars};
use crate::transport::{ConnectError, ConnectionDefaults, Escalation, Transport};
use crate::vars::{HostVars, Scope, VarStore, load_vars_file, omit_token};

/// Ansible's default `timeout`: seconds to establish a connection.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a host gets to confirm a cancel before it is abandoned.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Settings shared by every play of a run.
#[derive(Clone)]
pub struct RunOptions {
    /// Connection settings every host starts from; its host variables override them.
    pub defaults: ConnectionDefaults,
    /// How many hosts of a play run at once, Ansible's `forks`. Never zero.
    pub forks: usize,
    /// `--force-handlers`, or `[defaults] force_handlers`: whether a host that failed still runs
    /// the handlers it notified. A play saying so itself speaks over this.
    pub force_handlers: bool,
    /// Flips to `true` once when the user interrupts the run.
    pub stop: watch::Receiver<bool>,
}

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
}

/// Names whose value depends on what the other hosts have done: another host's variables, and
/// the play's own live host list. A task whose raw text mentions one of them must not run ahead
/// of the others, so `linear` puts a boundary in front of it.
const CROSS_HOST_NAMES: [&str; 3] = ["hostvars", "play_hosts", "play_batch"];

/// Whether the hosts of a play meet in front of this step: one the keyword table declares a
/// synchronisation point, or one whose text names another host's state. The step loop asks it
/// twice on the way through an iteration, once to give back a fork permit kept from the batch
/// before and once to stop and wait for the other hosts. What a batch end asks instead is
/// whether this driver carries straight on, which is a different question.
fn is_boundary(task: &PlayTask) -> bool {
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

/// Which agent a kept connection belongs to. The escalated user is part of the identity
/// because two connections to one host under two users are two different agents.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LinkKey {
    pub host: String,
    pub become_user: Option<String>,
}

/// What outlives a play: the templar, the variable store hosts write into, the hosts that
/// are out of the run, and the agents still connected.
pub struct RunState {
    pub templar: Arc<Templar>,
    pub vars: Arc<Mutex<VarStore>>,
    pub failed_hosts: HashSet<String>,
    pub verbosity: u8,
    /// Agents kept alive between plays, the way Ansible keeps its ssh connections open. Only
    /// the connecting user's own link per host is kept; see `keep_links`.
    pub links: HashMap<LinkKey, AgentLink>,
}

impl RunState {
    /// Closes every kept connection, concurrently: each `shutdown` waits up to a couple of
    /// seconds for its own agent, and a run of N hosts closing them one at a time would pay
    /// N times that before the recap ever shows. Draining the map makes a second call a no-op,
    /// so the recap paths can each ask for it without closing one agent twice.
    pub async fn shutdown_links(&mut self) {
        let mut closing = tokio::task::JoinSet::new();
        for (_, link) in self.links.drain() {
            closing.spawn(link.shutdown());
        }
        while closing.join_next().await.is_some() {}
    }
}

enum Event {
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

/// One expansion a host asked for at an include step.
struct IncludeGroup {
    /// What two hosts have to agree on to share one `included:` line. See
    /// [`crate::compile::IncludeRequest::key`].
    key: String,
    /// What that line names: the file's absolute path, or the role's bare name.
    what: String,
    /// The loop item's label, for the `=> (item=...)` the line carries.
    label: Option<String>,
    /// What the statement hands down to everything it brought in.
    params: Map<String, Value>,
    /// The steps, blocks, roles and handlers the statement brought in, numbered from zero.
    expanded: Compiled,
}

/// Everything a host driver needs about the play, shared read-only.
struct PlayPlan {
    /// The play flattened into numbered steps, with the block spans that say where each one
    /// sits. Every host walks the same list, which is what lets the coordinator name a step to
    /// all of them at once.
    ///
    /// It is a watch rather than a value because of the one thing in a play that is not known
    /// when it is compiled: at a flush point the coordinator inserts the handler steps behind
    /// the flush and publishes the new list here. Every index a driver holds is still the index
    /// it was holding, because no driver reads the list again between reporting a flush point
    /// and the splice behind it - see [`Compiled::splice`] and `wait_for_splice`.
    plan: watch::Receiver<Arc<Compiled>>,
    /// Whether a host that failed still runs the handlers it notified: the play's own keyword,
    /// or the run's `--force-handlers` when the play says nothing.
    force_handlers: bool,
    play_vars: Map<String, Value>,
    /// The `vars_files` maps of each host, in the order the play lists the files.
    vars_files: HashMap<String, Vec<Map<String, Value>>>,
    /// Every host the play resolved, whatever batch it belongs to and whether it has failed or
    /// not: `ansible_play_hosts_all`. Fixed for the whole play, batches included.
    all_play_hosts: Vec<String>,
    r#become: Option<bool>,
    become_user: Option<String>,
    /// Every host the inventory resolved, by name, for `delegate_to` to look one up in. The
    /// whole inventory rather than the play's own hosts: measured on ansible-core 2.19.12, a
    /// play over h1 and h2 delegates to h3 without h3 being in the play at all, and h3 stays
    /// out of the recap.
    inventory: Arc<HashMap<String, Host>>,
}

impl PlayPlan {
    /// The step list as it stands. An `Arc` clone, so the watch is never held across an `await`.
    fn steps(&self) -> Arc<Compiled> {
        self.plan.borrow().clone()
    }
}

/// How a play ended, for the run that contains it.
pub struct PlayEnd {
    /// Every live host of a batch failed, so the run is over: no further batch of this play, no
    /// further play, and the recap prints where it stands. Measured on ansible-core 2.19.12,
    /// three shapes of the same rule - two hosts failing in a one-batch play, one host failing
    /// in a `serial: 1` batch, and a whole batch going unreachable - and the run stops at all
    /// three, at exit 2 for the failures and 4 for the unreachable batch.
    pub stop_run: bool,
}

/// Runs one play, batch by batch.
///
/// `hosts` is the play's pattern resolved and narrowed by `--limit`, with **nothing filtered
/// out**: the hosts that failed in an earlier play are still in it, because the reference cuts
/// its batches from that list and only then drops the failures from each batch. Cutting the
/// filtered list instead merges two batches into one whenever an earlier play lost a host.
#[allow(clippy::too_many_arguments)]
pub async fn run_play(
    play: &Play,
    compiled: &Compiled,
    hosts: Vec<Host>,
    inventory: Arc<HashMap<String, Host>>,
    agents: &AgentSource,
    options: &RunOptions,
    state: &mut RunState,
    out: &mut Renderer,
    stats: &mut Stats,
) -> anyhow::Result<PlayEnd> {
    // A pattern that matched nothing is the one case with no batch at all: the reference prints
    // the banner and says so, and the play keyword that would have cut the batches is never read.
    if hosts.is_empty() {
        out.play(&play.name);
        out.no_hosts();
        return Ok(PlayEnd { stop_run: false });
    }
    let play_vars = {
        let store = state.vars.lock().expect("vars lock");
        store.play_scope(&play.vars)
    };
    let batches =
        crate::compile::batches(&hosts, play.serial.as_ref(), &state.templar, &play_vars)?;
    if play.gather_facts {
        out.warning("gather_facts is not available in this release; continuing without facts");
    }
    let all: Vec<String> = hosts.iter().map(|h| h.name.clone()).collect();
    let playbook_dir = state
        .vars
        .lock()
        .expect("vars lock")
        .playbook_dir()
        .to_path_buf();
    for batch in batches {
        // A batch that starts after the operator interrupted the run is a batch that should not
        // start: the drivers of the batch before it have already stopped for the same reason.
        if *options.stop.borrow() {
            break;
        }
        // One banner per batch, measured - and it prints even when the batch has nothing left to
        // run, which is the shape a play gets when every host it resolved has already failed.
        out.play(&play.name);
        let live: Vec<Host> = batch
            .into_iter()
            .filter(|h| !state.failed_hosts.contains(&h.name))
            .collect();
        if live.is_empty() {
            continue;
        }
        // Read here, once per batch about to run and not once for the whole play: see
        // `load_play_vars_files`. `play_hosts` is the play's still-live hosts computed from
        // `state.failed_hosts` as it stands right before this batch starts (this batch and every
        // batch still to come); `batch_hosts` is this batch's own live hosts; `all` stays the
        // play's whole resolved list, batches and failures alike.
        let play_hosts: Vec<String> = hosts
            .iter()
            .filter(|h| !state.failed_hosts.contains(&h.name))
            .map(|h| h.name.clone())
            .collect();
        let batch_hosts: Vec<String> = live.iter().map(|h| h.name.clone()).collect();
        let vars_files = load_play_vars_files(
            play,
            &live,
            &play_hosts,
            &batch_hosts,
            &all,
            &playbook_dir,
            state,
            out,
        )?;
        run_batch(
            play,
            compiled,
            &live,
            &all,
            &vars_files,
            Arc::clone(&inventory),
            agents,
            options,
            state,
            out,
            stats,
        )
        .await?;
        // The run ends where the reference ends it: a batch that had hosts to run and lost every
        // one of them. A batch that had none to begin with is not that - measured, `serial: 1`
        // over a host that failed in an earlier play prints its banner and the next batch runs.
        if live.iter().all(|h| state.failed_hosts.contains(&h.name)) {
            return Ok(PlayEnd { stop_run: true });
        }
    }
    Ok(PlayEnd { stop_run: false })
}

/// Plays one batch of hosts: the coordinator, its drivers, and everything the play's step list
/// does between them. Without `serial` a play is one batch and this is the whole of it.
#[allow(clippy::too_many_arguments)]
async fn run_batch(
    play: &Play,
    compiled: &Compiled,
    hosts: &[Host],
    all: &[String],
    vars_files: &HashMap<String, Vec<Map<String, Value>>>,
    inventory: Arc<HashMap<String, Host>>,
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
        inventory,
    });

    let (tx, mut rx) = mpsc::channel::<Event>(64);
    // The batch's shared progress. Every driver reads it; only this loop writes it.
    let (progress_tx, progress_rx) = watch::channel(Progress::default());
    publish(
        &progress_tx,
        &play_hosts,
        all,
        &state.failed_hosts,
        &HashMap::new(),
        None,
        &BTreeMap::new(),
    );
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
                    .await
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
    // The index each host has finished every task through. Not the highest index it has
    // mentioned: a driver reports the steps it is about to step over as soon as it has decided
    // to, which is before the batch in front of them has run, so its reports do arrive out of
    // order. See `finished`.
    let mut frontier: HashMap<String, usize> = HashMap::new();
    // The last splice point the coordinator has published a splice for, republished with every
    // `Progress` so a driver waiting on one reads it whatever else moved.
    let mut spliced: Option<usize> = None;
    // What the one host elected for each `run_once` step made of it, for the hosts waiting
    // behind it. Filled here rather than by the elected driver because the failure has to be
    // published **with** the live-set change it causes: a failed result takes its host out of
    // `live_hosts` in this same arm, and a host that saw the shrunken list before the verdict
    // would read a leader that is merely gone and leave the play the quiet way, at the wrong
    // exit code.
    let mut run_once: BTreeMap<usize, bool> = BTreeMap::new();
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
                            run_once.insert(key.1, true);
                        }
                        // A failed result is the host leaving the play, and it arrives before
                        // that task's `TaskDone`. Taking it out of the live set here, rather
                        // than waiting for its `Finished`, is what lets the next task read the
                        // shrunken host list without racing the driver that is shutting down.
                        if lost {
                            state.failed_hosts.insert(key.0.clone());
                        }
                        if lost || announced {
                            publish(
                                &progress_tx,
                                &play_hosts,
                                all,
                                &state.failed_hosts,
                                &frontier,
                                spliced,
                                &run_once,
                            );
                        }
                        pending.entry(key).or_default().push(event);
                    }
                    // Queued behind the same key as a result, rather than printed on arrival,
                    // so a host racing ahead cannot put its banner above the step before it.
                    Some(event @ (Event::Banner { .. } | Event::Retrying { .. })) => {
                        let (host, index) = match &event {
                            Event::Banner { host, index } | Event::Retrying { host, index, .. } => {
                                (host.clone(), *index)
                            }
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
                        finished(&mut done, &mut frontier, &host, index);
                        // The elected host is the first to report this step - every other host
                        // waits for this entry before it sends its own - so `or_insert` keeps
                        // the verdict of the one that ran the task and not the silence of the
                        // ones that did not.
                        //
                        // A host that has already failed is the one exception to that order: it
                        // is out of the live set, so it was never a candidate for the election,
                        // and under `force_handlers` it walks the rest of the play reporting
                        // every step it passes on its way to its handlers. Its report is not a
                        // verdict, and taking it as one would release the waiting hosts before
                        // the runner had run.
                        if !state.failed_hosts.contains(&host)
                            && plan
                                .steps()
                                .steps
                                .get(index)
                                .is_some_and(|s| s.task.runs_once())
                        {
                            run_once.entry(index).or_insert(false);
                        }
                        publish(
                            &progress_tx,
                            &play_hosts,
                            all,
                            &state.failed_hosts,
                            &frontier,
                            spliced,
                            &run_once,
                        );
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
                        publish(
                            &progress_tx,
                            &play_hosts,
                            all,
                            &state.failed_hosts,
                            &frontier,
                            spliced,
                            &run_once,
                        );
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
                        publish(
                            &progress_tx,
                            &play_hosts,
                            all,
                            &state.failed_hosts,
                            &frontier,
                            spliced,
                            &run_once,
                        );
                    }
                    None => break,
                }
            }
        }
        // Every host is now either done with this step or out of the play, which is the moment
        // the plan's one dynamic primitive is safe: no driver can be past this index, because a
        // driver that reported a flush point waits here, and a driver that stepped over one
        // waits there too. The handler steps go in behind it and the new list is published.
        //
        // A play with no handlers splices nothing and still publishes, because the drivers
        // waiting on it have no way to know that and would wait for ever.
        if crate::compile::is_splice_point(&step.kind) {
            let mut next = { (**plan_tx.borrow()).clone() };
            match step.kind {
                StepKind::Flush { .. } => {
                    let steps = crate::compile::handler_steps(&next, index);
                    next.splice(index + 1, steps);
                }
                // The requests of every host, grouped so two hosts that asked for the same thing
                // share one `included:` line and one copy of the steps behind it. First
                // appearance decides the order, walked in the play's host order and then in each
                // host's item order, which is the order the reference prints them in.
                _ => {
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
            }
            plan_tx.send_replace(Arc::new(next));
            spliced = Some(index);
            publish(
                &progress_tx,
                &play_hosts,
                all,
                &state.failed_hosts,
                &frontier,
                spliced,
                &run_once,
            );
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
                // Into `gone` here as well as in the loop above, because the last thing this
                // function does is count an `unreachable` for every worker that failed and is
                // not in that set. A host reported here and left out of it would be counted
                // twice in the recap for one failure.
                gone.insert(host.clone());
                state.failed_hosts.insert(host);
                publish(
                    &progress_tx,
                    &play_hosts,
                    all,
                    &state.failed_hosts,
                    &frontier,
                    spliced,
                    &run_once,
                );
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
                publish(
                    &progress_tx,
                    &play_hosts,
                    all,
                    &state.failed_hosts,
                    &frontier,
                    spliced,
                    &run_once,
                );
            }
            Event::TaskDone { host, index } => {
                finished(&mut done, &mut frontier, &host, index);
                // A host already in `failed_hosts` is walking the rest of the play for its
                // handlers alone; its report is not a verdict, the same way it is not one above.
                if !state.failed_hosts.contains(&host)
                    && plan
                        .steps()
                        .steps
                        .get(index)
                        .is_some_and(|s| s.task.runs_once())
                {
                    run_once.entry(index).or_insert(false);
                }
                publish(
                    &progress_tx,
                    &play_hosts,
                    all,
                    &state.failed_hosts,
                    &frontier,
                    spliced,
                    &run_once,
                );
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
            event @ (Event::Result { .. } | Event::Retrying { .. }) => {
                report_result(event, stats, out)
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

/// Files one finished step and moves that host's frontier: the index it has finished every step
/// through, which is the only figure a barrier may open on.
///
/// The frontier is not the highest index a host has mentioned. A driver collecting a batch
/// decides where it goes next before the batch has run, and tells the coordinator about the
/// steps it stepped over there and then, so a report for step 9 can arrive while step 7 is
/// still on the wire. Counting the highest would open another host's `hostvars` barrier on work
/// this one has not started. A gap closes when the batch reports; a gap that never closes
/// belongs to a host that failed or went unreachable, and such a host is out of the live set,
/// so nothing waits on it.
fn finished(
    done: &mut HashSet<(String, usize)>,
    frontier: &mut HashMap<String, usize>,
    host: &str,
    index: usize,
) {
    done.insert((host.to_string(), index));
    let mut next = frontier.get(host).map_or(0, |reached| reached + 1);
    while done.contains(&(host.to_string(), next)) {
        frontier.insert(host.to_string(), next);
        next += 1;
    }
}

/// Republishes the batch's progress. `live_hosts` is the batch's own list minus the hosts that
/// have failed or gone unreachable, which is what `ansible_play_batch` reports and the only list
/// a wait may open on; `play_hosts_left` is the same subtraction over the whole play, which is
/// what `ansible_play_hosts` reports. `completed_through` is the lowest frontier among the live
/// hosts of the batch, so a driver waiting for it to reach `i - 1` is waiting only on hosts that
/// are still expected to report - and never on a host of another batch, which has no driver here
/// to report at all.
#[allow(clippy::too_many_arguments)]
fn publish(
    tx: &watch::Sender<Progress>,
    batch_hosts: &[String],
    all_hosts: &[String],
    lost: &HashSet<String>,
    frontier: &HashMap<String, usize>,
    spliced_through: Option<usize>,
    run_once: &BTreeMap<usize, bool>,
) {
    let live_hosts: Vec<String> = batch_hosts
        .iter()
        .filter(|h| !lost.contains(*h))
        .cloned()
        .collect();
    let play_hosts_left: Vec<String> = all_hosts
        .iter()
        .filter(|h| !lost.contains(*h))
        .cloned()
        .collect();
    // `None` sorts below every `Some`, so a live host that has reported nothing yet holds the
    // minimum at `None` and no barrier opens on it.
    let completed_through = live_hosts
        .iter()
        .map(|h| frontier.get(h).copied())
        .min()
        .flatten();
    tx.send_replace(Progress {
        completed_through,
        live_hosts,
        play_hosts_left,
        spliced_through,
        run_once: run_once.clone(),
    });
}

/// Files the connections one host handed back, keeping only the one worth keeping.
///
/// What connection persistence buys is the connection to the host itself, and that is the
/// `become_user: None` link: it is the one whose `ssh`, host key exchange and authentication a
/// later play would otherwise pay for again. An escalated link is cheap to reopen on top of it,
/// because its bootstrap short-circuits on the agent already cached for that user, so it is
/// closed here instead of idling until the recap.
///
/// A driver normally releases its escalated links with its fork permit, well before it gets
/// here; this is what closes the ones a driver still held when it left the play, and everything
/// a failed host hands back. Between the two, no escalated link outlives the batch that needed
/// it, and the run carries one link per host to the recap instead of one per host per user.
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

/// The keys of the connections that escalate, the ones a driver only holds while it holds a
/// fork permit.
fn escalated_links(links: &HashMap<LinkKey, AgentLink>) -> Vec<LinkKey> {
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

/// Drops every value the `omit` variable rendered to, at any depth. Measured against
/// ansible-core 2.19.12, where `omit` is a sentinel the templating engine removes from any
/// container it survives in, sequences included: a mapping loses the key and a list loses the
/// element. Only the value goes, never the container around it, so `{k: omit}` comes back as an
/// empty mapping and `[omit]` as an empty list.
fn remove_omit(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, v| v.as_str() != Some(omit_token()));
            map.values_mut().for_each(remove_omit);
        }
        Value::Array(items) => {
            items.retain(|v| v.as_str() != Some(omit_token()));
            items.iter_mut().for_each(remove_omit);
        }
        _ => {}
    }
}

/// Resolves privilege escalation for one task: whether to escalate, to whom, and with what
/// password. `None` means the task runs as the connecting user.
///
/// The order is measured, not assumed. Against `ansible-core 2.19.12`: a host's
/// `ansible_become` variable beats both keywords in both directions (a play `become: false`
/// with `ansible_become=true` ran as root, and a task `become: true` with
/// `ansible_become=false` ran as the invoking user), and between the keywords the task beats
/// the play. `ansible_become_user` follows the same order. The connection defaults, which carry
/// `ansible.cfg`, its environment variables and the command line, speak last.
fn become_for(
    task: &PlayTask,
    play: &PlayPlan,
    vars: &HostVars,
    defaults: &ConnectionDefaults,
    templar: &Templar,
) -> Result<Option<Escalation>, TemplateError> {
    let on = vars
        .get("ansible_become")
        .and_then(as_bool_value)
        .or(task.r#become)
        .or(play.r#become)
        .unwrap_or(defaults.r#become);
    if !on {
        return Ok(None);
    }
    // The method is refused here, for the task that would actually use it, the way the
    // playbook's own keyword is refused at load time. A task that escalates nowhere is not
    // affected by a method it never runs.
    //
    // A variable beats the defaults, which carry `ansible.cfg`, its environment variables and
    // the command line. The startup pass already refuses what it can see; this catches the same
    // value reaching an escalating task through `group_vars`, `host_vars`, `--extra-vars` or a
    // `set_fact`, and the defaults for a task whose escalation the startup pass could not see.
    match vars.get("ansible_become_method").and_then(Value::as_str) {
        Some(method) if method != crate::playbook::BECOME_METHOD => {
            return Err(TemplateError(format!(
                "ansible_become_method '{method}' is not supported yet"
            )));
        }
        Some(_) => {}
        None if defaults.become_method != crate::playbook::BECOME_METHOD => {
            return Err(TemplateError(format!(
                "become_method '{}' is not supported yet",
                defaults.become_method
            )));
        }
        None => {}
    }
    let user = match vars.get("ansible_become_user").and_then(Value::as_str) {
        Some(user) => user.to_string(),
        None => task
            .become_user
            .clone()
            .or_else(|| play.become_user.clone())
            .unwrap_or_else(|| defaults.become_user.clone()),
    };
    // `become_user: "{{ app_user }}"` is ordinary Ansible, and the rendered name is what the
    // link is keyed by, so it has to be resolved before the connection is opened.
    //
    // It renders against the task's variables and not against one loop item's, so a
    // `become_user` naming the loop variable has nothing to render against and the task fails.
    // The reference escalates per item there, which is a divergence: one batch is one message
    // to one agent under one user, and a task whose items each want a different user would have
    // to split across links and interleave the answers. Saying which keyword could not be
    // rendered, and why, is what keeps that from reading as the operator's own typo - it used
    // to fail with nothing but "an option with an undefined variable".
    //
    // The name of the loop variable only explains the failure on a task that actually loops.
    // On a loopless task it is an ordinary undefined variable that happens to be spelled like
    // one, and the divergence is not what went wrong.
    let user = if Templar::is_template(&user) {
        templar
            .render(&user, vars)
            .map_err(|err| {
                let hint = match (task.loop_items.is_some(), user.contains(&task.loop_var)) {
                    (true, true) => {
                        ". A 'become_user' that changes per loop item is not supported yet: one \
                         batch escalates to one user"
                            .to_string()
                    }
                    (false, true) => format!(
                        ". '{}' is only defined while a task loops, and this task has no 'loop'",
                        task.loop_var
                    ),
                    _ => String::new(),
                };
                TemplateError(format!("rendering 'become_user' {user}: {}{hint}", err.0))
            })?
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| {
                TemplateError(format!("'become_user' must render to a user name: {user}"))
            })?
    } else {
        user
    };
    let password = vars
        .get("ansible_become_password")
        .or_else(|| vars.get("ansible_become_pass"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| defaults.become_password.clone());
    Ok(Some(Escalation { user, password }))
}

/// A variable's boolean, whether the inventory typed it as one or spelled it the way Ansible
/// content spells one (`yes`, `on`, `"true"`).
pub(crate) fn as_bool_value(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::String(s) => crate::yaml::bool_from_str(s.trim()),
        _ => None,
    }
}

/// `vars_files` paths are templates over play vars and the host's own variables, and Ansible
/// resolves them per host, so two hosts can read two different files. Each rendered path is
/// read once per call.
///
/// Called once per batch, not once per play: measured against ansible-core 2.19.12, a
/// `vars_files` path templated on `ansible_play_batch` reads a different file for each batch
/// (`serial/two.yml`'s shape, extended with a `vars_files` entry), so the reference re-renders
/// and re-reads it per batch rather than once at the play's start. `hosts` is therefore the
/// batch's own live hosts, and `play_hosts` / `batch_hosts` / `all_play_hosts` are the same three
/// lists `ansible_play_hosts`, `ansible_play_batch` and `ansible_play_hosts_all` are served from
/// everywhere else.
///
/// An entry that names nothing is not an error: the reference skips a `vars_files` path that
/// does not exist without a word and runs the play, and warns once for an entry whose template
/// has no value. Every other template failure stops the run, as it does there. A file that is
/// there but cannot be read stops the run too.
#[allow(clippy::too_many_arguments)]
fn load_play_vars_files(
    play: &Play,
    hosts: &[Host],
    play_hosts: &[String],
    batch_hosts: &[String],
    all_play_hosts: &[String],
    playbook_dir: &Path,
    state: &RunState,
    out: &mut Renderer,
) -> anyhow::Result<HashMap<String, Vec<Map<String, Value>>>> {
    let mut per_host = HashMap::new();
    if play.vars_files.is_empty() {
        return Ok(per_host);
    }
    let mut loaded: HashMap<PathBuf, Map<String, Value>> = HashMap::new();
    // One warning per entry, not one per host: two hosts reading the same unresolvable entry
    // are one complaint.
    let mut warned: HashSet<&String> = HashSet::new();
    for host in hosts {
        let scope = Scope {
            play_vars: play.vars.clone(),
            play_hosts: play_hosts.to_vec(),
            batch_hosts: batch_hosts.to_vec(),
            all_play_hosts: all_play_hosts.to_vec(),
            ..Scope::default()
        };
        let vars = state.templar.resolve_vars(
            &state
                .vars
                .lock()
                .expect("vars lock")
                .for_host(&host.name, &scope),
        );
        let mut files = Vec::new();
        for raw in &play.vars_files {
            let rendered = match state.templar.render(raw, &vars) {
                Ok(rendered) => rendered,
                // Only a variable without a value is recoverable. Every other template failure
                // stops the run there, and swallowing them all as one undefined variable both
                // named the wrong cause and let a broken playbook exit 0.
                Err(err) if err.is_undefined() => {
                    if warned.insert(raw) {
                        out.warning("skipping vars_files item due to an undefined variable");
                    }
                    continue;
                }
                Err(err) => anyhow::bail!("rendering vars_files entry {raw}: {err}"),
            };
            // Exit 4, the reference's own code for a playbook it cannot make sense of, measured
            // alongside the wording. A template that fails to render stays at 1, also measured:
            // the reference splits those two the same way.
            let path = rendered.as_str().ok_or_else(|| {
                crate::stats::Refusal::at(
                    4,
                    format!(
                        "Invalid `vars_files` value of type '{}'. A `vars_files` value should \
                         either be a string or list of strings.",
                        python_type(&rendered)
                    ),
                )
            })?;
            let path = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                playbook_dir.join(path)
            };
            // `exists` answers "no" both for a path that is not there and for one it cannot
            // stat, a parent directory refusing access included. The reference asks the same
            // question the same way and skips either without a word, so both stay a skip here.
            if !path.exists() {
                continue;
            }
            files.push(match loaded.get(&path) {
                Some(file) => file.clone(),
                None => {
                    // Exit 4 as well: measured, a `vars_files` entry naming a file whose YAML
                    // does not parse stops the reference with the same code a broken playbook
                    // gets.
                    let file =
                        load_vars_file(&path).map_err(|err| crate::stats::Refusal::or(4, err))?;
                    loaded.insert(path, file.clone());
                    file
                }
            });
        }
        per_host.insert(host.name.clone(), files);
    }
    Ok(per_host)
}

/// The name Python would give a value, so a message about a mistyped playbook key reads the way
/// the reference's does.
fn python_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
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

/// The task's own name for the `FAILED - RETRYING` line: templated against `vars`, and never
/// role-prefixed the way `task_name`'s banner is - measured, the retry line of a task inside a
/// role carries the task's own name rather than the `role : name` the banner shows.
fn retry_name(task: &PlayTask, vars: &HostVars, templar: &Templar) -> String {
    if Templar::is_template(&task.name) {
        templar
            .render(&task.name, vars)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| task.name.clone())
    } else {
        task.name.clone()
    }
}

/// The merged, self-resolved variables of a host for one task. `live` is the progress the
/// coordinator last published: its batch list feeds `ansible_play_batch` and its play-wide list
/// `ansible_play_hosts`, the two being the same list until `serial` parts them.
///
/// `role` decides which of the two role layers the step sits on: its own role's, which carry the
/// whole play's exported values with that role's own laid over them, or the play-wide export for
/// a step that belongs to no role. Role parameters are the one layer that does not leave the
/// role - measured, a parameter beats a `set_fact` inside the role and is not defined at all in
/// the play's own tasks afterwards.
#[allow(clippy::too_many_arguments)]
fn host_vars(
    host: &str,
    plan: &PlayPlan,
    task_vars: &Map<String, Value>,
    role: Option<usize>,
    include_params: Option<&Map<String, Value>>,
    live: &Progress,
    templar: &Templar,
    store: &Mutex<VarStore>,
) -> HostVars {
    let compiled = plan.steps();
    let role = role
        .and_then(|i| compiled.roles.get(i))
        .unwrap_or(&compiled.exported);
    // What an include handed down joins the role parameters rather than the task variables:
    // measured, it beats a `set_fact` of the same name, which a task's own `vars:` does not.
    let mut role_params = role.params.clone();
    for (key, value) in include_params.into_iter().flatten() {
        role_params.insert(key.clone(), value.clone());
    }
    let scope = Scope {
        play_vars: plan.play_vars.clone(),
        vars_files: plan.vars_files.get(host).cloned().unwrap_or_default(),
        task_vars: task_vars.clone(),
        role_defaults: role.defaults.clone(),
        role_vars: role.vars.clone(),
        role_params,
        play_hosts: live.play_hosts_left.clone(),
        batch_hosts: live.live_hosts.clone(),
        all_play_hosts: plan.all_play_hosts.clone(),
    };
    let (raw, hostvars) = {
        let mut store = store.lock().expect("vars lock");
        // The shared view first: a variable of this host's own can name `hostvars`, and
        // `resolve_vars` below has to be able to answer it.
        let hostvars = store.hostvars_shared(host);
        (store.for_host(host, &scope), hostvars)
    };
    let map = templar.resolve_vars(Vars {
        map: &raw,
        hostvars: Some(&hostvars),
    });
    HostVars { map, hostvars }
}

/// A task rendered for one host: what to do with it.
enum Prepared {
    /// Every item (or the single non-loop item) had a false `when`: results are ready. No
    /// delegate travels with it: measured on ansible-core 2.19.12, a task a `when` left out
    /// prints `skipping: [h1]` with no arrow, because it never reached the delegate.
    Skipped(Vec<Item>),
    /// `set_fact` or `debug`: run on the controller. A `delegate_to` changes nothing about
    /// where it runs - measured, a delegated `debug` still runs here - so the delegate travels
    /// only as the name the line shows.
    Local(Vec<Item>, Option<String>),
    /// Send to the agent, one `Task` per item, over a link running as this task's escalated
    /// user. Escalation belongs to the task rather than to an item: it decides which agent on
    /// the host the whole task talks to, so every item of a loop shares it.
    ///
    /// The host is the delegate when the task has one, and that is the host the link is opened
    /// to, keyed by, and reused from: measured on ansible-core 2.19.12, a task delegated away
    /// from a host nothing can reach runs perfectly well, so the delegating host's own
    /// connection is never opened for it.
    Remote(Vec<Item>, Option<Escalation>, Option<Host>),
}

/// One loop item (or the whole task when there is no loop), rendered.
struct Item {
    /// The loop element, present only for loops.
    element: Option<Value>,
    label: Option<String>,
    args: Map<String, Value>,
    /// Variables in force for this item, for `changed_when`, `failed_when` and local modules.
    vars: HostVars,
    /// The variables the module runs with, this item's layers merged and rendered. Per item
    /// rather than per task because a layer's values are templates and may name the loop
    /// variable.
    environment: BTreeMap<String, String>,
    /// Set when `when` was false: the skip result to report.
    skipped: Option<TaskResult>,
}

/// One task's `environment`, layer by layer: the play's, then each block's, then the task's,
/// each rendered against this item's variables and merged over the ones before it.
///
/// Measured on ansible-core 2.19.12: a layer that does not render to a mapping warns on stderr
/// and is skipped while the task still runs, and the layers around it stay in force. Values are
/// what Python's `str()` makes of them - `42` is `"42"`, `true` is `"True"`, `null` is `"None"`.
fn environment_for(
    task: &PlayTask,
    vars: &HostVars,
    templar: &Templar,
) -> Result<BTreeMap<String, String>, TemplateError> {
    let mut out = BTreeMap::new();
    for layer in &task.environment {
        // The reference's own prefix for a keyword that cannot be rendered, measured: an
        // undefined variable in an `environment:` fails the task with
        // `Task failed: Error processing keyword 'environment': ...`.
        let rendered = templar.render_value(layer, vars).map_err(|e| {
            TemplateError(format!("Error processing keyword 'environment': {}", e.0))
        })?;
        let Value::Object(map) = rendered else {
            // Straight to stderr rather than through the renderer: `prepare` runs inside a host
            // driver, which has no renderer of its own, and every `[WARNING]` this engine prints
            // goes to stderr anyway.
            eprintln!(
                "[WARNING]: could not parse environment value, skipping: {}",
                ansible_json(&rendered)
            );
            continue;
        };
        for (k, v) in map {
            out.insert(k, environment_value(&v));
        }
    }
    Ok(out)
}

/// One environment value as the process sees it, which is what `str()` makes of the Python
/// object the reference hands to `os.environ`. Measured: `{B: true, N: null, S: plain}` reaches
/// the shell as `True`, `None` and `plain`, and `{N: 42}` as `42`.
///
/// A list or a mapping is not measured and is written as JSON here rather than as Python's own
/// repr; nothing in ansible's documentation suggests writing one.
fn environment_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Null => "None".to_string(),
        other => ansible_json(other),
    }
}

fn prepare(
    step: &Step,
    host: &str,
    plan: &PlayPlan,
    live: &Progress,
    templar: &Templar,
    store: &Mutex<VarStore>,
    defaults: &ConnectionDefaults,
) -> Result<Prepared, TemplateError> {
    let task = &step.task;
    let base = host_vars(
        host,
        plan,
        &task.vars,
        step.role,
        step.include_params.as_deref(),
        live,
        templar,
        store,
    );
    let elements: Vec<Option<Value>> = match &task.loop_items {
        None => vec![None],
        Some(raw) => {
            let rendered = templar.render_value(raw, &base)?;
            let list = match rendered {
                Value::Array(items) => items,
                other => {
                    return Err(TemplateError(format!(
                        "Invalid data passed to 'loop', it requires a list, got this instead: {other}"
                    )));
                }
            };
            // `with_items` flattens one level; `loop` does not.
            let list = if task.with_items {
                flatten_once(list)
            } else {
                list
            };
            list.into_iter().map(Some).collect()
        }
    };
    let mut items = Vec::new();
    for element in elements {
        let mut vars = base.clone();
        if let Some(el) = &element {
            vars.insert(task.loop_var.clone(), el.clone());
            vars.insert(
                "ansible_loop_var".into(),
                Value::String(task.loop_var.clone()),
            );
            // Variables naming the loop variable could not resolve before it was bound.
            let resolved = templar.resolve_vars(&vars);
            vars.map = resolved;
        }
        let label = match (&element, &task.loop_label) {
            (None, _) => None,
            (Some(_), Some(template)) => Some(display(&templar.render(template, &vars)?)),
            (Some(el), None) => Some(display(el)),
        };
        let mut skipped = None;
        for condition in &task.when {
            if !templar.condition(condition, &vars)? {
                let mut r = Map::new();
                r.insert("changed".into(), json!(false));
                r.insert("skipped".into(), json!(true));
                r.insert("skip_reason".into(), json!("Conditional result was False"));
                r.insert("false_condition".into(), json!(condition));
                skipped = Some(TaskResult(r));
                break;
            }
        }
        let args = if skipped.is_some() {
            Map::new()
        } else {
            let mut rendered = templar.render_value(&Value::Object(task.args.clone()), &vars)?;
            remove_omit(&mut rendered);
            let Value::Object(map) = rendered else {
                unreachable!("an object renders to an object")
            };
            map
        };
        // A skipped item runs nothing, so a layer it could not render is not its problem: the
        // reference does not evaluate `environment` for a task a `when` left out.
        let environment = if skipped.is_some() {
            BTreeMap::new()
        } else {
            environment_for(task, &vars, templar)?
        };
        items.push(Item {
            element,
            label,
            args,
            vars,
            environment,
            skipped,
        });
    }
    if items.iter().all(|i| i.skipped.is_some()) {
        return Ok(Prepared::Skipped(items));
    }
    let delegate = delegate_for(task, &base, templar)?.map(|name| delegate_host(&name, plan));
    if is_local(&task.module) {
        return Ok(Prepared::Local(items, delegate.map(|d| d.name)));
    }
    // From the delegating host's own variables, measured on ansible-core 2.19.12:
    // `ansible_become_user` under a `delegate_to` still reads the value the **delegating**
    // host carries, while `ansible_host` and `ansible_connection` read the delegate's. So the
    // link goes to the delegate and escalates to the user the task's own host asked for.
    let escalation = become_for(task, plan, &base, defaults, templar)?;
    Ok(Prepared::Remote(items, escalation, delegate))
}

/// `delegate_to`, rendered once for the task. An empty name is no delegation, which is what
/// `delegate_to: "{{ maybe | default('') }}"` renders to when nobody asked for one.
///
/// Rendered against the task's variables and **not** against one loop item's, the way
/// `become_user` is and for the same reason: one batch is one message to one agent over one
/// link, and a task whose items each name a different delegate would have to split across links
/// and interleave the answers. The reference does delegate per item; that divergence is named
/// here rather than left to look like the operator's own typo.
fn delegate_for(
    task: &PlayTask,
    vars: &HostVars,
    templar: &Templar,
) -> Result<Option<String>, TemplateError> {
    let Some(raw) = &task.delegate_to else {
        return Ok(None);
    };
    let name = if Templar::is_template(raw) {
        templar
            .render(raw, vars)
            .map_err(|err| {
                let hint = if task.loop_items.is_some() && raw.contains(&task.loop_var) {
                    ". A 'delegate_to' that changes per loop item is not supported yet: one batch runs over one link"
                        .to_string()
                } else {
                    String::new()
                };
                TemplateError(format!("rendering 'delegate_to' {raw}: {}{hint}", err.0))
            })?
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| {
                TemplateError(format!("'delegate_to' must render to a host name: {raw}"))
            })?
    } else {
        raw.clone()
    };
    Ok(Some(name).filter(|n| !n.is_empty()))
}

/// The host a `delegate_to` names: the inventory's own entry when it has one, an implicit host
/// otherwise.
///
/// Measured on ansible-core 2.19.12: `delegate_to: localhost` with no `localhost` in the
/// inventory runs locally (`changed: [h1 -> localhost]`), `127.0.0.1` does the same, and a name
/// the inventory has never heard of is **connected to** rather than refused - `delegate_to:
/// nosuch` reports `fatal: [h1 -> nosuch]: UNREACHABLE!` with ssh's own resolution error and
/// exits 4. So an unknown name is an ssh target of that name, not a load-time error.
fn delegate_host(name: &str, plan: &PlayPlan) -> Host {
    if let Some(host) = plan.inventory.get(name) {
        return host.clone();
    }
    let mut vars = BTreeMap::new();
    if matches!(name, "localhost" | "127.0.0.1" | "::1") {
        vars.insert("ansible_connection".to_string(), json!("local"));
    }
    Host {
        name: name.to_string(),
        vars,
    }
}

fn flatten_once(list: Vec<Value>) -> Vec<Value> {
    list.into_iter()
        .flat_map(|v| match v {
            Value::Array(inner) => inner,
            other => vec![other],
        })
        .collect()
}

fn is_local(module: &str) -> bool {
    volant_protocol::modules::local(module).is_some()
}

/// Ansible prints a string item as is and anything else as JSON.
fn display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => ansible_json(other),
    }
}

/// `include_vars`, run where every other controller-side module runs.
///
/// Measured on ansible-core 2.19.12: a file that is there sets each of its keys as a fact and
/// answers `{"ansible_facts": {...}, "ansible_included_var_files": ["<abs>"], "changed": false}`,
/// a `name:` puts the whole mapping under that one key, and a file that is nowhere fails the task
/// with the paths it looked in spelled out one per line.
///
/// A deliberate divergence, recorded rather than hidden: these land as facts, which is precedence
/// 20 here against the reference's own rank 19 for `include_vars`. The two differ only for a name
/// a host variable also carries.
fn run_include_vars(
    item: &Item,
    step: &Step,
    hosts: &[String],
    store: &Mutex<VarStore>,
) -> TaskResult {
    let Some(name) = item
        .args
        .get("file")
        .or_else(|| item.args.get("_raw_params"))
        .and_then(Value::as_str)
    else {
        return TaskResult::failed_with("Task failed: 'include_vars' takes a file name");
    };
    let playbook_dir = store
        .lock()
        .expect("vars lock")
        .playbook_dir()
        .to_path_buf();
    let searched = crate::vars::include_vars_paths(
        &step.origin.file_dir,
        step.origin.role_dir.as_deref(),
        &playbook_dir,
        name,
    );
    let Some(path) = searched.iter().find(|p| p.is_file()) else {
        // The reference's own wording and its own shape, measured: `ansible_facts` and
        // `ansible_included_var_files` are both present and empty, the paths are listed one per
        // tab-indented line, and `msg` carries the sentence a task that died rather than failed
        // gets.
        let mut body = Map::new();
        body.insert("ansible_facts".into(), json!({}));
        body.insert("ansible_included_var_files".into(), json!([]));
        body.insert("changed".into(), json!(false));
        body.insert(
            "message".into(),
            json!(format!(
                "Could not find or access '{name}'\nSearched in:\n{} on the Ansible Controller.\nIf you are using a module and expect the file to exist on the remote, see the remote_src option",
                searched
                    .iter()
                    .map(|p| format!("\t{}", p.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            )),
        );
        body.insert(
            "msg".into(),
            json!("Task failed: Action failed: Unknown error."),
        );
        body.insert("failed".into(), json!(true));
        return TaskResult(body);
    };
    let loaded = match crate::vars::load_vars_file(path) {
        Ok(map) => map,
        Err(err) => return TaskResult::failed_with(format!("Task failed: {err:#}")),
    };
    let facts = match item.args.get("name").and_then(Value::as_str) {
        Some(under) => {
            let mut one = Map::new();
            one.insert(under.to_string(), Value::Object(loaded));
            one
        }
        None => loaded,
    };
    {
        let mut vars = store.lock().expect("vars lock");
        for (key, value) in &facts {
            for host in hosts {
                vars.set_fact(host, key, value.clone());
            }
        }
    }
    let mut r = Map::new();
    r.insert("ansible_facts".into(), Value::Object(facts));
    r.insert(
        "ansible_included_var_files".into(),
        json!([path.display().to_string()]),
    );
    r.insert("changed".into(), json!(false));
    r.insert("failed".into(), json!(false));
    TaskResult(r)
}

/// `set_fact`, `debug` and `include_vars` never leave the controller.
fn run_local(
    task: &PlayTask,
    item: &Item,
    step: &Step,
    fact_hosts: &[String],
    templar: &Templar,
    store: &Mutex<VarStore>,
    verbosity: u8,
) -> TaskResult {
    let mut r = Map::new();
    match short_name(&task.module) {
        "include_vars" => return run_include_vars(item, step, fact_hosts, store),
        "set_fact" => {
            let mut facts = Map::new();
            for (k, v) in &item.args {
                if k == "cacheable" {
                    continue;
                }
                let mut vars = store.lock().expect("vars lock");
                for host in fact_hosts {
                    vars.set_fact(host, k, v.clone());
                }
                drop(vars);
                facts.insert(k.clone(), v.clone());
            }
            r.insert("ansible_facts".into(), Value::Object(facts));
            r.insert("changed".into(), json!(false));
            r.insert("failed".into(), json!(false));
        }
        "debug" => {
            let wanted: u8 = item
                .args
                .get("verbosity")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u8;
            if wanted > verbosity {
                r.insert("skipped".into(), json!(true));
                r.insert(
                    "skipped_reason".into(),
                    json!("Verbosity threshold not met."),
                );
                r.insert("changed".into(), json!(false));
                return TaskResult(r);
            }
            if let Some(var) = item.args.get("var").and_then(Value::as_str) {
                match templar.evaluate(var, &item.vars) {
                    Ok(v) => r.insert(var.to_string(), v),
                    Err(e) => r.insert(
                        var.to_string(),
                        Value::String(format!("VARIABLE IS NOT DEFINED!: {e}")),
                    ),
                };
            } else {
                r.insert(
                    "msg".into(),
                    item.args
                        .get("msg")
                        .cloned()
                        .unwrap_or_else(|| json!("Hello world!")),
                );
            }
            // The callback never prints these for a `debug`, but `register` stores them, so a
            // later `when: reg.changed` or `when: not reg.failed` has something to read. The
            // renderer drops them again on the way out.
            r.insert("changed".into(), json!(false));
            r.insert("failed".into(), json!(false));
        }
        "validate_argument_spec" => return validate_argument_spec(item),
        other => {
            return TaskResult::failed_with(format!("{other} is not a controller-side module"));
        }
    }
    TaskResult(r)
}

/// The argument check a role with a `meta/argument_specs.yml` gets in front of it.
///
/// Where the arguments come from is measured, not assumed: a value the role entry never named
/// is still found when the host has a variable of that name (a `-e` on the command line passes
/// the check), while a name that is **not** in the spec is refused only when the entry wrote it,
/// because every host carries hundreds of variables the spec has never heard of.
///
/// The four checks run in the order the reference runs them, measured by asking for all four
/// failures at once: missing required arguments first, then types, then choices, then the
/// arguments the spec does not have. Each sentence is the reference's own.
///
/// What is **not** checked here, and is written down in the record rather than left to be
/// discovered: `aliases`, `default`, sub-options (`options` inside an option), `mutually_
/// exclusive` and the rest of the spec's vocabulary. A converted value is not written back
/// either, which is what the reference does too - measured, a role reading `count: "3"` against
/// a `type: int` sees the string it was given.
fn validate_argument_spec(item: &Item) -> TaskResult {
    let spec = match item.args.get("argument_spec") {
        Some(Value::Object(spec)) => spec.clone(),
        _ => Map::new(),
    };
    let provided = match item.args.get("provided_arguments") {
        Some(Value::Object(provided)) => provided.clone(),
        _ => Map::new(),
    };
    let mut errors: Vec<String> = Vec::new();
    // Every option, with the value in force for it: the entry's own first, then whatever the
    // host can see under that name.
    let value_of =
        |name: &str| -> Option<&Value> { provided.get(name).or_else(|| item.vars.get(name)) };
    let mut missing: Vec<&String> = spec
        .iter()
        .filter(|(name, option)| {
            option
                .get("required")
                .and_then(as_bool_value)
                .unwrap_or(false)
                && !matches!(value_of(name), Some(v) if !v.is_null())
        })
        .map(|(name, _)| name)
        .collect();
    missing.sort();
    if !missing.is_empty() {
        errors.push(format!(
            "missing required arguments: {}",
            missing
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for (name, option) in &spec {
        let Some(value) = value_of(name) else {
            continue;
        };
        if let Some(wanted) = option.get("type").and_then(Value::as_str)
            && let Err(err) = converts(value, wanted)
        {
            errors.push(format!(
                "argument '{name}' is of type {} and we were unable to convert to {wanted}: {err}",
                python_type(value)
            ));
        }
    }
    for (name, option) in &spec {
        let (Some(Value::Array(choices)), Some(value)) = (option.get("choices"), value_of(name))
        else {
            continue;
        };
        if !choices.iter().any(|c| c == value) {
            errors.push(format!(
                "value of {name} must be one of: {}, got: {}",
                choices.iter().map(display).collect::<Vec<_>>().join(", "),
                display(value)
            ));
        }
    }
    let mut supported: Vec<&str> = spec.keys().map(String::as_str).collect();
    supported.sort_unstable();
    for name in provided.keys() {
        if !spec.contains_key(name) {
            errors.push(format!(
                "{name}. Supported parameters include: {}.",
                supported.join(", ")
            ));
        }
    }
    let context = item
        .args
        .get("validate_args_context")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    let mut r = Map::new();
    r.insert("changed".into(), json!(false));
    if errors.is_empty() {
        r.insert("msg".into(), json!("The arg spec validation passed"));
        r.insert("validate_args_context".into(), context);
        return TaskResult(r);
    }
    r.insert(
        "msg".into(),
        json!(format!(
            "Validation of arguments failed:\n{}",
            errors.join("\n")
        )),
    );
    r.insert("argument_errors".into(), json!(errors));
    r.insert("argument_spec_data".into(), Value::Object(spec));
    r.insert("validate_args_context".into(), context);
    r.insert("failed".into(), json!(true));
    TaskResult(r)
}

/// Whether a value can be read as the type the spec asked for, with the reference's own
/// complaint when it cannot.
///
/// Ansible converts permissively and this follows it where the conversion was measured: a string
/// `"3"` is an `int`, `"yes"` is a `bool`. Only the two numeric types and `bool` can fail; `str`,
/// `path`, `raw`, `list` and `dict` take what they are given, which is what the reference does
/// with them for every shape a playbook can write.
fn converts(value: &Value, wanted: &str) -> Result<(), String> {
    let quoted = || {
        format!(
            "\"'{}'\" cannot be converted to an {wanted}",
            display(value)
        )
    };
    match wanted {
        "int" => match value {
            Value::Number(n) if n.is_i64() || n.is_u64() => Ok(()),
            Value::String(s) if s.trim().parse::<i64>().is_ok() => Ok(()),
            _ => Err(quoted()),
        },
        "float" => match value {
            Value::Number(_) => Ok(()),
            Value::String(s) if s.trim().parse::<f64>().is_ok() => Ok(()),
            _ => Err(format!(
                "\"'{}'\" cannot be converted to a {wanted}",
                display(value)
            )),
        },
        "bool" => match value {
            Value::Bool(_) => Ok(()),
            Value::String(s) if crate::yaml::bool_from_str(s.trim()).is_some() => Ok(()),
            Value::Number(n) if n.as_i64() == Some(0) || n.as_i64() == Some(1) => Ok(()),
            _ => Err(format!(
                "\"'{}'\" cannot be converted to a {wanted}",
                display(value)
            )),
        },
        _ => Ok(()),
    }
}

/// One module result, ready to report: `changed_when` and `failed_when` decide its outcome
/// wherever the module ran, controller side as well as on the agent.
fn finish(task: &PlayTask, item: &Item, result: TaskResult, templar: &Templar) -> TaskResult {
    match apply_conditions(task, item, result, templar) {
        Ok(r) => r,
        Err(e) => TaskResult::failed_with(e.0),
    }
}

/// Applies `changed_when` and `failed_when` to one result, with `result` bound to it.
fn apply_conditions(
    task: &PlayTask,
    item: &Item,
    mut result: TaskResult,
    templar: &Templar,
) -> Result<TaskResult, TemplateError> {
    if task.changed_when.is_empty() && task.failed_when.is_empty() {
        return Ok(result);
    }
    let mut vars = item.vars.clone();
    if let Some(reg) = &task.register {
        vars.insert(reg.clone(), Value::Object(result.0.clone()));
    }
    vars.insert("result".into(), Value::Object(result.0.clone()));
    if !task.changed_when.is_empty() {
        let changed = all_hold(&task.changed_when, &vars, templar)?;
        result.0.insert("changed".into(), json!(changed));
    }
    if !task.failed_when.is_empty() {
        let failed = all_hold(&task.failed_when, &vars, templar)?;
        result.0.insert("failed_when_result".into(), json!(failed));
        if failed {
            result.0.insert("failed".into(), json!(true));
        } else {
            result.0.remove("failed");
            // A non-zero rc would still count as failed: the condition has spoken.
            if result.failed() {
                result.0.insert("failed".into(), json!(false));
            }
        }
    }
    Ok(result)
}

fn all_hold(
    conditions: &[String],
    vars: &HostVars,
    templar: &Templar,
) -> Result<bool, TemplateError> {
    for c in conditions {
        if !templar.condition(c, vars)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// What a loop over an empty list reports and registers. The reference shows the task as
/// `skipping` and gives `skipped_reason` this exact wording, with no items behind it.
fn empty_loop_result() -> TaskResult {
    let mut r = Map::new();
    r.insert("changed".into(), json!(false));
    r.insert("skipped".into(), json!(true));
    r.insert("skipped_reason".into(), json!("No items in the list"));
    TaskResult(r)
}

/// What `register` stores: the single result, or Ansible's loop aggregate.
fn registered_value(task: &PlayTask, results: &[(Option<Value>, TaskResult)]) -> Value {
    if task.loop_items.is_none() {
        return results
            .first()
            .map(|(_, r)| Value::Object(r.0.clone()))
            .unwrap_or(Value::Null);
    }
    if results.is_empty() {
        let mut agg = empty_loop_result().0;
        agg.insert("results".into(), Value::Array(Vec::new()));
        return Value::Object(agg);
    }
    let mut list = Vec::new();
    for (element, r) in results {
        let mut m = r.0.clone();
        if let Some(el) = element {
            m.insert(task.loop_var.clone(), el.clone());
            m.insert("ansible_loop_var".into(), json!(task.loop_var));
        }
        list.push(Value::Object(m));
    }
    let mut agg = Map::new();
    agg.insert("results".into(), Value::Array(list));
    agg.insert(
        "changed".into(),
        json!(results.iter().any(|(_, r)| r.changed())),
    );
    if results.iter().any(|(_, r)| r.failed()) {
        agg.insert("failed".into(), json!(true));
    }
    agg.insert(
        "skipped".into(),
        json!(results.iter().all(|(_, r)| r.skipped())),
    );
    agg.insert("msg".into(), json!("All items completed"));
    Value::Object(agg)
}

/// `ansible_failed_task`: the task that failed, as a rescue reads it.
///
/// Measured on ansible-core 2.19.12 with a `command` carrying `register`, `when`, `tags` and
/// `vars`: the dictionary holds one key per task keyword, `action` holding the module's short
/// name, plus six keys of the engine's own (`uuid`, `finalized`, `squashed`, `_resolved_action`,
/// `async_val`, `loop_with`) that describe nothing a playbook wrote. Those six are deliberately
/// not reproduced, and neither are the `_ansible_*` keys the reference adds to `args` on its way
/// to the module.
///
/// The keys come from the keyword table itself rather than from a list written here, so a
/// keyword added to the grammar appears in this dictionary too. Two families are left out
/// because the reference has already resolved them by the time it builds it: `local_action`,
/// which is folded into `action`, and the `with_*` lookups, which are folded into `loop`.
///
/// A keyword this release parks reads `null` here rather than the reference's default. The
/// default would describe something nothing in this engine honours, and a rescue reading
/// `ansible_failed_task.connection` is better told nothing than told `ssh` by an engine that
/// never looked at the keyword.
///
/// `become_method` is not one of those: this release honours it. It reads `null` here anyway
/// because its resolved value lives with the host's variables, not on `PlayTask`, and this
/// function only takes the task - not because nothing looked at the keyword.
fn failed_task_value(task: &PlayTask) -> Value {
    let mut out = Map::new();
    let strings = |list: &[String]| Value::Array(list.iter().map(|s| json!(s)).collect());
    for kw in crate::keywords::TASK_KEYWORDS {
        if kw.name == "local_action" || kw.name.starts_with("with_") {
            continue;
        }
        let value = match kw.name {
            "action" => json!(short_name(&task.module)),
            "args" => Value::Object(task.args.clone()),
            "become" => json!(task.r#become),
            "become_user" => json!(task.become_user),
            "changed_when" => strings(&task.changed_when),
            "failed_when" => strings(&task.failed_when),
            "ignore_errors" => json!(task.ignore_errors),
            "loop" => task.loop_items.clone().unwrap_or(Value::Null),
            "loop_control" => json!({"loop_var": task.loop_var, "label": task.loop_label}),
            "name" => json!(task.name),
            "register" => json!(task.register),
            "tags" => strings(&task.tags),
            "timeout" => json!(task.timeout),
            "vars" => Value::Object(task.vars.clone()),
            "when" => strings(&task.when),
            _ => Value::Null,
        };
        out.insert(kw.name.to_string(), value);
    }
    Value::Object(out)
}

/// Files the handlers one finished task asked for, if it changed anything.
///
/// Measured on ansible-core 2.19.12 and each half worth stating: a task that came back `ok`
/// notifies nothing, a name notified twice runs its handler once, and for a loop it is the
/// **aggregate** that decides - one changed item is a changed task. Notifications live in the
/// driver and nowhere else: they are one host's business, and the flush the coordinator opens is
/// the same flush whether this host has anything to run in it or not.
fn notify(
    compiled: &Compiled,
    task: &PlayTask,
    results: &[(Option<Value>, TaskResult)],
    notified: &mut Vec<usize>,
) {
    if task.notify.is_empty() || !results.iter().any(|(_, r)| r.changed()) {
        return;
    }
    for name in &task.notify {
        // Not deduplicated here, deliberately: what makes a handler run once is the driver
        // taking **every** copy of its index off this list when it runs it, and a guard here
        // that cannot fail because of that would read as if it were what did the work. Proved
        // by deletion - removing this guard reddened nothing, removing that one reddens the
        // order-and-count test.
        notified.extend(crate::compile::resolve_notify(compiled, name));
    }
}

/// What `until`, `retries` and `delay` ask of one task, rendered against its own variables.
///
/// Measured on ansible-core 2.19.12, and every line of it is a measurement: `until` with no
/// `retries` gives three attempts; `retries` with no `until` retries while the result is failed;
/// `retries` under 1 (0 and -1 were both measured) turns the whole machinery off, so the task
/// runs once, prints no retry line, carries no `attempts` and is not failed by a condition that
/// never held; the default `delay` is five seconds; and the sleep happens after **every** failed
/// attempt, the last one included - measured by timing, 2 attempts cost 10 seconds and 3 cost 15.
#[derive(Clone)]
struct Retry {
    /// Attempts in all, never zero.
    attempts: u32,
    delay: Duration,
    /// The conditions that end the loop. Empty means "the result did not fail".
    until: Vec<String>,
}

/// `item` is the task's first one, whose variables `retries` and `delay` are rendered against.
/// It is an `Option` because nothing here promises a task has items; one that has none is
/// `Prepared::Skipped` and never reaches this.
fn retry_plan(
    task: &PlayTask,
    item: Option<&Item>,
    templar: &Templar,
) -> Result<Option<Retry>, TemplateError> {
    let empty = HostVars::default();
    let vars = item.map(|i| &i.vars).unwrap_or(&empty);
    let number = |raw: &Value, keyword: &str| -> Result<f64, TemplateError> {
        let rendered = templar.render_value(raw, vars)?;
        match &rendered {
            Value::Number(n) => n.as_f64().ok_or_else(|| TemplateError(String::new())),
            Value::String(s) => s.trim().parse::<f64>().map_err(|_| TemplateError(String::new())),
            _ => Err(TemplateError(String::new())),
        }
        .map_err(|_| {
            TemplateError(format!(
                "Error processing keyword '{keyword}': The value {rendered} could not be converted to 'int'."
            ))
        })
    };
    let attempts = match &task.retries {
        Some(raw) => {
            let n = number(raw, "retries")?;
            if n < 1.0 {
                return Ok(None);
            }
            n as u32
        }
        None if task.until.is_empty() => return Ok(None),
        None => 3,
    };
    let delay = match &task.delay {
        Some(raw) => Duration::from_secs_f64(number(raw, "delay")?.max(0.0)),
        None => Duration::from_secs(5),
    };
    Ok(Some(Retry {
        attempts,
        delay,
        until: task.until.clone(),
    }))
}

/// Whether the attempt loop stops here: the `until` conditions all hold, or - when the task gave
/// none - the result did not fail.
///
/// Measured on ansible-core 2.19.12, both halves: a task whose `failed_when` makes it fail while
/// its `until` holds stops at once (`attempts: 1`, no retry line), and a task whose
/// `changed_when` makes `until: r.changed` false retries although the module itself succeeded.
/// So the conditions decide the result first and `until` reads what they decided, with the
/// registered name bound to it.
fn until_holds(
    task: &PlayTask,
    item: &Item,
    result: &TaskResult,
    retry: &Retry,
    templar: &Templar,
) -> Result<bool, TemplateError> {
    if retry.until.is_empty() {
        return Ok(!result.failed());
    }
    let mut vars = item.vars.clone();
    if let Some(reg) = &task.register {
        vars.insert(reg.clone(), Value::Object(result.0.clone()));
    }
    vars.insert("result".into(), Value::Object(result.0.clone()));
    all_hold(&retry.until, &vars, templar)
}

/// What an `until` that cannot be evaluated reports.
///
/// Measured on ansible-core 2.19.12: `Task failed: Error while evaluating conditional: object of
/// type 'dict' has no attribute 'nosuchkey'`, with no further attempt and no `attempts`. The
/// prefix is the reference's, which is what a playbook testing `'Task failed' in result.msg`
/// reads; the sentence behind it is this engine's own wording for the same fault.
fn conditional_error(err: &TemplateError) -> String {
    format!("Task failed: Error while evaluating conditional: {}", err.0)
}

/// Waits `delay`, or gives up when the run is interrupted.
async fn sleep_between(
    delay: Duration,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
) -> Option<()> {
    if delay.is_zero() {
        return Some(());
    }
    let deadline = tokio::time::Instant::now() + delay;
    loop {
        if *stop.borrow() {
            return None;
        }
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return Some(()),
            res = stop.changed(), if !*stop_broken => {
                if res.is_err() {
                    *stop_broken = true;
                } else {
                    return None;
                }
            }
        }
    }
}

/// One item of one task, as the agent is asked to run it.
fn protocol_task(task: &PlayTask, item: &Item) -> Task {
    Task {
        module: task.module.clone(),
        args: item.args.clone(),
        // An item is not the task: a failing item never stops the ones behind it, exactly as in
        // Ansible. The task's own failure is decided later, by `report_task`, from every item's
        // result.
        ignore_errors: task.ignores_errors() || task.loop_items.is_some(),
        timeout: task.timeout,
        environment: item.environment.clone(),
    }
}

/// Sends one batch to the agent and collects what comes back, by position in `tasks`.
async fn run_agent_batch(
    link: &mut AgentLink,
    host: &str,
    id: u64,
    tasks: Vec<Task>,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
) -> (Vec<Option<TaskResult>>, Result<BatchOutcome, String>) {
    let mut received: Vec<Option<TaskResult>> = vec![None; tasks.len()];
    if let Err(err) = link.send(&ToAgent::RunBatch { id, tasks }).await {
        return (received, Err(format!("sending batch: {err}")));
    }
    let ended = loop {
        let msg = tokio::select! {
            msg = link.recv() => msg,
            res = stop.changed(), if !*stop_broken => {
                if res.is_err() {
                    *stop_broken = true;
                    continue;
                }
                link.cancel(id, CANCEL_GRACE).await;
                break Ok(BatchOutcome::Cancelled { at: 0 });
            }
        };
        match msg {
            Ok(Some(FromAgent::TaskResult { index, result, .. })) => {
                if let Some(slot) = received.get_mut(index) {
                    *slot = Some(result);
                }
            }
            Ok(Some(FromAgent::BatchDone { outcome, .. })) => break Ok(outcome),
            Ok(Some(FromAgent::Log { message, .. })) => eprintln!("[{host}] {message}"),
            Ok(Some(FromAgent::Ready { .. })) => {}
            Ok(None) => break Err("agent stopped before the batch finished".to_string()),
            Err(err) => break Err(format!("reading from the agent: {err}")),
        }
    };
    (received, ended)
}

fn classify(result: &TaskResult, ignore_errors: bool, rescuable: bool) -> Outcome {
    if result.failed() {
        if ignore_errors {
            Outcome::Ignored
        } else if rescuable {
            Outcome::Rescued
        } else {
            Outcome::Failed
        }
    } else if result.skipped() {
        Outcome::Skipped
    } else if result.changed() {
        Outcome::Changed
    } else {
        Outcome::Ok
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive_host(
    host: Host,
    plan: Arc<PlayPlan>,
    agents: AgentSource,
    options: RunOptions,
    templar: Arc<Templar>,
    store: Arc<Mutex<VarStore>>,
    verbosity: u8,
    existing: Vec<(LinkKey, AgentLink)>,
    forks: Arc<Semaphore>,
    mut progress: watch::Receiver<Progress>,
    tx: mpsc::Sender<Event>,
) {
    let name = host.name.clone();
    let mut stop = options.stop.clone();
    // Set once the stop watch's sender is gone, so a dropped sender is never read as an
    // interrupt and the select below stops polling a branch that would otherwise resolve
    // immediately forever.
    let mut stop_broken = false;
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
    if let Some(grown) = stepped_over(
        &tx,
        &name,
        &plan,
        &mut progress,
        &mut stop,
        &mut stop_broken,
        0..pos,
    )
    .await
    {
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
    // The step a failure was just reported at with the result it failed with, and the `always`
    // section this host is draining on its way out - as the index that section ends at. Those
    // two are the only state a failure adds: the coordinator still knows one thing about a
    // host, that it failed.
    let mut failed_at: Option<(usize, TaskResult)> = None;
    let mut cleanup: Option<usize> = None;

    'run: loop {
        let c = plan.steps();
        let n = c.steps.len();
        if let Some((index, result)) = failed_at.take() {
            // A handler that failed under `force_handlers` stops the rest: measured on
            // ansible-core 2.19.12, `good handler` does not run behind a `bad handler` that
            // failed, with the flag and without it.
            if handlers_only {
                break 'run;
            }
            match rescue_target(&c, index) {
                // A `rescue` takes this failure: the host jumps into it and stays in the play,
                // which is why `failed` was never set for it. The two variables the recovery
                // reads are written on the way in, and they outlive the block and the play -
                // measured, a task after the block and a task in the next play both still read
                // `ansible_failed_task`.
                Some(next) => {
                    let task = &c.steps[index].task;
                    {
                        let mut vars = store.lock().expect("vars lock");
                        vars.set_fact(&name, "ansible_failed_task", failed_task_value(task));
                        vars.set_fact(
                            &name,
                            "ansible_failed_result",
                            Value::Object(result.0.clone()),
                        );
                    }
                    let Some(grown) = stepped_over(
                        &tx,
                        &name,
                        &plan,
                        &mut progress,
                        &mut stop,
                        &mut stop_broken,
                        index + 1..next,
                    )
                    .await
                    else {
                        break 'run;
                    };
                    // Where the drain stands now, for a host that was already draining an
                    // `always` when this failure was raised. A rescue written **inside** that
                    // section leaves the drain where it is - the rest of the cleanup still has
                    // to run - moved by whatever the splices stepped over on the way added in
                    // front of its end. Measured: `rest of the always` runs behind a rescued
                    // block written in a cleanup, and it stops running as soon as an include
                    // between the failure and that rescue grows the list. A rescue **outside**
                    // the section ends the drain: the host is rescued, not leaving, and
                    // `after_pending` asked about a section it has left routes it down the
                    // failed path instead.
                    cleanup = match cleanup {
                        Some(end) if next < end => Some(end + grown),
                        _ => None,
                    };
                    pos = next + grown;
                }
                // Measured on ansible-core 2.19.12: a task failing inside a nested block runs
                // the inner `always`, then the outer one, and only then leaves the play. A
                // `cleanup` already in hand is left where it is: a failure raised while
                // draining one `always` still has to finish leaving through the ones outside.
                None => {
                    failed_index = Some(index);
                    match after_failure(&c, index) {
                        Some((next, end)) => {
                            let Some(grown) = stepped_over(
                                &tx,
                                &name,
                                &plan,
                                &mut progress,
                                &mut stop,
                                &mut stop_broken,
                                index + 1..next,
                            )
                            .await
                            else {
                                break 'run;
                            };
                            pos = next + grown;
                            cleanup = Some(end + grown);
                        }
                        // Nothing left to clean up. Where the host goes from here is the single
                        // test below, so `force_handlers` changes it in one place rather than
                        // two.
                        None => pos = n,
                    }
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
            cleanup = None;
            let Some(next) = advance(
                &tx,
                &name,
                &plan,
                &mut progress,
                &mut stop,
                &mut stop_broken,
                index,
                &mut cleanup,
            )
            .await
            else {
                break 'run;
            };
            pos = next;
            continue 'run;
        }
        if pos >= n || *stop.borrow() {
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
        let mut batch_target: Option<Host> = None;
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
            // A permit kept from the batch before goes back here, in front of every wait this
            // loop can reach: the boundary it stops at, a splice point it has to see grow, and
            // a step whose successor sits behind one, which is where `advance` waits. A permit
            // held across any of them is one the hosts that have to reach that same point
            // cannot have, and with `-f` under the number of live hosts none of them ever
            // would. The batch being empty is what says this is the kept permit rather than the
            // one this batch is working under: no wait is reached with a batch in hand.
            if batch.is_empty()
                && permit.is_some()
                && (is_boundary(task)
                    || crate::compile::is_splice_point(&step.kind)
                    || steps_over_a_splice_point(&c, pos, cleanup))
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
                let _ = tx
                    .send(Event::TaskDone {
                        host: name.clone(),
                        index: pos,
                    })
                    .await;
                let Some(next) = advance(
                    &tx,
                    &name,
                    &plan,
                    &mut progress,
                    &mut stop,
                    &mut stop_broken,
                    pos,
                    &mut cleanup,
                )
                .await
                else {
                    break 'run;
                };
                pos = next;
                continue;
            }
            // A step the hosts of the batch meet in front of: one the table declares a
            // synchronisation point (`run_once`, whose one runner every other host reads), or
            // one whose text names another host's state (`hostvars` and the play's live host
            // lists). Neither subsumes the other - `run_once` spells none of those names, and a
            // `hostvars` read needs no keyword - so a step is a boundary when either says so.
            //
            // Asked of the task rather than read off a table parallel to the step list: the
            // list grows at a flush point, and a second list to keep in step with it is a second
            // thing to get wrong. It costs one pass over the task's own text, against the full
            // render `prepare` does for it a few lines below.
            //
            // In front of the flush, include and mask arms rather than behind them, because the
            // election below has to be unanimous: two hosts reading `live_hosts` at two
            // different moments could otherwise elect two different runners for one step, and
            // both of them would run it.
            if is_boundary(task) && pos > 0 {
                if !batch.is_empty() {
                    break;
                }
                loop {
                    if *stop.borrow() {
                        break 'run;
                    }
                    let p = progress.borrow().clone();
                    // Alone in the play there is nobody left to wait for, which is also how a
                    // wait ends when every other host has died.
                    if p.live_hosts.len() <= 1 || p.completed_through.is_some_and(|c| c + 1 >= pos)
                    {
                        break;
                    }
                    tokio::select! {
                        changed = progress.changed() => {
                            // The coordinator is gone, so no further progress can be published
                            // and waiting on it would never end.
                            if changed.is_err() {
                                break;
                            }
                        }
                        res = stop.changed(), if !stop_broken => {
                            if res.is_err() {
                                stop_broken = true;
                            } else {
                                break 'run;
                            }
                        }
                    }
                }
            }
            // Who runs a `run_once` step: the first live host of the batch **the step's mask
            // includes**, in inventory order. Every other host of the batch reads what it
            // produced and runs nothing. Measured on ansible-core 2.19.12: `changed: [h1]` alone
            // under the banner, the registered variable and the facts readable on h2 as well, and
            // h2 counting no `ok` for it. With `serial` each batch elects its own, which falls
            // out of `live_hosts` being the batch's list and not the play's. Measured again with
            // an include a `when` kept one of three hosts out of: the two the mask holds both
            // read the registered value back, and the third runs only what follows.
            //
            // The mask is part of the election and not a filter after it. Elected off the whole
            // live set, the runner could be a host the mask leaves out - which runs nothing, so
            // nobody runs the step - and every host outside the mask would report the step
            // straight away, publishing the verdict before the runner had run. The hosts waiting
            // on it then read a registered variable that does not exist yet.
            //
            // The unanimity this election needs is what
            // `executor::tests::a_boundary_is_either_declared_by_a_keyword_or_found_in_the_text`
            // guards, over the table rather than over a run: the barrier above is what makes two
            // hosts read one live host list, and without it they can elect two runners and run
            // the task twice.
            let runner = {
                let p = progress.borrow();
                p.live_hosts
                    .iter()
                    .find(|h| {
                        step.hosts
                            .as_ref()
                            .is_none_or(|only| only.iter().any(|m| m == *h))
                    })
                    .cloned()
            };
            let follower =
                task.runs_once() && !handlers_only && runner.as_deref().is_some_and(|h| h != name);
            // Leaving a flush point's handlers behind. This one line is what makes a handler run
            // once per notification: every index the flush was asked for goes, whether or not
            // the flush reached it. Measured on ansible-core 2.19.12, both halves - `first
            // handler` notified twice runs once and does **not** come back at the next flush,
            // and a handler notified by a handler defined **before** it runs in neither flush.
            // A task behind the flush notifying again is what plays a handler a second time.
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
                    let _ = tx
                        .send(Event::Banner {
                            host: name.clone(),
                            index: pos,
                        })
                        .await;
                }
                let _ = tx
                    .send(Event::TaskDone {
                        host: name.clone(),
                        index: pos,
                    })
                    .await;
                // `n` is the length of the list this host is walking, read before the word it
                // just sent could let the coordinator grow it. See `wait_for_splice`.
                if wait_for_splice(
                    &plan,
                    &mut progress,
                    &mut stop,
                    &mut stop_broken,
                    pos,
                    n,
                    &mut cleanup,
                )
                .await
                .is_none()
                {
                    break 'run;
                }
                in_flush = true;
                let Some(next) = advance(
                    &tx,
                    &name,
                    &plan,
                    &mut progress,
                    &mut stop,
                    &mut stop_broken,
                    pos,
                    &mut cleanup,
                )
                .await
                else {
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
                let live = progress.borrow().clone();
                // A host outside the mask, one running for its handlers alone, or one that lost
                // the `run_once` election, asks for nothing and shows nothing. It still reports
                // the step and waits: the steps go in behind this index for everyone's list.
                // Measured on ansible-core 2.19.12, `include_tasks` under `run_once: true`:
                // `included: inc.yml for h1` names the one host, the included tasks run for h1
                // alone, and h2 counts nothing for any of them.
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
                let _ = tx
                    .send(Event::TaskDone {
                        host: name.clone(),
                        index: pos,
                    })
                    .await;
                // Waited for even by a host whose own include just failed: the splice grows every
                // index past this one, and `rescue_target` and `after` are about to be asked for
                // this step on the list the splice leaves behind. A host that skipped the wait
                // would jump to a target computed against the list as it used to be.
                if wait_for_splice(
                    &plan,
                    &mut progress,
                    &mut stop,
                    &mut stop_broken,
                    pos,
                    n,
                    &mut cleanup,
                )
                .await
                .is_none()
                {
                    break 'run;
                }
                if failed_at.is_some() {
                    break;
                }
                let Some(next) = advance(
                    &tx,
                    &name,
                    &plan,
                    &mut progress,
                    &mut stop,
                    &mut stop_broken,
                    pos,
                    &mut cleanup,
                )
                .await
                else {
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
                let _ = tx
                    .send(Event::TaskDone {
                        host: name.clone(),
                        index: pos,
                    })
                    .await;
                let Some(next) = advance(
                    &tx,
                    &name,
                    &plan,
                    &mut progress,
                    &mut stop,
                    &mut stop_broken,
                    pos,
                    &mut cleanup,
                )
                .await
                else {
                    break 'run;
                };
                pos = next;
                continue;
            }
            // A step an include brought in for other hosts. Reported and shown nothing, the way
            // a handler this host never notified is. It sits after the flush and the include arms
            // on purpose: both of those are splice points, and a host outside the mask still has
            // to report them and wait for the coordinator rather than walk past a list that is
            // about to grow.
            //
            // A `run_once` step is the exception and takes the follower arm below instead: the
            // runner is elected inside the mask, so this host is not it, and reporting the step
            // here would publish the verdict for the hosts waiting on a runner that has not run.
            if masked_out && !follower {
                if !batch.is_empty() {
                    break;
                }
                let _ = tx
                    .send(Event::TaskDone {
                        host: name.clone(),
                        index: pos,
                    })
                    .await;
                let Some(next) = advance(
                    &tx,
                    &name,
                    &plan,
                    &mut progress,
                    &mut stop,
                    &mut stop_broken,
                    pos,
                    &mut cleanup,
                )
                .await
                else {
                    break 'run;
                };
                pos = next;
                continue;
            }
            // A host that lost the `run_once` election. It runs nothing and shows nothing, and
            // it waits here until the coordinator publishes what the one runner made of the
            // step: what that host registered or set as a fact is written for every host of the
            // batch, and a host reading it one moment too early would read nothing.
            //
            // The include and flush arms above are not reached through here on purpose: a
            // follower still reports those and still waits for the splice, it just asks for
            // nothing. This arm is for every other step, a host the step's mask leaves out
            // included: it has nothing to run either, and nothing to report until the runner has
            // spoken.
            if follower {
                if !batch.is_empty() {
                    break;
                }
                let runner = runner.clone().unwrap_or_default();
                let Some(verdict) =
                    wait_for_run_once(&mut progress, &mut stop, &mut stop_broken, &runner, pos)
                        .await
                else {
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
                let _ = tx
                    .send(Event::TaskDone {
                        host: name.clone(),
                        index: pos,
                    })
                    .await;
                let Some(next) = advance(
                    &tx,
                    &name,
                    &plan,
                    &mut progress,
                    &mut stop,
                    &mut stop_broken,
                    pos,
                    &mut cleanup,
                )
                .await
                else {
                    break 'run;
                };
                pos = next;
                continue;
            }
            let live = progress.borrow().clone();
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
                            vars.set_fact(target, reg, value.clone());
                        }
                    }
                    let Some(next) = advance(
                        &tx,
                        &name,
                        &plan,
                        &mut progress,
                        &mut stop,
                        &mut stop_broken,
                        pos,
                        &mut cleanup,
                    )
                    .await
                    else {
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
                    let _ = tx
                        .send(Event::Banner {
                            host: name.clone(),
                            index: pos,
                        })
                        .await;
                    let _ = tx
                        .send(Event::TaskDone {
                            host: name.clone(),
                            index: pos,
                        })
                        .await;
                    let Some(next) = advance(
                        &tx,
                        &name,
                        &plan,
                        &mut progress,
                        &mut stop,
                        &mut stop_broken,
                        pos,
                        &mut cleanup,
                    )
                    .await
                    else {
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
                        let r = match &item.skipped {
                            Some(s) => s.clone(),
                            None => {
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
                                    if sleep_between(retry.delay, &mut stop, &mut stop_broken)
                                        .await
                                        .is_none()
                                    {
                                        break 'run;
                                    }
                                    if last {
                                        break r;
                                    }
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
                            vars.set_fact(target, reg, value.clone());
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
                    let Some(next) = advance(
                        &tx,
                        &name,
                        &plan,
                        &mut progress,
                        &mut stop,
                        &mut stop_broken,
                        pos,
                        &mut cleanup,
                    )
                    .await
                    else {
                        break 'run;
                    };
                    pos = next;
                }
                Ok(Prepared::Remote(items, escalation, delegate)) => {
                    // A different target user is a different agent on the host, and a different
                    // delegate is a different host entirely, so either one ends the batch and
                    // the next one opens its own link. `pos` does not move, so this task is the
                    // first of that batch.
                    let delegate_name = delegate.as_ref().map(|d| d.name.clone());
                    if !batch.is_empty()
                        && (escalation != batch_escalation || delegate_name != batch_delegate)
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
                    batch_target = delegate;
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
                    // Where this host goes after a step a `rescue` would catch depends on how
                    // that step ends, so the batch stops here and `pos` waits for the result.
                    // Moving it now would step over that very rescue and tell the coordinator
                    // so - the host would then enter a section already reported as passed, and
                    // its `fatal:` line would print under the banner of a later task.
                    //
                    // A flush point between here and where this step leads ends the batch for
                    // its own reason: `advance` would stop at it and wait for the splice while
                    // this step has no `TaskDone` behind it, and the coordinator cannot reach
                    // that flush until it has one. Both sides would then wait for the other.
                    // Every other blocking wait in this loop is already reached with the batch
                    // empty; this one has to be too.
                    if rescue_target(&c, pos).is_some()
                        || steps_over_a_splice_point(&c, pos, cleanup)
                    {
                        undecided = Some(pos);
                        break;
                    }
                    let Some(next) = advance(
                        &tx,
                        &name,
                        &plan,
                        &mut progress,
                        &mut stop,
                        &mut stop_broken,
                        pos,
                        &mut cleanup,
                    )
                    .await
                    else {
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
                match Arc::clone(&forks).acquire_owned().await {
                    Ok(p) => permit = Some(p),
                    // Nothing in this run closes the semaphore, so this is a bug rather than
                    // a shutdown. Reporting it beats returning as if the host had run.
                    Err(_) => {
                        unreachable = Some("the run's fork limit is gone".to_string());
                        break 'run;
                    }
                }
            }
            // The delegate's own connection, never the delegating host's. Measured on
            // ansible-core 2.19.12: a task delegated away from a host that answers nothing runs
            // perfectly well, so the delegating host's link is not opened for it at all.
            let target = batch_target.as_ref().unwrap_or(&host);
            let key = LinkKey {
                host: target.name.clone(),
                become_user: batch_escalation.as_ref().map(|e| e.user.clone()),
            };
            let link = match reuse_or_connect(
                &mut links,
                &mut checked,
                &key,
                batch_escalation.as_ref(),
                target,
                &agents,
                &options,
            )
            .await
            {
                Ok(l) => l,
                // The host answered and then refused to escalate, so this is the task failing
                // and not the host going away. `ignore_errors` is deliberately not honoured:
                // the batch never ran, and a run that reported success while having quietly
                // skipped every escalated task is the worst outcome available here.
                // Measured on ansible-core 2.19.12: a `become_user` the host refuses is a task
                // failure like any other, so a rescue around it takes it (`rescued=1`, exit 0).
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
                            &mut stop,
                            &mut stop_broken,
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
                        if sleep_between(retry.delay, &mut stop, &mut stop_broken)
                            .await
                            .is_none()
                        {
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
                let (flat, ended) =
                    run_agent_batch(link, &name, batch_id, tasks, &mut stop, &mut stop_broken)
                        .await;
                for (k, result) in flat.into_iter().enumerate() {
                    if let Some(&(bi, ii)) = origin.get(k) {
                        received[bi][ii] = result;
                    }
                }
                ended
            };
            // Report every task of the batch in order; tasks the agent never reached (after a
            // failure) are not reported at all, as in Ansible.
            //
            // `undecided`, when set, is always this loop's last entry - the step whose own
            // batch-ending push is the one in the `Prepared::Remote` arm above. Whether it was
            // actually reported is tracked rather than assumed: the agent can end the batch
            // `Ok` without a result for it (a bug elsewhere, or a connection hiccup the batch
            // outcome does not carry), and advancing past a step with no `TaskDone` behind it
            // is the exact barrier stall this task exists to avoid.
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
                    let live = progress.borrow().live_hosts.clone();
                    let mut vars = store.lock().expect("vars lock");
                    let value = registered_value(task, &results);
                    for target in fact_targets(task, &name, &live) {
                        vars.set_fact(&target, reg, value.clone());
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
            // The results are in and reported, so the next host may start while this one
            // renders its remaining local tasks.
            //
            // The escalated links go with the permit, which is what makes `forks` bound the
            // connections open as well as the hosts working. It bounded only the latter, and a
            // driver keeps its links from its first batch to the end of the play, so a wide
            // play held one link per host per target user however narrow `-f` was. Measured on
            // the development machine: three controller descriptors per link, so the usual
            // `ulimit -n 1024` runs out past roughly 338 links, and 20 hosts escalating to root
            // under `ulimit -n 128` and `-f 5` reported `UNREACHABLE! ... starting ssh: Too
            // many open files` for a host that was perfectly reachable. The link to the host
            // itself stays - that is the expensive one, and the one persistence is for - and
            // the escalated agent is already cached for its user, so reopening it is one probe
            // and one `ssh`.
            //
            // Closed off-task rather than awaited here: `shutdown` gives its own agent up to
            // two seconds, and a driver paying that between batches would serialise exactly
            // what releasing the permit just freed.
            //
            // Both stay with this driver while the next step needs nobody else. A `register` or
            // a `changed_when` ends a batch for a reason internal to this host, and handing the
            // escalated agent back at every one of them cost one `sudo` and one probe per
            // registered task - measured, six escalations for a play of six. They go back
            // before every wait on the other hosts, because a permit held across one is a
            // permit the hosts that have to reach that same point cannot have, and with `-f`
            // under the number of live hosts none of them ever would. Three kinds of wait:
            // the boundary the step loop stops at, the splice the next `advance` waits for, and
            // the step held back above, whose own `advance` runs below.
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
                let Some(next) = advance(
                    &tx,
                    &name,
                    &plan,
                    &mut progress,
                    &mut stop,
                    &mut stop_broken,
                    step,
                    &mut cleanup,
                )
                .await
                else {
                    break 'run;
                };
                pos = next;
            }
        }

        if failed_at.is_none()
            && let Some((index, err)) = deferred_error
        {
            let task = &c.steps[index].task;
            // The reference's own prefix on the `msg` of a task that dies before it runs -
            // a `when` it cannot evaluate, arguments it cannot render. Measured: a failing
            // `when` reports `Task failed: Error while evaluating conditional: ...`, and a
            // playbook of the operator's testing `'Task failed' in result.msg` must still see
            // it here. `failed_when` is the one that does not get it: there the reference
            // leaves `msg` empty and puts the error in `failed_when_result`.
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
                let Some(next) = advance(
                    &tx,
                    &name,
                    &plan,
                    &mut progress,
                    &mut stop,
                    &mut stop_broken,
                    index,
                    &mut cleanup,
                )
                .await
                else {
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
    // A link this driver opened to somebody else's host - a `delegate_to` - goes no further
    // than this play. What connection persistence is for is the link to the host a driver owns,
    // and the run keeps exactly one per host: handing back a second link to a host whose own
    // driver also handed one back would leave two entries racing for the same key, and the one
    // that won would be a connection opened with another host's fork permit. The escalated
    // delegated links are already closed with each batch's permit; these are the rest.
    //
    // Awaited rather than spawned and forgotten: at the end of the last play the runtime can be
    // dropped before a detached task ever runs, which would leave the agent on the delegate
    // waiting on a connection nobody closes. `keep_links` awaits its own `JoinSet` for the same
    // reason, and this is the same shape.
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

/// The connection for the next batch, under `key`'s target user: the one kept from an earlier
/// play if its agent still answers, a fresh one otherwise. A kept connection gets exactly one
/// liveness check per play, and a failed check exactly one reconnection; a failed reconnection
/// is the host's `UNREACHABLE`.
#[allow(clippy::too_many_arguments)]
async fn reuse_or_connect<'a>(
    links: &'a mut HashMap<LinkKey, AgentLink>,
    checked: &mut HashSet<LinkKey>,
    key: &LinkKey,
    escalation: Option<&Escalation>,
    host: &Host,
    agents: &AgentSource,
    options: &RunOptions,
) -> Result<&'a mut AgentLink, ConnectError> {
    // Why the kept connection was not reused, kept only in case the reconnection below fails
    // too: on its own it is not a failure (a single reconnection is the designed recovery), but
    // discarding it silently would leave a reconnect failure reporting only its own cause.
    let mut stale: Option<String> = None;
    if checked.insert(key.clone())
        && let Some(mut link) = links.remove(key)
    {
        let alive = tokio::time::timeout(options.defaults.connect_timeout, link.handshake()).await;
        match alive {
            Ok(Ok(())) => {
                links.insert(key.clone(), link);
            }
            Ok(Err(err)) => {
                stale = Some(format!("{err:#}"));
                link.shutdown().await;
            }
            Err(_) => {
                stale = Some("no answer from the kept connection".to_string());
                link.shutdown().await;
            }
        }
    }
    if !links.contains_key(key) {
        let link = match connect(host, agents, &options.defaults, escalation).await {
            Ok(link) => link,
            Err(ConnectError::Unreachable(msg)) => {
                return Err(ConnectError::Unreachable(match stale {
                    Some(reason) => format!(
                        "the kept connection failed ({reason}), and reconnecting failed too: {msg}"
                    ),
                    None => msg,
                }));
            }
            // A refused escalation is the host's answer about this user, not a connection that
            // failed, so it travels on untouched: adding the stale note would turn a failed
            // task into something that reads like an unreachable host.
            Err(err) => return Err(err),
        };
        links.insert(key.clone(), link);
    }
    Ok(links.get_mut(key).expect("connected just above"))
}

/// The hosts a step's registered variable and facts are written for: every live host of the
/// batch when one of them runs the step for all of them, and the step's own host otherwise.
///
/// Measured on ansible-core 2.19.12: a `run_once` task's `register` and its `set_fact` are both
/// readable on every host of the batch, not only on the one that ran it.
fn fact_targets(task: &PlayTask, host: &str, live: &[String]) -> Vec<String> {
    if task.runs_once() && !live.is_empty() {
        live.to_vec()
    } else {
        vec![host.to_string()]
    }
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

/// Waits for the one runner of the `run_once` step at `pos` to say what it made of it.
///
/// `None` when the run was interrupted, the way every other wait in this file answers it.
///
/// The verdict is read before the live host list, and that order is the whole of it: the
/// coordinator publishes a failed result and the shrunken list in the same `Progress`, so a
/// host that asked "is the runner still live?" first would see a runner that is merely gone and
/// leave at exit 4 where the reference exits 2.
async fn wait_for_run_once(
    progress: &mut watch::Receiver<Progress>,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    runner: &str,
    pos: usize,
) -> Option<RunOnce> {
    loop {
        if *stop.borrow() {
            return None;
        }
        let p = progress.borrow().clone();
        match p.run_once.get(&pos) {
            Some(true) => return Some(RunOnce::Failed),
            Some(false) => return Some(RunOnce::Done),
            None => {}
        }
        // The host this step was handed to has left the batch without a verdict, which is what
        // an unreachable host does. Asked of that one host and not of the list's length: with
        // three hosts waiting on a runner that died, a test for "anybody else still live" would
        // be true for ever and this wait would never end.
        if !p.live_hosts.iter().any(|h| h == runner) {
            return Some(RunOnce::Gone);
        }
        tokio::select! {
            changed = progress.changed() => {
                // The coordinator is gone, so no verdict can ever be published.
                if changed.is_err() {
                    return Some(RunOnce::Gone);
                }
            }
            res = stop.changed(), if !*stop_broken => {
                if res.is_err() {
                    *stop_broken = true;
                } else {
                    return None;
                }
            }
        }
    }
}

/// Whether `advance` from `pos` would walk past a splice point - a flush or an include - and so
/// block waiting for the splice.
///
/// A driver may only wait for a splice once it owes the coordinator nothing in front of it: the
/// coordinator walks the step list in order and holds at each index until every host is done
/// with it or gone, so a host waiting at a splice point while a step behind it is still unreported
/// waits for an index the coordinator can never reach. The batch collection loop asks this before
/// moving on from a step it has queued and not yet run.
fn steps_over_a_splice_point(compiled: &Compiled, pos: usize, cleanup: Option<usize>) -> bool {
    let next = match cleanup {
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
#[allow(clippy::too_many_arguments)]
async fn advance(
    tx: &mpsc::Sender<Event>,
    host: &str,
    plan: &PlayPlan,
    progress: &mut watch::Receiver<Progress>,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    pos: usize,
    cleanup: &mut Option<usize>,
) -> Option<usize> {
    let compiled = plan.steps();
    let next = match *cleanup {
        Some(end) => after_pending(&compiled, pos, end).map(|(next, end)| {
            *cleanup = Some(end);
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
    let grown = stepped_over(tx, host, plan, progress, stop, stop_broken, pos + 1..next).await?;
    if let Some(end) = cleanup.as_mut() {
        *end += grown;
    }
    Some(next + grown)
}

/// Tells the coordinator about every step of `range` this host stepped over, stopping at each
/// splice point on the way, and answers how much longer the list is for it.
///
/// A host has to stop at a splice point it steps over as surely as at one it runs. The
/// coordinator inserts steps behind a flush or an include once every host has reported that
/// index, and a host that read the list again before that would be holding an index into a list
/// that has changed underneath it - the exact shape this file's barrier invariant exists to
/// prevent. Measured on ansible-core 2.19.12: a `meta: flush_handlers` written inside a `rescue:`
/// nobody entered runs nothing and shows nothing, and so does an `include_tasks` written there,
/// which is what puts a live host in this position.
async fn stepped_over(
    tx: &mpsc::Sender<Event>,
    host: &str,
    plan: &PlayPlan,
    progress: &mut watch::Receiver<Progress>,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    range: std::ops::Range<usize>,
) -> Option<usize> {
    let mut grown = 0;
    let mut from = range.start;
    loop {
        let compiled = plan.steps();
        let end = range.end + grown;
        let splice = (from..end.min(compiled.steps.len()))
            .find(|&i| crate::compile::is_splice_point(&compiled.steps[i].kind));
        let Some(at) = splice else {
            skipped(tx, host, from..end).await;
            return Some(grown);
        };
        skipped(tx, host, from..at + 1).await;
        let before = compiled.steps.len();
        drop(compiled);
        wait_for_splice(plan, progress, stop, stop_broken, at, before, &mut None).await?;
        grown += plan.steps().steps.len() - before;
        // Back at the splice point's successor, which is now the first of the steps it just grew
        // by. This host steps over those too - they sit in the section the flush or the include
        // sat in, and it is not in that section - but it still owes the coordinator a word about
        // each of them, or the barrier behind them opens on a host that never said it had passed
        // them.
        from = at + 1;
    }
}

/// Waits until the coordinator has put the handler steps in behind the flush point at `at`.
///
/// This is the whole of the plan's one dynamic primitive on the driver's side: between reporting
/// a flush point and this returning, the host reads nothing from the step list, so the index it
/// holds cannot be invalidated by the splice. `cleanup`, the end of the `always` section a host
/// may be draining, is the one index it carries that sits past `at`, so it moves with the list.
///
/// `before` is how long the list was when the host still owed `at` a `TaskDone`, and the caller
/// has to read it before sending that word. Reading it here would be a race: the coordinator
/// publishes the splice as soon as the last host reports the index, so the list can already have
/// grown by the time this function runs, and the growth would then measure as nothing at all -
/// leaving `cleanup` where it was and ending the host's drain one step early. Rare, load
/// dependent, and it reports success while a cleanup step nobody ran goes missing.
///
/// `None` when the run was interrupted or the coordinator is gone.
async fn wait_for_splice(
    plan: &PlayPlan,
    progress: &mut watch::Receiver<Progress>,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    at: usize,
    before: usize,
    cleanup: &mut Option<usize>,
) -> Option<()> {
    loop {
        if *stop.borrow() {
            return None;
        }
        if progress.borrow().spliced_through.is_some_and(|s| s >= at) {
            break;
        }
        tokio::select! {
            changed = progress.changed() => {
                // The coordinator is gone, so no splice can be published and waiting on one
                // would never end. Whatever the host has left to do, it does on the list it has.
                if changed.is_err() {
                    return Some(());
                }
            }
            res = stop.changed(), if !*stop_broken => {
                if res.is_err() {
                    *stop_broken = true;
                } else {
                    return None;
                }
            }
        }
    }
    if let Some(end) = cleanup.as_mut() {
        *end += plan.steps().steps.len() - before;
    }
    Some(())
}

/// Tells the coordinator about every step in `range` that this host stepped over, so the shared
/// progress it publishes counts this host as having reached them.
///
/// It is what keeps a host that steps over a section from waiting on itself: the progress every
/// barrier reads is the lowest step any live host has finished, so a host that jumped from 3 to
/// 9 without saying so would hold that figure at 3 while waiting for it to reach 8. Both halves
/// of that are live now: a host that failed steps over the rest of the body on its way to a
/// cleanup, and a host that failed nothing steps over every `rescue` it walks past - the second
/// with the whole play still waiting on it.
async fn skipped(tx: &mpsc::Sender<Event>, host: &str, range: std::ops::Range<usize>) {
    for index in range {
        let _ = tx
            .send(Event::TaskDone {
                host: host.to_string(),
                index,
            })
            .await;
    }
}

/// What one host asks for at an include step, and the lines it has to show for the items that
/// asked for nothing.
///
/// The whole of the resolution happens here, on the driver, and every way it can go wrong comes
/// back as a `TaskResult` rather than as an error: a file that is not there, a file that is not a
/// list of tasks, a role nobody can find and a statement nested past the ceiling all fail **this
/// host's** task, which is what lets a `rescue` around the statement take them - measured on
/// ansible-core 2.19.12, an `include_tasks` in a block's body whose file is missing is rescued
/// like any other failure - and what lets the other hosts carry on.
#[allow(clippy::too_many_arguments)]
fn resolve_include(
    compiled: &Compiled,
    step: &Step,
    kind: IncludeKind,
    host: &str,
    plan: &PlayPlan,
    live: &Progress,
    templar: &Templar,
    store: &Mutex<VarStore>,
    defaults: &ConnectionDefaults,
) -> (Vec<IncludeGroup>, Vec<Shown>) {
    let items = match prepare(step, host, plan, live, templar, store, defaults) {
        Ok(
            Prepared::Skipped(items) | Prepared::Local(items, _) | Prepared::Remote(items, _, _),
        ) => items,
        // A `when` that cannot be evaluated or a `loop` that is not a list, reported with the
        // reference's own prefix for a task that dies before it runs. Not deferred the way an
        // ordinary task's is: this arm is reached with the batch empty, so there is nothing to
        // send out first.
        Err(err) => {
            return (
                Vec::new(),
                vec![Shown {
                    element: None,
                    label: None,
                    result: TaskResult::failed_with(format!("Task failed: {}", err.0)),
                    failed: true,
                }],
            );
        }
    };
    let mut groups = Vec::new();
    let mut shown = Vec::new();
    for item in items {
        if let Some(skipped) = &item.skipped {
            shown.push(Shown {
                element: item.element.clone(),
                label: item.label.clone(),
                result: skipped.clone(),
                failed: false,
            });
            continue;
        }
        let result = match include_request(compiled, step, kind, &item, templar) {
            // The file is there and cannot be used: it holds a mapping rather than a list of
            // tasks, its YAML does not parse, or something inside it names an option this
            // release refuses. Measured on ansible-core 2.19.12 for the first of those:
            // `fatal: [h1]: FAILED! => {"changed": false, "include": "mapping.yml", "reason":
            // "included task files must contain a list of tasks"}`, exit 2, with a recap.
            Ok(request) => match crate::compile::expand_include(compiled, step, &request) {
                Ok(expanded) => {
                    groups.push(IncludeGroup {
                        key: request.key(),
                        what: request.what,
                        label: request.label,
                        params: request.vars,
                        expanded,
                    });
                    continue;
                }
                Err(err) => {
                    let mut body = Map::new();
                    body.insert("changed".into(), json!(false));
                    body.insert("include".into(), json!(request.what));
                    body.insert("reason".into(), json!(format!("{err:#}")));
                    TaskResult(body)
                }
            },
            Err(result) => result,
        };
        shown.push(Shown {
            element: item.element,
            label: item.label,
            result,
            failed: true,
        });
    }
    (groups, shown)
}

/// One item of an include statement that has a line to show: a `when` left it out, or resolving
/// what it named went wrong.
struct Shown {
    element: Option<Value>,
    label: Option<String>,
    result: TaskResult,
    /// Whether this is a failure rather than a skip. Carried here rather than read off the
    /// result's own `failed` key, because the reference's `fatal:` line for an include is
    /// `{"changed": false, "include": "nosuch.yml", "reason": "..."}` - measured, with no
    /// `failed` in it - and a key put there to be classified by would be a key on the line.
    failed: bool,
}

/// One item's request, or the result that item fails with.
fn include_request(
    compiled: &Compiled,
    step: &Step,
    kind: IncludeKind,
    item: &Item,
    templar: &Templar,
) -> Result<IncludeRequest, TaskResult> {
    let task = &step.task;
    // A file that includes itself, or a ring of them, otherwise grows the step list for as long
    // as the run has memory. The reference has no ceiling here at all: measured, `a.yml`
    // including itself runs until Python's stack is gone and exits 250 with a traceback, four
    // thousand lines in. Refused instead, at the depth the compiler's own recursions share.
    if step.origin.depth as usize + 1 > crate::compile::DEPTH {
        return Err(TaskResult::failed_with(format!(
            "includes nest deeper than {} levels: a file or a role that includes itself",
            crate::compile::DEPTH
        )));
    }
    // Only the statement's own `vars:` and the loop variable travel down. Measured on
    // ansible-core 2.19.12: an included task reads the statement's `vars:`, and everything else
    // written on the statement stops there - `tags` do not descend, `when` is evaluated for the
    // statement alone, `no_log` censors the statement's own line and not the tasks behind it, and
    // `become` and `environment` are refused on a statement at load time.
    // Seeded with what the statement above this one handed down, so a chain of includes carries
    // the whole chain's values rather than only the last link's.
    let mut vars = step
        .include_params
        .as_deref()
        .cloned()
        .unwrap_or_else(Map::new);
    // Rendered from what the statement wrote, not read back out of the host's merged scope: the
    // scope resolves a name the host also carries a fact for to the **fact**, and the whole point
    // of the measurement above is that an include's own value wins there.
    for (key, raw) in &task.vars {
        match templar.render_value(raw, &item.vars) {
            Ok(value) => {
                vars.insert(key.clone(), value);
            }
            Err(err) => {
                return Err(TaskResult::failed_with(format!("Task failed: {}", err.0)));
            }
        }
    }
    if item.element.is_some() {
        if let Some(value) = item.vars.get(&task.loop_var) {
            vars.insert(task.loop_var.clone(), value.clone());
        }
        vars.insert(
            "ansible_loop_var".into(),
            Value::String(task.loop_var.clone()),
        );
    }
    let (target, what) = match kind {
        IncludeKind::Tasks => {
            let name = item
                .args
                .get("file")
                .or_else(|| item.args.get("_raw_params"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    TaskResult::failed_with("Task failed: 'include_tasks' takes a file name")
                })?;
            let path = crate::compile::beside_or_in_role(
                &step.origin.file_dir,
                step.origin.role_dir.as_deref(),
                name,
            );
            if !path.is_file() {
                // The reference's own three sentences, measured word for word, with the file
                // named as the playbook wrote it and the path it looked in spelled out. The
                // `errno` tail is Python's; this engine reaches the same conclusion by asking the
                // filesystem, so the sentence is kept and the number with it.
                let mut body = Map::new();
                body.insert("changed".into(), json!(false));
                body.insert("include".into(), json!(name));
                body.insert(
                    "reason".into(),
                    json!(format!(
                        "Could not find or access '{p}' on the Ansible Controller: Unable to retrieve file contents.\nCould not find or access '{p}' on the Ansible Controller.\nIf you are using a module and expect the file to exist on the remote, see the remote_src option: [Errno 2] No such file or directory: '{p}'",
                        p = path.display()
                    )),
                );
                return Err(TaskResult(body));
            }
            let what = path.display().to_string();
            (IncludeTarget::File(path), what)
        }
        IncludeKind::Role => {
            let entry = crate::compile::include_role_entry(&item.args)
                .map_err(|err| TaskResult::failed_with(format!("Task failed: {err:#}")))?;
            // Measured on ansible-core 2.19.12: a role an `include_role` cannot find is an
            // ordinary task failure, `fatal: [h1]: FAILED! => {"changed": false, "reason": "the
            // role 'nosuchrole' was not found in <paths>"}`, exit 2 and a recap - and not the
            // exit 1 before the first banner a `roles:` entry gets, because by then the play is
            // already running.
            compiled.search.locate(&entry.name).map_err(|err| {
                let mut body = Map::new();
                body.insert("changed".into(), json!(false));
                body.insert("reason".into(), json!(format!("{err:#}")));
                TaskResult(body)
            })?;
            let what = entry.name.clone();
            (IncludeTarget::Role(Box::new(entry)), what)
        }
    };
    Ok(IncludeRequest {
        target,
        what,
        vars,
        label: item.label.clone(),
    })
}

/// Sends the lines an include step shows and its `TaskDone`, and answers with the result a rescue
/// would be given when it failed for good.
///
/// It is not [`report_task`] because of what an include **does not** count. Measured on
/// ansible-core 2.19.12: a statement that brought something in counts one `ok` per host per item,
/// and that one is counted by the coordinator when it prints the `included:` line - so the
/// aggregate a looping statement would otherwise contribute has to count nothing, or a two-item
/// loop would read three. The two aggregates that do count are the ones with no `included:` line
/// behind them: a loop over an empty list, which shows `skipping:` and counts it, and a loop an
/// item failed in, which counts the failure without a line of its own.
async fn report_include(
    tx: &mpsc::Sender<Event>,
    host: &str,
    index: usize,
    task: &PlayTask,
    shown: &[Shown],
    rescuable: bool,
    nothing_asked: bool,
) -> Option<TaskResult> {
    let is_loop = task.loop_items.is_some();
    let censored = task.censors();
    let ignored = task.ignores_errors();
    let mut any_failed = false;
    let mut failure: Option<TaskResult> = None;
    for item in shown {
        let outcome = match (item.failed, ignored) {
            (false, _) => Outcome::Skipped,
            (true, true) => Outcome::Ignored,
            (true, false) if rescuable => Outcome::Rescued,
            (true, false) => Outcome::Failed,
        };
        if item.failed {
            if failure.is_none() {
                failure = Some(item.result.clone());
            }
            any_failed = true;
        }
        let _ = tx
            .send(Event::Result {
                host: host.to_string(),
                index,
                label: item.label.clone(),
                outcome,
                result: item.result.clone(),
                dump: false,
                show: true,
                counts: !is_loop,
                censored,
                delegate: None,
            })
            .await;
    }
    if is_loop {
        let empty = shown.is_empty() && nothing_asked;
        if empty || any_failed {
            let results: Vec<(Option<Value>, TaskResult)> = shown
                .iter()
                .map(|s| (s.element.clone(), s.result.clone()))
                .collect();
            let aggregate = if empty {
                empty_loop_result()
            } else {
                match registered_value(task, &results) {
                    Value::Object(mut m) => {
                        m.remove("results");
                        TaskResult(m)
                    }
                    _ => TaskResult::default(),
                }
            };
            let outcome = classify(&aggregate, task.ignores_errors(), rescuable);
            if any_failed {
                failure = Some(aggregate.clone());
            }
            let _ = tx
                .send(Event::Result {
                    host: host.to_string(),
                    index,
                    label: None,
                    outcome,
                    result: aggregate,
                    dump: false,
                    show: empty,
                    counts: true,
                    censored,
                    delegate: None,
                })
                .await;
        }
    }
    if any_failed && !task.ignores_errors() {
        failure.or_else(|| Some(TaskResult::default()))
    } else {
        None
    }
}

/// Sends the result lines of one task and its `TaskDone`. Returns the result a rescue would be
/// given as `ansible_failed_result` when the task failed for good, and `None` when it did not:
/// for a loop that is the aggregate, `results` and all, as the reference hands it over.
///
/// `labels` is parallel to `results`: each item's display label, honouring a custom
/// `loop_control.label` template where the playbook gave one. `rescuable` says whether a
/// `rescue` around this step takes a failure here, which is what tells a `fatal:` line that
/// counts `failed` from one that counts `rescued`.
///
/// `retries` is parallel to `results` too: the `retries left` counts of the attempts that item
/// needed, in order. They are sent from here rather than as they happen because the reference
/// prints each item's retry lines directly in front of that item's own result line - measured
/// with a loop whose first item passed and whose second needed two attempts.
///
/// `names` is parallel to `results` as well: each item's task name, already templated. The
/// `FAILED - RETRYING` line shows it rather than the raw `task.name` - measured, a templated name
/// like `probe {{ n }}` renders there the way it does everywhere else.
#[allow(clippy::too_many_arguments)]
async fn report_task(
    tx: &mpsc::Sender<Event>,
    host: &str,
    index: usize,
    task: &PlayTask,
    results: &[(Option<Value>, TaskResult)],
    labels: &[Option<String>],
    retries: &[Vec<u32>],
    names: &[String],
    dump: bool,
    rescuable: bool,
    delegate: Option<&str>,
) -> Option<TaskResult> {
    let is_loop = task.loop_items.is_some();
    let censored = task.censors();
    let mut any_failed = false;
    let mut first_failure: Option<TaskResult> = None;
    for (i, (_element, r)) in results.iter().enumerate() {
        for left in retries.get(i).into_iter().flatten() {
            let _ = tx
                .send(Event::Retrying {
                    host: host.to_string(),
                    index,
                    name: names.get(i).cloned().unwrap_or_else(|| task.name.clone()),
                    left: *left,
                })
                .await;
        }
        let outcome = classify(r, task.ignores_errors(), rescuable);
        if r.failed() && first_failure.is_none() {
            first_failure = Some(r.clone());
        }
        any_failed |= r.failed();
        let label = labels.get(i).cloned().flatten();
        let _ = tx
            .send(Event::Result {
                host: host.to_string(),
                index,
                label,
                outcome,
                result: r.clone(),
                dump,
                show: true,
                counts: !is_loop,
                censored,
                delegate: delegate.map(str::to_string),
            })
            .await;
    }
    let mut failure = first_failure;
    if is_loop {
        // A loop over an empty list has no item lines to carry it, so the aggregate is the whole
        // display: one `skipping` line for the task.
        let (aggregate, show) = if results.is_empty() {
            (empty_loop_result(), true)
        } else {
            let aggregate = match registered_value(task, results) {
                Value::Object(mut m) => {
                    m.remove("results");
                    TaskResult(m)
                }
                _ => TaskResult::default(),
            };
            // The reference prints no aggregate line for a loop that had items: the failing
            // item's own line carries the message, and `...ignoring` follows the items alone.
            (aggregate, false)
        };
        let outcome = classify(&aggregate, task.ignores_errors(), rescuable);
        // Measured on ansible-core 2.19.12: a rescue reading `ansible_failed_result.results`
        // after a loop whose second item failed sees all three items. The aggregate is the
        // whole task's result, which is what the reference hands over - an item's own result
        // would lose the ones behind it.
        if any_failed {
            failure = Some(aggregate.clone());
        }
        let _ = tx
            .send(Event::Result {
                host: host.to_string(),
                index,
                label: None,
                outcome,
                result: aggregate,
                dump: false,
                show,
                counts: true,
                censored,
                delegate: delegate.map(str::to_string),
            })
            .await;
    }
    let _ = tx
        .send(Event::TaskDone {
            host: host.to_string(),
            index,
        })
        .await;
    if any_failed && !task.ignores_errors() {
        failure.or_else(|| Some(TaskResult::default()))
    } else {
        None
    }
}

/// Opens one link, escalated when `escalation` is given. Every way this can fail is a host the
/// run cannot reach and comes back as `ConnectError::Unreachable`, except a `sudo` that refused,
/// which comes back as `ConnectError::Become` because the host itself answered.
async fn connect(
    host: &Host,
    agents: &AgentSource,
    defaults: &ConnectionDefaults,
    escalation: Option<&Escalation>,
) -> Result<AgentLink, ConnectError> {
    let transport = Transport::for_host(host, defaults)
        .map_err(|e| ConnectError::Unreachable(format!("{e:#}")))?;
    let mut link = transport.connect(agents, escalation).await?;
    let timeout = defaults.connect_timeout;
    tokio::time::timeout(timeout, link.handshake())
        .await
        .map_err(|_| {
            ConnectError::Unreachable(format!(
                "no answer from the agent after {} seconds",
                timeout.as_secs()
            ))
        })?
        .map_err(|e| ConnectError::Unreachable(format!("{e:#}")))?;
    Ok(link)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each shape below was run through `ansible-core 2.19.12` as a `debug: msg=` argument
    /// first, and the expectations are what it printed: a key nested three levels down goes,
    /// a bare list element goes too, and every emptied container stays.
    #[test]
    fn omit_leaves_containers_behind_but_never_its_own_value() {
        let omit = json!(omit_token());
        let mut args = json!({
            "toplevel_keep": 9,
            "toplevel_drop": omit,
            "nested": {"keep": 1, "drop": omit, "deeper": {"dropme": omit, "stay": 2}},
            "alist": [omit, 1, {"inlist": omit, "other": 3}],
            "nested_list": [[1, omit], 3],
            "only_omit_list": [omit],
            "empty_after": {"k": omit},
        });
        remove_omit(&mut args);
        assert_eq!(
            args,
            json!({
                "toplevel_keep": 9,
                "nested": {"keep": 1, "deeper": {"stay": 2}},
                "alist": [1, {"other": 3}],
                "nested_list": [[1], 3],
                "only_omit_list": [],
                "empty_after": {},
            })
        );
    }

    fn task(module: &str) -> PlayTask {
        PlayTask {
            name: "t".into(),
            module: module.into(),
            ..PlayTask::empty()
        }
    }

    fn plan() -> PlayPlan {
        PlayPlan {
            plan: watch::channel(Arc::new(Compiled::default())).1,
            force_handlers: false,
            play_vars: Map::new(),
            vars_files: HashMap::new(),
            all_play_hosts: Vec::new(),
            inventory: Arc::new(HashMap::new()),
            r#become: None,
            become_user: None,
        }
    }

    fn defaults() -> ConnectionDefaults {
        ConnectionDefaults {
            remote_user: None,
            private_key: None,
            host_key_checking: true,
            remote_tmp: "~/.ansible/tmp".into(),
            connect_timeout: Duration::from_secs(10),
            r#become: false,
            become_user: "root".into(),
            become_method: "sudo".into(),
            become_password: None,
        }
    }

    /// A host's variables as the render path carries them: the map, and an empty shared view.
    fn hvars(v: Value) -> HostVars {
        HostVars {
            map: vars(v),
            ..HostVars::default()
        }
    }

    fn vars(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    /// Every row of `LOCAL_MODULES` has an arm in `run_local` behind it.
    ///
    /// Nothing but a name joins the two. The table is what the pre-flight consults, what the
    /// documentation page is generated from and what decides a task runs on the controller
    /// rather than on the host; the `match` in `run_local` is what then runs. A row added to the
    /// table without an arm passes the loader, passes the pre-flight, shows its banner, and only
    /// then fails on the host that reached it, telling the operator that a module the engine
    /// advertises is "not a controller-side module". That is this project's most expensive bug
    /// family - accepted, then silently not done - and it is the same totality property the
    /// keyword tables carry, one level over.
    ///
    /// What would make this red: a name in `LOCAL_MODULES` that falls through to the catch-all
    /// arm.
    #[test]
    fn every_local_module_has_an_arm_in_run_local() {
        let inventory = crate::inventory::Inventory::parse_ini("h1\n").expect("an inventory");
        let store = Mutex::new(
            VarStore::new(&inventory, None, Path::new("."), Map::new()).expect("a var store"),
        );
        let templar = Templar::new(PathBuf::from("."));
        let step = Step {
            kind: StepKind::Task,
            task: PlayTask::empty(),
            block: None,
            section: crate::compile::Section::Body,
            role: None,
            origin: Arc::new(crate::compile::Origin::default()),
            include_params: None,
            hosts: None,
        };
        for spec in volant_protocol::modules::LOCAL_MODULES {
            let item = Item {
                element: None,
                label: None,
                args: Map::new(),
                vars: HostVars::default(),
                environment: BTreeMap::new(),
                skipped: None,
            };
            // Arguments are deliberately empty: what is asserted is that the name is known, not
            // that the module does its work. A module that needs an argument says so in its own
            // words, which is not the catch-all's words.
            let result = run_local(
                &task(spec.name),
                &item,
                &step,
                std::slice::from_ref(&"h1".to_string()),
                &templar,
                &store,
                0,
            );
            assert_ne!(
                result.0.get("msg").and_then(Value::as_str),
                Some(format!("{} is not a controller-side module", spec.name).as_str()),
                "{} is in LOCAL_MODULES with no arm in run_local",
                spec.name
            );
        }
    }

    /// Each complaint the argument check can make, in the reference's own words and in the order
    /// it makes them.
    ///
    /// Measured on ansible-core 2.19.12 by asking for all four at once: missing required
    /// arguments (sorted by name, whatever order the spec lists them in), then a type that
    /// cannot be converted, then a value outside `choices`, then an argument the spec does not
    /// have. A value the entry never named is still found when the host has a variable of that
    /// name, which is what lets `-e needed=given` pass a check the role entry said nothing
    /// about; a name the spec does not have is only refused when the entry wrote it, since a
    /// host carries hundreds of variables the spec has never heard of.
    ///
    /// What would make this red: a check that passes a permissive conversion the reference
    /// refuses, or refuses one it accepts - either way a role runs, or fails to run, on
    /// arguments the reference judges differently.
    #[test]
    fn the_argument_check_speaks_the_reference_s_sentences() {
        let check = |provided: Value, host: Value| {
            let item = Item {
                element: None,
                label: None,
                args: vars(json!({
                    "argument_spec": {
                        "zeta": {"type": "str", "required": true},
                        "alpha_req": {"type": "str", "required": true},
                        "count": {"type": "int"},
                        "flag": {"type": "bool"},
                        "pick": {"type": "str", "choices": ["alpha", "beta"]},
                    },
                    "provided_arguments": provided,
                    "validate_args_context": {"argument_spec_name": "main", "name": "types", "type": "role"},
                })),
                vars: hvars(host),
                environment: BTreeMap::new(),
                skipped: None,
            };
            validate_argument_spec(&item)
        };

        let all_wrong = check(
            json!({"count": "notanint", "pick": "gamma", "unknown_arg": "whatever"}),
            json!({}),
        );
        assert!(all_wrong.failed());
        assert_eq!(
            all_wrong.0["argument_errors"],
            json!([
                "missing required arguments: alpha_req, zeta",
                "argument 'count' is of type str and we were unable to convert to int: \"'notanint'\" cannot be converted to an int",
                "value of pick must be one of: alpha, beta, got: gamma",
                "unknown_arg. Supported parameters include: alpha_req, count, flag, pick, zeta.",
            ])
        );
        assert_eq!(
            all_wrong.0["msg"],
            json!(
                "Validation of arguments failed:\nmissing required arguments: alpha_req, zeta\nargument 'count' is of type str and we were unable to convert to int: \"'notanint'\" cannot be converted to an int\nvalue of pick must be one of: alpha, beta, got: gamma\nunknown_arg. Supported parameters include: alpha_req, count, flag, pick, zeta."
            )
        );
        assert_eq!(
            all_wrong.0["argument_spec_data"]["zeta"]["required"],
            json!(true)
        );

        // Measured: `"3"` is an `int` and `"yes"` is a `bool`, and nothing is written back - the
        // role's own tasks read `count=3 flag=yes`, the strings they were handed.
        let permissive = check(
            json!({"count": "3", "flag": "yes", "pick": "alpha", "zeta": "z", "alpha_req": "a"}),
            json!({}),
        );
        assert!(!permissive.failed(), "{:?}", permissive.0);
        assert_eq!(permissive.0["msg"], json!("The arg spec validation passed"));
        assert_eq!(
            permissive.0["validate_args_context"]["name"],
            json!("types")
        );

        // Nothing on the entry, everything on the host: the shape `-e zeta=z ...` makes.
        let from_host = check(
            json!({}),
            json!({"zeta": "z", "alpha_req": "a", "count": 2, "flag": false, "pick": "beta"}),
        );
        assert!(!from_host.failed(), "{:?}", from_host.0);
        // A host variable the spec never heard of is not an unknown argument.
        let noise = check(
            json!({}),
            json!({"zeta": "z", "alpha_req": "a", "ansible_forks": 5}),
        );
        assert!(!noise.failed(), "{:?}", noise.0);
    }

    /// The order measured against `ansible-core 2.19.12`: `ansible_become` wins over both
    /// keywords, in both directions, and the task keyword wins over the play's.
    #[test]
    fn a_host_variable_beats_both_become_keywords() {
        let templar = Templar::new(std::env::temp_dir());
        let escalate = |task_become, play_become, host: Value| {
            let mut t = task("command");
            t.r#become = task_become;
            let mut p = plan();
            p.r#become = play_become;
            become_for(&t, &p, &hvars(host), &defaults(), &templar).unwrap()
        };
        assert!(
            escalate(None, Some(true), json!({"ansible_become": false})).is_none(),
            "the variable turns a play's become off"
        );
        assert!(
            escalate(None, Some(false), json!({"ansible_become": true})).is_some(),
            "and turns it on where the play said no"
        );
        assert!(
            escalate(Some(true), None, json!({"ansible_become": false})).is_none(),
            "a task keyword loses to the variable too"
        );
        assert!(
            escalate(Some(false), Some(true), json!({})).is_none(),
            "with no variable, the task keyword beats the play's"
        );
        assert!(
            escalate(None, Some(true), json!({})).is_some(),
            "and the play keyword stands on its own"
        );
        assert!(
            escalate(None, None, json!({"ansible_become": "yes"})).is_some(),
            "a variable spelled the way Ansible content spells it is still a boolean"
        );
    }

    #[test]
    fn the_target_user_follows_the_same_order_and_defaults_to_root() {
        let templar = Templar::new(std::env::temp_dir());
        let mut t = task("command");
        t.r#become = Some(true);
        let mut p = plan();
        p.become_user = Some("play".into());
        assert_eq!(
            become_for(&t, &p, &HostVars::default(), &defaults(), &templar)
                .unwrap()
                .unwrap()
                .user,
            "play"
        );
        t.become_user = Some("task".into());
        assert_eq!(
            become_for(&t, &p, &HostVars::default(), &defaults(), &templar)
                .unwrap()
                .unwrap()
                .user,
            "task"
        );
        let host = hvars(json!({"ansible_become_user": "host"}));
        assert_eq!(
            become_for(&t, &p, &host, &defaults(), &templar)
                .unwrap()
                .unwrap()
                .user,
            "host",
            "the variable wins here as well"
        );
        let bare = task("command");
        let mut d = defaults();
        d.r#become = true;
        assert_eq!(
            become_for(&bare, &plan(), &HostVars::default(), &d, &templar)
                .unwrap()
                .unwrap()
                .user,
            "root",
            "nothing said anywhere means root, as in Ansible"
        );
        let templated = hvars(json!({"ansible_become_user": "{{ who }}", "who": "deploy"}));
        assert_eq!(
            become_for(&t, &p, &templated, &defaults(), &templar)
                .unwrap()
                .unwrap()
                .user,
            "deploy",
            "a templated user is resolved before the link is keyed by it"
        );
    }

    /// `become_user: "{{ item }}"` escalates per item in the reference, measured on the
    /// development machine: two items, `root` then the invoking account, and each ran as its
    /// own. Here it fails the task, because escalation is settled once per batch and one batch
    /// is one message to one agent under one user. What this pins is the message: it names the
    /// keyword and says why, where it used to say only that some option held an undefined
    /// variable.
    #[test]
    fn a_become_user_that_changes_per_loop_item_says_why_it_cannot() {
        let templar = Templar::new(std::env::temp_dir());
        let mut t = task("command");
        t.r#become = Some(true);
        t.become_user = Some("{{ item }}".into());
        t.loop_items = Some(json!(["root", "deploy"]));
        let err = become_for(&t, &plan(), &HostVars::default(), &defaults(), &templar).unwrap_err();
        assert!(
            err.0.contains("become_user") && err.0.contains("per loop item"),
            "{}",
            err.0
        );
    }

    /// A method arriving as a variable is refused by its name rather than quietly escalating
    /// with `sudo`: running the task under rules nobody wrote is worse than not running it.
    #[test]
    fn an_unsupported_become_method_variable_fails_the_task() {
        let templar = Templar::new(std::env::temp_dir());
        let mut t = task("command");
        t.r#become = Some(true);
        let err = become_for(
            &t,
            &plan(),
            &hvars(json!({"ansible_become_method": "su"})),
            &defaults(),
            &templar,
        )
        .unwrap_err();
        assert!(
            err.0.contains("su") && err.0.contains("not supported"),
            "{}",
            err.0
        );
        assert_eq!(
            become_for(
                &task("command"),
                &plan(),
                &hvars(json!({"ansible_become_method": "su"})),
                &defaults(),
                &templar,
            )
            .unwrap(),
            None,
            "a task that escalates nowhere is not refused for a method it never runs"
        );
        let d = ConnectionDefaults {
            become_method: "doas".into(),
            ..defaults()
        };
        let err = become_for(&t, &plan(), &HostVars::default(), &d, &templar).unwrap_err();
        assert!(
            err.0.contains("doas") && err.0.contains("not supported"),
            "the defaults are refused for an escalating task the startup pass could not see: {}",
            err.0
        );
        assert_eq!(
            become_for(
                &task("command"),
                &plan(),
                &HostVars::default(),
                &d,
                &templar
            )
            .unwrap(),
            None,
            "and the same defaults leave a task that never escalates alone"
        );
    }

    /// The password never has to be quoted, logged or formatted, so the one place it could still
    /// leak is a `Debug` that a `-vvv` diagnostic reaches.
    #[test]
    fn the_become_password_is_redacted_in_debug_output() {
        let escalation = Escalation {
            user: "root".into(),
            password: Some("s3cret".into()),
        };
        assert!(!format!("{escalation:?}").contains("s3cret"));
        let mut d = defaults();
        d.become_password = Some("s3cret".into());
        assert!(!format!("{d:?}").contains("s3cret"));
    }

    fn result(v: Value) -> TaskResult {
        TaskResult(v.as_object().unwrap().clone())
    }

    /// `ansible_failed_task` carries the keys the reference carries, and the ones this engine
    /// models carry the task's own values.
    ///
    /// Measured on ansible-core 2.19.12: the dictionary has 46 keys, six of them the engine's
    /// own bookkeeping (`uuid`, `finalized`, `squashed`, `_resolved_action`, `async_val`,
    /// `loop_with`); of the forty left, `local_action` and the `with_*` lookups are absent
    /// because the reference has already folded them into `action` and `loop`.
    ///
    /// What would make this red: a key dropped, which turns a rescue reading it into a failure
    /// of its own; or `action` carrying the fully qualified module name, which the reference
    /// keeps in a private key and which every playbook comparing `.action` to a short name
    /// would then miss.
    #[test]
    fn the_failed_task_carries_the_reference_s_own_keys() {
        let mut task = task("ansible.builtin.command");
        task.name = "boom".to_string();
        task.register = Some("b".to_string());
        task.when = vec!["true".to_string()];
        task.tags = vec!["t1".to_string()];
        task.vars = vars(json!({"v": 1}));
        let value = failed_task_value(&task);
        let object = value.as_object().expect("an object");
        assert_eq!(object.len(), 40, "{value}");
        for key in [
            "action",
            "args",
            "become",
            "connection",
            "loop",
            "loop_control",
            "name",
            "register",
            "tags",
            "vars",
            "when",
        ] {
            assert!(object.contains_key(key), "{key} is missing from {value}");
        }
        assert!(!object.contains_key("local_action"), "{value}");
        assert!(!object.contains_key("with_items"), "{value}");
        assert_eq!(object["action"], json!("command"));
        assert_eq!(object["name"], json!("boom"));
        assert_eq!(object["register"], json!("b"));
        assert_eq!(object["when"], json!(["true"]));
        assert_eq!(object["tags"], json!(["t1"]));
        assert_eq!(object["vars"], json!({"v": 1}));
        assert_eq!(object["loop"], Value::Null);
        assert_eq!(
            object["loop_control"],
            json!({"loop_var": "item", "label": null})
        );
        assert_eq!(
            object["connection"],
            Value::Null,
            "a keyword nothing here honours says nothing rather than the reference's default"
        );
    }

    #[test]
    fn a_loop_aggregate_follows_ansible_shape() {
        let mut t = task("command");
        t.loop_items = Some(json!([1, 2]));
        let results = vec![
            (Some(json!(1)), result(json!({"changed": true, "rc": 0}))),
            (
                Some(json!(2)),
                result(json!({"changed": false, "rc": 1, "failed": true})),
            ),
        ];
        let agg = registered_value(&t, &results);
        assert_eq!(agg["changed"], json!(true));
        assert_eq!(agg["failed"], json!(true));
        assert_eq!(agg["skipped"], json!(false));
        assert_eq!(agg["msg"], json!("All items completed"));
        assert_eq!(agg["results"][1]["item"], json!(2));
        assert_eq!(agg["results"][1]["ansible_loop_var"], json!("item"));
    }

    #[test]
    fn failed_when_false_rescues_a_non_zero_rc() {
        let templar = Templar::new(std::env::temp_dir());
        let mut t = task("command");
        t.failed_when = vec!["result.rc == 99".into()];
        let item = Item {
            element: None,
            label: None,
            args: Map::new(),
            vars: HostVars::default(),
            environment: BTreeMap::new(),
            skipped: None,
        };
        let r = apply_conditions(
            &t,
            &item,
            result(json!({"rc": 1, "changed": true})),
            &templar,
        )
        .unwrap();
        assert!(!r.failed(), "{:?}", r.0);
        assert_eq!(r.0["failed_when_result"], json!(false));
        let r = apply_conditions(
            &t,
            &item,
            result(json!({"rc": 99, "changed": true})),
            &templar,
        )
        .unwrap();
        assert!(r.failed());
    }

    #[test]
    fn labels_show_strings_bare_and_the_rest_as_json() {
        assert_eq!(display(&json!("one")), "one");
        assert_eq!(display(&json!({"name": "one"})), r#"{"name": "one"}"#);
        assert_eq!(display(&json!(3)), "3");
    }

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

    /// The barrier opens on the slowest live host, and a host that has left the play stops
    /// holding it: this is what keeps a wait from outliving the host it waits for.
    #[test]
    fn progress_follows_the_slowest_live_host_and_forgets_the_others() {
        let hosts: Vec<String> = vec!["alpha".into(), "beta".into()];
        let (tx, rx) = watch::channel(Progress::default());
        let mut last_done = HashMap::new();
        let mut lost = HashSet::new();

        publish(
            &tx,
            &hosts,
            &hosts,
            &lost,
            &last_done,
            None,
            &BTreeMap::new(),
        );
        assert_eq!(rx.borrow().completed_through, None, "nobody has reported");
        assert_eq!(rx.borrow().live_hosts, hosts);

        last_done.insert("beta".to_string(), 3);
        publish(
            &tx,
            &hosts,
            &hosts,
            &lost,
            &last_done,
            None,
            &BTreeMap::new(),
        );
        assert_eq!(
            rx.borrow().completed_through,
            None,
            "alpha has reported nothing, so the barrier stays shut"
        );

        last_done.insert("alpha".to_string(), 1);
        publish(
            &tx,
            &hosts,
            &hosts,
            &lost,
            &last_done,
            None,
            &BTreeMap::new(),
        );
        assert_eq!(rx.borrow().completed_through, Some(1));

        lost.insert("alpha".to_string());
        publish(
            &tx,
            &hosts,
            &hosts,
            &lost,
            &last_done,
            None,
            &BTreeMap::new(),
        );
        assert_eq!(
            rx.borrow().completed_through,
            Some(3),
            "a host out of the play no longer holds the barrier"
        );
        assert_eq!(rx.borrow().live_hosts, vec!["beta".to_string()]);

        lost.insert("beta".to_string());
        publish(
            &tx,
            &hosts,
            &hosts,
            &lost,
            &last_done,
            None,
            &BTreeMap::new(),
        );
        assert!(rx.borrow().live_hosts.is_empty());
        assert_eq!(rx.borrow().completed_through, None);
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
        let mut done = HashSet::new();
        let mut frontier = HashMap::new();
        let lost = HashSet::new();

        for index in [0, 1] {
            finished(&mut done, &mut frontier, "alpha", index);
        }
        // Steps 4 and 5 are stepped over: the driver says so while the batch holding 2 and 3 is
        // still running.
        for index in [4, 5] {
            finished(&mut done, &mut frontier, "alpha", index);
        }
        publish(
            &tx,
            &hosts,
            &hosts,
            &lost,
            &frontier,
            None,
            &BTreeMap::new(),
        );
        assert_eq!(
            rx.borrow().completed_through,
            Some(1),
            "the steps behind the gap are not finished yet"
        );

        finished(&mut done, &mut frontier, "alpha", 2);
        publish(
            &tx,
            &hosts,
            &hosts,
            &lost,
            &frontier,
            None,
            &BTreeMap::new(),
        );
        assert_eq!(rx.borrow().completed_through, Some(2), "the gap is smaller");

        finished(&mut done, &mut frontier, "alpha", 3);
        publish(
            &tx,
            &hosts,
            &hosts,
            &lost,
            &frontier,
            None,
            &BTreeMap::new(),
        );
        assert_eq!(
            rx.borrow().completed_through,
            Some(5),
            "the batch reported, so everything behind it counts"
        );
    }

    #[test]
    fn with_items_flattens_one_level_only() {
        assert_eq!(
            flatten_once(vec![json!([1, [2]]), json!(3)]),
            vec![json!(1), json!([2]), json!(3)]
        );
    }

    /// The step loop's boundary test is the union of two independent rules, and neither one
    /// covers the other: `run_once` is a boundary because the table says so and spells none of
    /// the cross-host names, a `hostvars` read is a boundary because of its text and carries no
    /// keyword, and `delegate_to` is neither - the delegating driver opens its own link to the
    /// delegate, so nothing is shared and nobody has to wait.
    ///
    /// What would make this red: the `barrier` flag dropped from the table, which lets two
    /// hosts elect two different runners for one `run_once` step and run it twice; the textual
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

    /// A delegate is the inventory's own host when it has one, the local host for the three
    /// implicit spellings, and an ssh target of that name otherwise.
    ///
    /// Measured on ansible-core 2.19.12: `delegate_to: localhost` with no `localhost` in the
    /// inventory runs locally, and `delegate_to: nosuch` is connected to rather than refused -
    /// it reports ssh's own resolution failure as `UNREACHABLE`.
    ///
    /// What would make this red: an unknown name refused, which refuses a playbook the
    /// reference runs; or `localhost` turned into an ssh target, which tries to connect to the
    /// machine the controller is already on.
    #[test]
    fn a_delegate_is_the_inventory_s_host_or_an_implicit_one() {
        let mut plan = plan();
        let mut known = Host {
            name: "h3".into(),
            vars: BTreeMap::new(),
        };
        known.vars.insert("ansible_host".into(), json!("10.0.0.3"));
        plan.inventory = Arc::new(HashMap::from([("h3".to_string(), known)]));

        let found = delegate_host("h3", &plan);
        assert_eq!(found.vars.get("ansible_host"), Some(&json!("10.0.0.3")));

        for name in ["localhost", "127.0.0.1", "::1"] {
            let implicit = delegate_host(name, &plan);
            assert_eq!(implicit.name, name);
            assert_eq!(
                implicit.vars.get("ansible_connection"),
                Some(&json!("local")),
                "{name}"
            );
        }

        let stranger = delegate_host("nosuch", &plan);
        assert_eq!(stranger.name, "nosuch");
        assert!(
            stranger.vars.is_empty(),
            "an unknown name is an ssh target of that name, not a refusal"
        );
    }

    /// A `run_once` step's registered variable and facts are written for every live host of the
    /// batch; every other step writes for its own host alone.
    ///
    /// What would make this red: the broadcast dropped, which leaves the hosts that did not run
    /// the task with an undefined variable behind it.
    #[test]
    fn a_run_once_step_writes_its_result_for_the_whole_batch() {
        let live = ["h1".to_string(), "h2".to_string()];
        let mut t = task("command");
        assert_eq!(fact_targets(&t, "h1", &live), ["h1"]);
        t.run_once = Some(true);
        assert_eq!(fact_targets(&t, "h1", &live), live);
    }
}
