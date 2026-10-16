// SPDX-License-Identifier: GPL-3.0-or-later
//! The coordinator: the serialised view of a batch's progress, and everything that reaches
//! the terminal.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};
use tokio::sync::{Semaphore, mpsc, watch};
use volant_protocol::TaskResult;

use crate::agent::{AgentLink, AgentSource};
use crate::compile::{Compiled, Step, StepKind};
use crate::inventory::Host;
use crate::playbook::Play;
use crate::render::{Dump, Renderer};
use crate::stats::{Outcome, Stats};
use crate::template::Templar;
use crate::vars::VarStore;

use super::driver::{Definitions, drive_host, mark_boundaries};
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
    /// The `any_errors_fatal` steps some host failed or went unreachable at. Every other host
    /// of the batch fails there too, once it has finished the step: the entry is written before
    /// the failing host leaves the live set, so a host that sees every live host past the step
    /// and no entry knows nobody failed.
    pub fatal: BTreeSet<usize>,
    /// The batch lost more hosts than `max_fail_percentage` allows. Every host still running
    /// leaves at its next step, and the run ends behind the batch.
    pub aborted: bool,
    /// The `meta: end_batch` and `meta: end_play` steps whose condition held on the host that
    /// ran them. Written together with that step's `run_once` verdict, so a host that reads the
    /// verdict reads this too: it leaves the play there, not failed, and its `always` sections
    /// do not run.
    pub ending: BTreeSet<usize>,
    /// The hosts a `meta: refresh_inventory` took out of the play, because the play's pattern
    /// finds them no longer in the inventory it read again. They leave quietly, not failed,
    /// and a host reads this once it has read the step's verdict.
    pub dismissed: BTreeSet<String>,
}

pub(super) enum Event {
    /// One result line. `counts` says whether it enters the recap (loop items do not, their
    /// aggregate does); `show` whether a line is printed (a loop aggregate is, only when every
    /// item was skipped).
    Result {
        host: String,
        index: usize,
        label: Option<String>,
        outcome: Outcome,
        result: TaskResult,
        dump: Dump,
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
        /// The `[ERROR]:` block printed in front of a failure's line, when it has one.
        error: Option<crate::render::ErrorBlock>,
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
        /// The attempt that failed, for the `Result was:` the line may carry.
        result: TaskResult,
        /// The task's `no_log`.
        censored: bool,
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
    /// A line an action prints on stdout before its result, as the reference's `pause` prints
    /// `Pausing for N seconds`. Queued like a warning.
    Display {
        host: String,
        index: usize,
        line: String,
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
    /// The templated name of step `index`, rendered by `host`'s driver when it reached the step:
    /// what the banner shows when `host` is the one it is printed for. See `task_name`.
    Named {
        host: String,
        index: usize,
        name: String,
    },
    /// What a `meta` action did that the coordinator has to carry out or remember, sent in front
    /// of the step's `TaskDone` so that the verdict the other hosts wait for already holds it.
    Meta {
        host: String,
        index: usize,
        effect: MetaEffect,
    },
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
        /// Where the task the host could not be reached for was written.
        origin: Option<crate::playbook::Origin>,
        /// The step the batch that could not be reached began at, for `any_errors_fatal`.
        index: Option<usize>,
    },
    /// A host could not be reached for step `index` and the step said `ignore_unreachable`: the
    /// reference prints the same block and line as for a host that left, then `...ignoring`,
    /// and counts the step as `ok` and `ignored` with `unreachable` untouched. Queued behind the
    /// step's key like a result, since the host carries on and the step prints in host order.
    UnreachableIgnored {
        host: String,
        index: usize,
        msg: String,
        censored: bool,
        delegate: Option<String>,
        origin: Option<crate::playbook::Origin>,
    },
}

/// What a `meta` action asks of the batch, besides the banner it shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MetaEffect {
    /// This host leaves the play, not failed.
    EndHost,
    /// Every host of the batch leaves the play, not failed.
    EndBatch,
    /// As `EndBatch`, and no batch of the play after this one starts.
    EndPlay,
    /// The facts modules gathered for the hosts of the batch are forgotten.
    ClearFacts,
    /// The failures and the unreachable hosts of the batch are forgiven.
    ClearHostErrors,
    /// The inventory file is read again.
    RefreshInventory,
}

/// What a host that could not be reached shows: the reference's error block, then the
/// `UNREACHABLE!` line.
struct Unreached {
    msg: String,
    censored: bool,
    delegate: Option<String>,
    origin: Option<crate::playbook::Origin>,
}

