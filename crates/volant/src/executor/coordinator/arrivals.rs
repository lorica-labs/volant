// SPDX-License-Identifier: GPL-3.0-or-later
//! What the hosts that reached one include step asked for, grouped for the splice.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use crate::compile::{Compiled, Grafted, Step};
use crate::executor::include::IncludeGroup;
use crate::roles::RoleKind;
use crate::stats::Outcome;

use super::types::Event;

/// One `included:` line: what it names, the hosts that show it, and the item's label.
pub(super) type Line = (String, Vec<String>, Option<String>);

pub(super) struct Arrivals {
    /// The hosts that asked, in the order the play lists them.
    pub asking: Vec<String>,
    pub lines: Vec<Line>,
    pub expansions: Vec<Grafted>,
}

/// The requests of every host, grouped so two hosts that asked for the same thing share one
/// `included:` line and one copy of the steps behind it. First appearance decides the order,
/// walked in the play's host order and then in each host's item order, which is the order the
/// reference prints them in.
///
/// A host whose role has already run still shows the `included:` line of an `include_role` that
/// says `allow_duplicates: false`, measured, and the steps behind it are spliced in for the other
/// hosts only. A role counts as run once the host has completed it, see `completion`: an
/// earlier `import_role` or `include_role` of it, not a role of `roles:`.
pub(super) fn arrivals(
    mut arrived: Vec<(String, Vec<IncludeGroup>)>,
    play_hosts: &[String],
    completion: &Completion,
) -> Arrivals {
    // `usize::MAX` rather than `None` for a host the batch does not hold, which would otherwise
    // sort in front of every rank and take the first line.
    arrived.sort_by_key(|(host, _)| {
        play_hosts
            .iter()
            .position(|h| h == host)
            .unwrap_or(usize::MAX)
    });
    let asking: Vec<String> = arrived.iter().map(|(host, _)| host.clone()).collect();
    let mut order: Vec<String> = Vec::new();
    // The hosts of a group, and among them the ones the steps are spliced in for.
    let mut grouped: HashMap<String, (IncludeGroup, Vec<String>, Vec<String>)> = HashMap::new();
    for (host, groups) in arrived {
        // A host that asks for the same include twice, a loop over `[foo, foo]` say, joins a
        // second group: measured on ansible-core 2.19.12, the file is included once per item.
        let mut seen: HashMap<String, usize> = HashMap::new();
        for group in groups {
            let nth = seen.entry(group.key.clone()).or_default();
            let key = format!("{}\0{nth}", group.key);
            *nth += 1;
            let runs = match &group.role_id {
                Some(id) => {
                    let ran = completion.completed(&host, id);
                    !(ran && group.unique)
                }
                None => true,
            };
            match grouped.entry(key.clone()) {
                Entry::Vacant(slot) => {
                    order.push(key);
                    let runners = if runs { vec![host.clone()] } else { Vec::new() };
                    slot.insert((group, vec![host.clone()], runners));
                }
                Entry::Occupied(mut slot) => {
                    slot.get_mut().1.push(host.clone());
                    if runs {
                        slot.get_mut().2.push(host.clone());
                    }
                }
            }
        }
    }
    let mut lines = Vec::new();
    let mut expansions = Vec::new();
    for key in order {
        let (group, hosts, runners) = grouped.remove(&key).expect("a key just inserted");
        lines.push((group.what, hosts, group.label));
        if !runners.is_empty() {
            let mut expanded = group.expanded;
            // The included role completes under the statement's identity, which a later
            // `include_role` of it that says `allow_duplicates: false` is compared with.
            for role in &mut expanded.roles {
                if role.kind == RoleKind::Include {
                    role.dedup.clone_from(&group.role_id);
                }
            }
            expansions.push(Grafted {
                expanded,
                params: group.params,
                hosts: runners.into(),
            });
        }
    }
    Arrivals {
        asking,
        lines,
        expansions,
    }
}

