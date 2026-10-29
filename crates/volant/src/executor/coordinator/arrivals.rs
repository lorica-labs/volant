// SPDX-License-Identifier: GPL-3.0-or-later
//! What the hosts that reached one include step asked for, grouped for the splice.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::compile::{Compiled, Grafted};
use crate::executor::include::IncludeGroup;

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
/// hosts only. `roles_run` holds the role instances each host has had brought in.
pub(super) fn arrivals(
    mut arrived: Vec<(String, Vec<IncludeGroup>)>,
    play_hosts: &[String],
    roles_run: &mut HashMap<String, HashSet<String>>,
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
        for group in groups {
            let runs = match &group.role_id {
                Some(id) => {
                    let ran = !roles_run
                        .entry(host.clone())
                        .or_default()
                        .insert(id.clone());
                    !(ran && group.unique)
                }
                None => true,
            };
            match grouped.entry(group.key.clone()) {
                Entry::Vacant(slot) => {
                    order.push(group.key.clone());
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
            narrow_dependencies(&mut expanded, &runners, roles_run);
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

/// A role that does not allow duplicates runs once per host. Measured on ansible-core 2.19.12: an
/// `include_role` whose role depends on one the host has already run, in the play's `roles:` or
/// through an earlier include, leaves it out for that host. Its steps are spliced in for the
/// hosts that have not run it.
fn narrow_dependencies(
    expanded: &mut Compiled,
    runners: &[String],
    roles_run: &mut HashMap<String, HashSet<String>>,
) {
    for (index, role) in expanded.roles.iter().enumerate() {
        let Some(id) = &role.dedup else { continue };
        let hosts: Vec<String> = runners
            .iter()
            .filter(|host| {
                roles_run
                    .entry((*host).clone())
                    .or_default()
                    .insert(id.clone())
            })
            .cloned()
            .collect();
        if hosts.len() == runners.len() {
            continue;
        }
        let hosts: Arc<[String]> = hosts.into();
        for step in expanded.steps.iter_mut().filter(|s| s.role == Some(index)) {
            step.hosts = Some(Arc::clone(&hosts));
        }
    }
}