impl Unreached {
    fn show(&self, host: &str, out: &mut Renderer) {
        // Measured on ansible-core 2.19.12: a `no_log` task's unreachable host gets its block,
        // the reason censored.
        out.error(&crate::render::ErrorBlock::unreachable(
            &self.msg,
            self.origin.as_ref(),
            self.censored,
        ));
        out.unreachable(host, &self.msg, self.censored, self.delegate.as_deref());
    }
}

/// How a round of a batch ended, for the play that runs it.
pub(super) struct BatchEnd {
    /// A `meta: end_play` held: no batch of the play after this one starts.
    pub end_play: bool,
    /// The hosts `meta: clear_host_errors` brought back, each with the step it picks the play up
    /// at. They run a round of their own once everybody else is done, as the reference's linear
    /// strategy goes back for them when its task list has run out.
    pub revived: Vec<(String, usize)>,
    /// The hosts that went unreachable in this round and were not forgiven afterwards.
    pub dark: Vec<String>,
    /// The step list as the round left it, handed to the next round so that the handlers and the
    /// includes spliced in on the way are already there. Only kept when `revived` is not empty.
    pub steps: Option<Compiled>,
}

/// What one round of a batch remembers besides the coordinator's own books.
#[derive(Default)]
struct Round {
    end_play: bool,
    /// Hosts that went unreachable and have not been forgiven.
    dark: HashSet<String>,
    /// Where each of them picks the play up if it is.
    resume: HashMap<String, usize>,
    /// Hosts whose `Unreachable` has been read and whose `Finished` has not. The `Finished` of
    /// such a host must not put it back among the failed: a `clear_host_errors` may have run in
    /// between, and the host's connections take a moment to close before it speaks.
    finishing: HashSet<String>,
    /// Hosts that were unreachable before this play and are brought back as well.
    dormant: Vec<String>,
    revived: Vec<(String, usize)>,
}

impl Round {
    fn unreachable(&mut self, host: &str, at: Option<usize>, plan: &PlayPlan) {
        self.dark.insert(host.to_string());
        self.finishing.insert(host.to_string());
        if let Some(at) = at {
            self.resume
                .insert(host.to_string(), crate::compile::after(&plan.steps(), at));
        }
    }

