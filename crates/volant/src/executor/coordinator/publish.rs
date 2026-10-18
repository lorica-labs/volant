// SPDX-License-Identifier: GPL-3.0-or-later
//! The coordinator's published state, and the connections a batch hands back.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Mutex;

use tokio::sync::watch;

use crate::agent::AgentLink;
use crate::compile::Step;
use crate::template::Templar;
use crate::vars::VarStore;

use super::types::Progress;
use crate::executor::prepare::{PlayPlan, host_vars};
use crate::executor::{LinkKey, RunState};
/// The coordinator's published state: everything a publish reads, held in one place because it
/// is read under one serialised view and nowhere else.
pub(super) struct Coordinator<'a> {
    /// The batch's shared progress. Every driver reads it; only the coordinator writes it.
    pub(super) tx: &'a watch::Sender<Progress>,
    /// The batch's own hosts, which is what `ansible_play_batch` is served from.
    pub(super) batch_hosts: &'a [String],
    /// Every host of the play, which is what `ansible_play_hosts_all` is served from.
    pub(super) all_hosts: &'a [String],
    /// The index each host has finished every task through. Not the highest index it has
    /// mentioned: a driver reports the steps it is about to step over as soon as it has decided
    /// to, which is before the batch in front of them has run, so its reports do arrive out of
    /// order. See `finished`.
    pub(super) frontier: HashMap<String, usize>,
    /// The last splice point the coordinator has published a splice for, republished with every
    /// `Progress` so a driver waiting on one reads it whatever else moved.
    pub(super) spliced_through: Option<usize>,
    /// What the one host elected for each `run_once` step made of it, for the hosts waiting
    /// behind it. Filled by the coordinator rather than by the elected driver because the failure
    /// has to be published **with** the live-set change it causes: a failed result takes its host
    /// out of `live_hosts` in the same arm, and a host that saw the shrunken list before the
    /// verdict would read a leader that is merely gone and leave the play the quiet way, at the
    /// wrong exit code.
    pub(super) run_once: BTreeMap<usize, bool>,
    /// Who runs each `run_once` step. Written once per index and never rewritten. See
    /// `Progress::elected`.
    pub(super) elected: BTreeMap<usize, String>,
    /// The `any_errors_fatal` steps a host failed at. See `Progress::fatal`.
    pub(super) fatal: BTreeSet<usize>,
    /// Set once `max_fail_percentage` ended the batch. See `Progress::aborted`.
    pub(super) aborted: bool,
    /// The step the batch was lost at, which the two `NO MORE HOSTS LEFT` follow.
    pub(super) aborted_at: Option<usize>,
    /// The hosts of this batch whose task failed, for `max_fail_percentage`. Unreachable hosts
    /// are not in it: the reference counts failures alone.
    pub(super) failed_here: HashSet<String>,
    /// The latest step one of them failed at.
    pub(super) last_failure: Option<usize>,
    /// `max_fail_percentage`, rendered.
    pub(super) limit: Option<f64>,
    /// The batch as the inventory cut it, failed hosts of earlier plays included.
    pub(super) batch_size: usize,
    /// The hosts a `meta` action took out of this round of the play without failing them: the
    /// ones that ended, and the ones `clear_host_errors` forgave, which stay out of the play they
    /// were forgiven in. Left out of both host lists like a host that failed.
    pub(super) removed: HashSet<String>,
    /// See `Progress::ending`.
    pub(super) ending: BTreeSet<usize>,
    /// See `Progress::dismissed`.
    pub(super) dismissed: BTreeSet<String>,
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
    pub(super) fn finished(
        &mut self,
        done: &mut HashSet<(String, usize)>,
        host: &str,
        index: usize,
    ) {
        done.insert((host.to_string(), index));
        let mut next = self.frontier.get(host).map_or(0, |reached| reached + 1);
        while done.contains(&(host.to_string(), next)) {
            self.frontier.insert(host.to_string(), next);
            next += 1;
        }
    }

    /// Drops every election past `index`, which a splice invalidated: the entries name steps
    /// that have moved.
    pub(super) fn forget_elections_past(&mut self, index: usize) {
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
    pub(super) fn publish(&mut self, lost: &HashSet<String>, steps: &[Step]) {
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
pub(super) async fn keep_links(
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
pub(in crate::executor) fn escalated_links(links: &HashMap<LinkKey, AgentLink>) -> Vec<LinkKey> {
    links
        .keys()
        .filter(|key| key.become_user.is_some())
        .cloned()
        .collect()
}

/// Takes every connection belonging to one host out of the run's map.
pub(super) fn take_links(
    links: &mut HashMap<LinkKey, AgentLink>,
    host: &str,
) -> Vec<(LinkKey, AgentLink)> {
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
pub(super) fn task_name(
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
pub(in crate::executor) fn render_name(
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
