// SPDX-License-Identifier: GPL-3.0-or-later
//! Every host's groups in the order their variables layer, worked out once for the whole
//! inventory.

use std::collections::HashMap;

use serde_json::{Map, Value};

use super::{Group, Host, Inventory};

/// Each host's groups, `all` included, in the order [`Inventory::sorted_groups_of`] gives them,
/// and each host's own line. One walk over the groups serves every host: asked host by host,
/// each question walks every group again, and an inventory of 18,000 hosts took ten minutes to
/// list.
pub struct Layering<'a> {
    groups: HashMap<String, Vec<&'a Group>>,
    hosts: HashMap<&'a str, &'a Host>,
}

impl Inventory {
    pub fn layering(&self) -> Layering<'_> {
        let depth = self.group_depths();
        let mut groups: HashMap<String, Vec<&Group>> = HashMap::new();
        for group in &self.groups {
            for host in self.group_hosts(group) {
                groups.entry(host).or_default().push(group);
            }
        }
        for held in groups.values_mut() {
            held.sort_by_key(|g| {
                (
                    depth.get(&g.name).copied().unwrap_or(1),
                    g.priority.unwrap_or(1),
                    g.name.clone(),
                )
            });
        }
        let mut hosts = HashMap::new();
        for host in &self.hosts {
            hosts.entry(host.name.as_str()).or_insert(host);
        }
        Layering { groups, hosts }
    }
}

impl<'a> Layering<'a> {
    pub(super) fn groups_of(&self, host: &str) -> &[&'a Group] {
        self.groups.get(host).map_or(&[], Vec::as_slice)
    }

    pub(super) fn host(&self, name: &str) -> Option<&'a Host> {
        self.hosts.get(name).copied()
    }

    /// [`Inventory::host_with_vars`]'s variables: the groups' in order, then the host's own.
    pub fn vars_of(&self, host: &str) -> Map<String, Value> {
        let mut vars = Map::new();
        for group in self.groups_of(host) {
            vars.extend(group.vars.clone());
        }
        if let Some(own) = self.host(host) {
            vars.extend(own.vars.clone());
        }
        vars
    }

    /// [`Inventory::host_line_vars`].
    pub fn line_vars(&self, host: &str) -> Option<&'a Map<String, Value>> {
        self.host(host).map(|h| &h.vars)
    }

    /// [`Inventory::layered_groups_of`].
    pub fn layered(&self, host: &str) -> Vec<String> {
        self.groups_of(host)
            .iter()
            .filter(|g| g.name != "all")
            .map(|g| g.name.clone())
            .collect()
    }

    /// The names the host's groups set, in the order the reference merges the groups, each once.
    pub fn var_names(&self, host: &str) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for group in self.groups_of(host) {
            for name in group.vars.keys() {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
        names
    }

    /// The group whose inventory variable `name` a host reads: the last of its groups, in the
    /// order they are merged, that sets it.
    pub fn setting(&self, host: &str, name: &str) -> Option<&'a str> {
        self.groups_of(host)
            .iter()
            .rev()
            .find(|g| g.vars.contains_key(name))
            .map(|g| g.name.as_str())
    }
}