    /// Whether a host that finished failed is to be filed among the failed hosts by this event.
    fn files_failure(&mut self, host: &str, failed: bool) -> bool {
        let reported = self.finishing.remove(host);
        failed && !reported
    }
}

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
        lockstep: max_fail.is_some(),
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
    // Not `for .. in steps.iter()`: the list grows at a flush point, so its length is read again
    // each time round. The step is cloned rather than borrowed for the same reason - the splice
    // below replaces the value the borrow would point into.
    while let Some(step) = plan.steps().steps.get(index).cloned() {
        let mut header_shown = false;
        for host in &play_hosts {
            loop {
                // The batch is lost: every host of it is out of the play, the way the reference
                // marks them all failed.
                if coordinator.aborted && !marked_failed {
                    marked_failed = true;
                    state.failed_hosts.extend(play_hosts.iter().cloned());
                }
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
                            let spoken = names.get(&(host.clone(), index));
                            header(
                                &step,
                                &task_name(&step, host, &plan, &live, state, spoken),
                                out,
                            );
                            header_shown = true;
                        }
                        report_result(event, stats, out);
                    }
                    break;
                }
                if gone.contains(host) {
                    if let Some(blame) = unreached.remove(&key) {
                        if !header_shown {
                            let live = progress_tx.borrow().clone();
                            let spoken = names.get(&key);
                            header(
                                &step,
                                &task_name(&step, host, &plan, &live, state, spoken),
                                out,
                            );
                            header_shown = true;
                        }
                        blame.show(host, out);
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
                                .is_some_and(|s| s.task.bypasses_host_loop());
                        if announced {
                            coordinator.run_once.insert(key.1, true);
                        }
                        // A failed result is the host leaving the play, and it arrives before
                        // that task's `TaskDone`. Taking it out of the live set here, rather
                        // than waiting for its `Finished`, is what lets the next task read the
                        // shrunken host list without racing the driver that is shutting down.
                        if lost {
                            state.failed_hosts.insert(key.0.clone());
                            coordinator.failed_here.insert(key.0.clone());
                            coordinator.last_failure = coordinator.last_failure.max(Some(key.1));
                        }
                        // A failure `any_errors_fatal` covers, rescued or not: the verdict is
                        // published with the failing host's change of state, before any other
                        // host can see the step finished.
                        let fatal = sank
                            && plan
                                .steps()
                                .steps
                                .get(key.1)
                                .is_some_and(|s| s.task.fails_all());
                        if fatal {
                            coordinator.fatal.insert(key.1);
                        }
                        if lost || announced || fatal {
                            coordinator.publish(&state.failed_hosts, &plan.steps().steps);
                        }
                        pending.entry(key).or_default().push(event);
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
                        pending.entry((host, index)).or_default().push(event);
                    }
                    Some(Event::Include {
                        host,
                        index,
                        groups,
                    }) => {
                        includes.entry(index).or_default().push((host, groups));
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
                        stats.unreachable(&host);
                        round.unreachable(&host, at, &plan);
                        let blame = Unreached {
                            msg,
                            censored,
                            delegate,
                            origin,
                        };
                        if let Some(at) = at {
                            unreached.insert((host.clone(), at), blame);
                        } else {
                            if !header_shown {
                                let live = progress_tx.borrow().clone();
                                let spoken = names.get(&(host.clone(), index));
                                header(
                                    &step,
                                    &task_name(&step, &host, &plan, &live, state, spoken),
                                    out,
                                );
                                header_shown = true;
                            }
                            blame.show(&host, out);
                        }
                        state.failed_hosts.insert(host.clone());
                        if let Some(at) = at
                            && plan
                                .steps()
                                .steps
                                .get(at)
                                .is_some_and(|s| s.task.fails_all() && !s.task.ignores_errors())
                        {
                            coordinator.fatal.insert(at);
                        }
                        gone.insert(host);
                        coordinator.publish(&state.failed_hosts, &plan.steps().steps);
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
                        let spoken = names.get(&(against.to_string(), index));
                        header(
                            &step,
                            &task_name(&step, against, &plan, &live, state, spoken),
                            out,
                        );
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
                stats.unreachable(&host);
                round.unreachable(&host, at, &plan);
                // Measured on ansible-core 2.19.12: a `no_log` task's unreachable host gets its block,
                // the reason censored.
                out.error(&crate::render::ErrorBlock::unreachable(
                    &msg,
                    origin.as_ref(),
                    censored,
                ));
                out.unreachable(&host, &msg, censored, delegate.as_deref());
                state.failed_hosts.insert(host);
                if let Some(at) = at
                    && plan
                        .steps()
                        .steps
                        .get(at)
                        .is_some_and(|s| s.task.fails_all() && !s.task.ignores_errors())
                {
                    coordinator.fatal.insert(at);
                }
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
                        first: crate::compile::first(&plan.steps()),
                    },
                    state,
                    stats,
                );
                for message in warned {
                    out.warning(&message, false);
                }
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

/// What carrying out a `meta` effect reads besides the books the round keeps.
struct MetaContext<'a> {
    /// The host that ran the action and the step it ran at.
    host: &'a str,
    at: usize,
    /// The batch as the inventory cut it: measured, a clear in a later batch leaves an earlier batch's failures.
    whole: &'a [String],
    /// The play's `hosts:` pattern, which a refreshed inventory is asked about again.
    pattern: &'a str,
    plan: &'a PlayPlan,
    /// The step a host that was unreachable before the play picks it up at.
    first: usize,
}

