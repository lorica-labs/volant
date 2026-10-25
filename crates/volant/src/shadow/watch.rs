// SPDX-License-Identifier: GPL-3.0-or-later
//! What Volant did during a replay, gathered as the run reports it: each result with its
//! status, the arguments each answered task was sent with, the inventory and the recap.

use std::collections::{BTreeMap, HashMap};

use serde_json::{Map, Value};
use volant_protocol::TaskResult;

use super::replay::Replay;
use crate::playbook::{Origin, PlayTask};
use crate::stats::{HostStats, Outcome, Stats};

#[derive(Debug, Default)]
pub(crate) struct Watched {
    pub(super) results: Vec<Done>,
    /// The arguments sent for (host, `file:line`, occurrence, item index).
    pub(super) args: HashMap<(String, String, usize, usize), Map<String, Value>>,
    /// Every host with the groups it belongs to besides `all`, as the first play saw them.
    pub(super) inventory: Option<BTreeMap<String, Vec<String>>>,
    pub(super) recap: BTreeMap<String, HostStats>,
    /// Every task the plays compiled, every branch, by `file:line`, with its name.
    pub(super) known: BTreeMap<String, String>,
}

/// One result line as Volant reported it.
#[derive(Debug)]
pub(super) struct Done {
    pub(super) host: String,
    pub(super) path: String,
    /// The step it belongs to: two runs of one file and line are two steps.
    pub(super) step: usize,
    /// A loop item's line rather than a whole task's.
    pub(super) item: bool,
    pub(super) status: &'static str,
    pub(super) result: Map<String, Value>,
}

impl Replay {
    /// Notes one result line of `host` for step `step`, written at `origin`.
    pub(crate) fn observe(
        &self,
        host: &str,
        origin: Option<&Origin>,
        step: usize,
        item: bool,
        outcome: Outcome,
        result: &TaskResult,
    ) {
        let status = match outcome {
            Outcome::Failed | Outcome::Rescued | Outcome::Ignored => "failed",
            Outcome::Skipped => "skipped",
            Outcome::Ok | Outcome::Changed if result.changed() => "changed",
            Outcome::Ok | Outcome::Changed => "ok",
        };
        let done = Done {
            host: host.to_owned(),
            path: self.path_of(origin),
            step,
            item,
            status,
            result: result.0.clone(),
        };
        self.lock().results.push(done);
    }

    /// Notes the arguments `task`'s item `item` was sent with on `host`.
    pub(crate) fn sent(
        &self,
        host: &str,
        task: &PlayTask,
        occurrence: usize,
        item: usize,
        args: &Map<String, Value>,
    ) {
        let key = (
            host.to_owned(),
            self.path_of(task.origin.as_ref()),
            occurrence,
            item,
        );
        self.lock().args.insert(key, args.clone());
    }

    /// Notes the inventory the first play sees, as `groups` (Ansible's `groups`) gives it.
    pub(crate) fn saw_inventory(&self, groups: &BTreeMap<String, Vec<String>>) {
        let mut watched = self.lock();
        if watched.inventory.is_some() {
            return;
        }
        let mut hosts: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (group, members) in groups {
            for host in members {
                let entry = hosts.entry(host.clone()).or_default();
                if group != "all" {
                    entry.push(group.clone());
                }
            }
        }
        hosts.values_mut().for_each(|g| g.sort());
        watched.inventory = Some(hosts);
    }

    /// Notes the tasks and handlers of a play as it compiled them: every branch, whether it then
    /// runs or not. Statements that run no module are left out, as the comparison leaves them.
    pub(crate) fn saw_tasks(&self, compiled: &crate::compile::Compiled) {
        let tasks = compiled
            .steps
            .iter()
            .map(|s| &s.task)
            .chain(compiled.handlers.iter().map(|h| &h.task));
        let mut watched = self.lock();
        for task in tasks {
            if task.origin.is_none() || super::compare::NOT_TASKS.contains(&task.module.as_str()) {
                continue;
            }
            let path = self.path_of(task.origin.as_ref());
            watched
                .known
                .entry(path)
                .or_insert_with(|| task.name.clone());
        }
    }

    /// Notes the recap as it stands; the last play's is the run's.
    pub(crate) fn saw_recap(&self, stats: &Stats) {
        self.lock().recap = stats.hosts().map(|(h, s)| (h.to_owned(), *s)).collect();
    }

    /// Every task the replay compiled, by `file:line`, with its name.
    pub(super) fn known(&self) -> BTreeMap<String, String> {
        self.lock().known.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Watched> {
        self.watched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
