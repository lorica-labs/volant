// SPDX-License-Identifier: GPL-3.0-or-later
//! The state a batch shares: progress, events, meta effects and the batch's end.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use volant_protocol::TaskResult;

use crate::agent::AgentLink;
use crate::compile::Compiled;
use crate::render::{Dump, Renderer};
use crate::stats::Outcome;

use crate::executor::LinkKey;
use crate::executor::include::IncludeGroup;
use crate::executor::prepare::PlayPlan;
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

pub(in crate::executor) enum Event {
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
    /// `Pausing for N seconds`, or the `ASYNC POLL` lines a job's polling prints. Queued like a
    /// warning.
    Display {
        host: String,
        index: usize,
        line: String,
        /// Printed in the reference's debug colour, as the `ASYNC` lines are.
        debug: bool,
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
pub(in crate::executor) enum MetaEffect {
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
pub(super) struct Unreached {
    pub(super) msg: String,
    pub(super) censored: bool,
    pub(super) delegate: Option<String>,
    pub(super) origin: Option<crate::playbook::Origin>,
}

impl Unreached {
    pub(super) fn show(&self, host: &str, out: &mut Renderer) {
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
pub(in crate::executor) struct BatchEnd {
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
pub(super) struct Round {
    pub(super) end_play: bool,
    /// Hosts that went unreachable and have not been forgiven.
    pub(super) dark: HashSet<String>,
    /// Where each of them picks the play up if it is.
    pub(super) resume: HashMap<String, usize>,
    /// Hosts whose failure (an `Unreachable`, or a failed result that ends their play) has been
    /// filed and whose `Finished` has not been read. The `Finished` of such a host must not put
    /// it back among the failed: a `clear_host_errors` may have run in between, and the host's
    /// connections take a moment to close before it speaks.
    pub(super) finishing: HashSet<String>,
    /// Hosts that were unreachable before this play and are brought back as well.
    pub(super) dormant: Vec<String>,
    pub(super) revived: Vec<(String, usize)>,
}

impl Round {
    pub(super) fn unreachable(&mut self, host: &str, at: Option<usize>, plan: &PlayPlan) {
        self.dark.insert(host.to_string());
        self.finishing.insert(host.to_string());
        if let Some(at) = at {
            self.resume
                .insert(host.to_string(), crate::compile::after(&plan.steps(), at));
        }
    }

    /// Files a host whose failed result ends its play among the failed hosts, so that its
    /// `Finished` does not file it again.
    pub(super) fn failed(&mut self, failed_hosts: &mut HashSet<String>, host: &str) {
        failed_hosts.insert(host.to_string());
        self.finishing.insert(host.to_string());
    }

    /// Whether a host that finished failed is to be filed among the failed hosts by this event.
    pub(super) fn files_failure(&mut self, host: &str, failed: bool) -> bool {
        let reported = self.finishing.remove(host);
        failed && !reported
    }
}