/// Carries out what a `meta` action asked of the batch, and answers the warnings it raised. Runs
/// in the order the events arrive, which puts it in front of the `TaskDone` of the step: the
/// verdict the other hosts wait for is published after it.
fn apply_meta(
    effect: MetaEffect,
    round: &mut Round,
    coordinator: &mut Coordinator<'_>,
    cx: &MetaContext<'_>,
    state: &mut RunState,
    stats: &mut Stats,
) -> Vec<String> {
    let mut warned = Vec::new();
    match effect {
        MetaEffect::EndHost => {
            coordinator.removed.insert(cx.host.to_string());
        }
        MetaEffect::EndBatch | MetaEffect::EndPlay => {
            // Every host of the batch that is still in it, the one that asked included. The
            // verdict goes out in the same publish, because a host that read the shrunken list
            // before it would take the runner for a host that vanished without one.
            let staying: Vec<String> = coordinator
                .batch_hosts
                .iter()
                .filter(|h| !state.failed_hosts.contains(*h))
                .cloned()
                .collect();
            coordinator.removed.extend(staying);
            coordinator.ending.insert(cx.at);
            coordinator.run_once.insert(cx.at, false);
            round.end_play |= effect == MetaEffect::EndPlay;
        }
        MetaEffect::ClearFacts => {
            state
                .vars
                .lock()
                .expect("vars lock")
                .clear_gathered(cx.whole);
        }
        MetaEffect::ClearHostErrors => {
            // The hosts that are out of the play now stay out of this round, forgiven or not:
            // the reference revives an unreachable host in a round of its own and never a
            // failed one.
            coordinator.removed.extend(
                cx.whole
                    .iter()
                    .filter(|h| state.failed_hosts.contains(*h))
                    .cloned(),
            );
            for h in cx.whole {
                // An earlier play's unreachable host stays unreachable for every play after,
                // whatever is cleared: its mark on the exit code stays with it.
                let kept = state.unreachable_hosts.contains(h);
                stats.clear_errors(h, kept);
                if !kept {
                    state.failed_hosts.remove(h);
                }
            }
            for (h, resume) in round.resume.drain() {
                if cx.whole.contains(&h) {
                    round.dark.remove(&h);
                    state.failed_hosts.remove(&h);
                    round.revived.push((h, resume));
                }
            }
            for h in std::mem::take(&mut round.dormant) {
                state.failed_hosts.remove(&h);
                round.revived.push((h, cx.first));
            }
            coordinator.failed_here.clear();
            coordinator.last_failure = None;
        }
        MetaEffect::RefreshInventory => {
            let path = state
                .vars
                .lock()
                .expect("vars lock")
                .inventory_file()
                .map(std::path::Path::to_path_buf);
            // No inventory file, nothing to read again: the implicit `localhost` is all there is.
            if let Some(path) = path {
                // Measured on ansible-core 2.19.12: an inventory that cannot be read any more is
                // warned about and left empty, and the run carries on without any host in it.
                let fresh = crate::inventory::Inventory::load(&path).unwrap_or_else(|_| {
                    let shown = std::path::absolute(&path).unwrap_or_else(|_| path.clone());
                    warned.push(format!(
                        "Unable to parse {} as an inventory source",
                        shown.display()
                    ));
                    warned.push(
                        "No inventory was parsed, only implicit localhost is available".to_string(),
                    );
                    crate::inventory::Inventory::empty()
                });
                state
                    .vars
                    .lock()
                    .expect("vars lock")
                    .refresh_inventory(&fresh);
                // The hosts of the batch the play's pattern no longer finds leave the play where
                // it stands, not failed; a host the file gained joins the next play. The verdict
                // goes out in the same publish as the dismissal, for the reason the end of a
                // batch does.
                let kept: HashSet<String> = fresh
                    .resolve(cx.pattern)
                    .hosts
                    .into_iter()
                    .map(|host| host.name)
                    .collect();
                let leaving: Vec<String> = coordinator
                    .batch_hosts
                    .iter()
                    .filter(|h| !kept.contains(*h))
                    .cloned()
                    .collect();
                coordinator.removed.extend(leaving.iter().cloned());
                coordinator.dismissed.extend(leaving);
                coordinator.run_once.insert(cx.at, false);
                state.refreshed = Some(fresh);
            }
        }
    }
    coordinator.publish(&state.failed_hosts, &cx.plan.steps().steps);
    warned
}