// Which role instances each host has completed, the way ansible-core 2.19.12 counts them.
//
// Measured with the `roles` integration target's `role_complete.yml`, and read off
// `StrategyBase._process_pending_results` and the implicit `meta: role_complete` a role ends
// with: a task of a role that came back ok or failed, and not skipped, marks the role as having
// had a task run for that host, and once the host passes the role's last task it has completed
// it. From then on every task of an instance with the same identity is left out for that host
// when that instance does not allow duplicates. A role whose tasks all skipped, or that the host
// could not be reached for, runs again at its next instance.

/// The role identities each host has completed.
pub(in crate::executor) type Completed = Arc<HashMap<String, HashSet<String>>>;

#[derive(Default)]
pub(in crate::executor) struct Completion {
    /// The role identities a task ran for, per host.
    ran: HashMap<String, HashSet<String>>,
    done: Completed,
    /// The step that ends each role instance, by step index.
    ends: HashMap<usize, usize>,
    /// Each role instance's identity, and whether a host that completed it leaves it out.
    roles: Vec<(Option<String>, bool)>,
    /// The length of the step list `ends` and `roles` were read from.
    read_from: (usize, usize),
}

impl Completion {
    /// Notes a result: a task of a role that was not skipped. A `run_once` result counts for the
    /// host elected to run it alone. Unreachable hosts report no result.
    pub(super) fn result(
        &mut self,
        event: &Event,
        compiled: &Compiled,
        elected: &BTreeMap<usize, String>,
    ) {
        let Event::Result {
            host,
            index,
            outcome,
            counts: true,
            ..
        } = event
        else {
            return;
        };
        let Some(step) = compiled.steps.get(*index) else {
            return;
        };
        if *outcome == Outcome::Skipped
            || step.task.bypasses_host_loop() && elected.get(index) != Some(host)
        {
            return;
        }
        if let Some(id) = identity(compiled, step) {
            self.ran.entry(host.clone()).or_default().insert(id.clone());
        }
    }

    /// A host is done with the step at `index`: past the last step of a role it ran a task of, it
    /// has completed that role.
    pub(super) fn task_done(&mut self, host: &str, index: usize, compiled: &Compiled) {
        self.read(compiled);
        let Some(id) = self
            .ends
            .get(&index)
            .and_then(|r| self.roles.get(*r)?.0.as_ref())
        else {
            return;
        };
        if !self.ran.get(host).is_some_and(|ran| ran.contains(id)) || self.completed(host, id) {
            return;
        }
        let mut done = (*self.done).clone();
        done.entry(host.to_string()).or_default().insert(id.clone());
        self.done = Arc::new(done);
    }

    /// What the drivers read, see `already_ran`.
    pub(super) fn done(&self) -> Completed {
        Arc::clone(&self.done)
    }

    pub(super) fn completed(&self, host: &str, id: &str) -> bool {
        self.done.get(host).is_some_and(|done| done.contains(id))
    }

    /// Whether `host` leaves `step` out, for the election of a `run_once` runner.
    pub(super) fn skips(&self, step: &Step, host: &str) -> bool {
        step.role
            .and_then(|r| self.roles.get(r))
            .is_some_and(|(id, unique)| {
                *unique && id.as_ref().is_some_and(|id| self.completed(host, id))
            })
    }

    fn read(&mut self, compiled: &Compiled) {
        let size = (compiled.steps.len(), compiled.roles.len());
        if self.read_from == size {
            return;
        }
        self.read_from = size;
        let mut last: HashMap<usize, usize> = HashMap::new();
        for (index, step) in compiled.steps.iter().enumerate() {
            if let Some(role) = step.role {
                last.insert(role, index);
            }
        }
        self.ends = last
            .into_iter()
            .map(|(role, index)| (index, role))
            .collect();
        self.roles = compiled
            .roles
            .iter()
            .map(|role| (role.dedup.clone(), role.unique))
            .collect();
    }
}

/// The identity a host completes by running `step`, if `step` belongs to a role.
fn identity<'a>(compiled: &'a Compiled, step: &Step) -> Option<&'a String> {
    compiled.roles.get(step.role?)?.dedup.as_ref()
}