/// Whether an event puts a line on the terminal, which is what a step's header goes in front
/// of. A result nobody shows still speaks when `ignore_errors` swallowed a failure: the
/// `...ignoring` line is the whole display of that step for that host.
fn shows_a_line(event: &Event) -> bool {
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
    if let Event::Display { line, .. } = &event {
        out.display(line);
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
    /// The `any_errors_fatal` steps a host failed at. See `Progress::fatal`.
    fatal: BTreeSet<usize>,
    /// Set once `max_fail_percentage` ended the batch. See `Progress::aborted`.
    aborted: bool,
    /// The step the batch was lost at, which the two `NO MORE HOSTS LEFT` follow.
    aborted_at: Option<usize>,
    /// The hosts of this batch whose task failed, for `max_fail_percentage`. Unreachable hosts
    /// are not in it: the reference counts failures alone.
    failed_here: HashSet<String>,
    /// The latest step one of them failed at.
    last_failure: Option<usize>,
    /// `max_fail_percentage`, rendered.
    limit: Option<f64>,
    /// The batch as the inventory cut it, failed hosts of earlier plays included.
    batch_size: usize,
    /// The hosts a `meta` action took out of this round of the play without failing them: the
    /// ones that ended, and the ones `clear_host_errors` forgave, which stay out of the play they
    /// were forgiven in. Left out of both host lists like a host that failed.
    removed: HashSet<String>,
    /// See `Progress::ending`.
    ending: BTreeSet<usize>,
    /// See `Progress::dismissed`.
    dismissed: BTreeSet<String>,
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

    /// The share of the batch that failed. A step under `any_errors_fatal` that a host failed at
    /// fails the whole batch, and the reference counts them all before it compares.
    fn lost_share(&self) -> f64 {
        if self.last_failure.is_some_and(|s| self.fatal.contains(&s)) {
            return 1.0;
        }
        self.failed_here.len() as f64 / self.batch_size as f64
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
            .filter(|h| !lost.contains(*h) && !self.removed.contains(*h))
            .cloned()
            .collect();
        let play_hosts_left: Vec<String> = self
            .all_hosts
            .iter()
            .filter(|h| !lost.contains(*h) && !self.removed.contains(*h))
            .cloned()
            .collect();
        // `max_fail_percentage`: once the step a host failed at is finished on every host still
        // live, the batch is lost if more of it failed than the limit allows - strictly more, so
        // one failure in four is within a limit of 25. Decided here, in the publish that would
        // open the barrier behind that step, so no host can start the next one first. Every host
        // is then out of the live set: the reference marks them all failed.
        if let Some(limit) = self.limit
            && !self.aborted
            && let Some(step) = self.last_failure
            && self.lost_share() > limit / 100.0
        {
            let finished = live_hosts
                .iter()
                .all(|h| self.frontier.get(h).is_some_and(|f| *f >= step));
            if finished {
                self.aborted = true;
                self.aborted_at = Some(step);
            }
        }
        let live_hosts = if self.aborted { Vec::new() } else { live_hosts };
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
            && step.task.bypasses_host_loop()
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
            fatal: self.fatal.clone(),
            aborted: self.aborted,
            ending: self.ending.clone(),
            dismissed: self.dismissed.clone(),
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
///
/// A templated name comes from `spoken`, the name `host`'s driver rendered when it reached the
/// step (`Event::Named`), whenever there is one. Rendering it here instead reads the host's
/// variables as they stand when the banner prints, and a host that has run ahead of the others
/// has already changed them: `Deploy {{ release }}` in front of `set_fact: release=2` would say
/// `Deploy 2` where the reference, which renders it as the step is queued, says `Deploy 1`.
fn task_name(
    step: &Step,
    host: &str,
    plan: &PlayPlan,
    live: &Progress,
    state: &RunState,
    spoken: Option<&String>,
) -> String {
    let name = match spoken {
        Some(name) => name.clone(),
        None => render_name(step, host, plan, live, &state.templar, &state.vars),
    };
    match step.role.and_then(|i| plan.steps().roles.get(i).cloned()) {
        Some(role) => format!("{} : {name}", role.name),
        None => name,
    }
}

/// A step's own name, rendered against `host`'s variables as they stand now when it is a
/// template, and as written otherwise. A name that does not render is shown as written.
pub(super) fn render_name(
    step: &Step,
    host: &str,
    plan: &PlayPlan,
    live: &Progress,
    templar: &Templar,
    store: &Mutex<VarStore>,
) -> String {
    let task = &step.task;
    if !Templar::is_template(&task.name) {
        return task.name.clone();
    }
    let playbook_dir = store
        .lock()
        .expect("vars lock")
        .playbook_dir()
        .to_path_buf();
    let vars = host_vars(
        host,
        plan,
        &task.vars,
        step.role,
        step.include_params.as_deref(),
        live,
        templar,
        store,
        &step.origin,
        &playbook_dir,
    );
    templar
        .render(&task.name, &vars)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| task.name.clone())
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
            fatal: BTreeSet::new(),
            aborted: false,
            aborted_at: None,
            failed_here: HashSet::new(),
            last_failure: None,
            limit: None,
            batch_size: hosts.len(),
            removed: HashSet::new(),
            ending: BTreeSet::new(),
            dismissed: BTreeSet::new(),
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

    /// Who runs a `run_once` step is decided once, and a host that leaves afterwards does not move
    /// it. The mask is part of that decision, not a filter applied after it.
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

    /// A host is never counted past what it has finished, whatever order its reports arrive in.
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
