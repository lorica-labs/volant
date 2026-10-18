// SPDX-License-Identifier: GPL-3.0-or-later
//! The group graph: adding members and children, walking a group's hosts and its parents, and the
//! order groups layer in.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use serde_json::Value;

use super::{Group, Host, Inventory};

impl Inventory {
    /// Adds `host` to `group`, creating the group: what `add_host` and `group_by` do mid-run.
    pub fn add_member(&mut self, host: &str, group: &str) {
        let group = self.group_mut(group);
        if !group.hosts.iter().any(|h| h == host) {
            group.hosts.push(host.to_string());
        }
    }

    /// Makes `child` a child group of `parent`, creating both. A group is never its own child.
    pub fn add_child(&mut self, parent: &str, child: &str) {
        if parent == child {
            return;
        }
        self.group_mut(child);
        let parent = self.group_mut(parent);
        if !parent.children.iter().any(|c| c == child) {
            parent.children.push(child.to_string());
        }
    }

    /// Adds a host, or merges `vars` into the one of that name.
    pub fn add_host(&mut self, name: &str, vars: BTreeMap<String, Value>) {
        match self.hosts.iter_mut().find(|h| h.name == name) {
            Some(host) => {
                self.unsafe_vars
                    .retain(|(h, v), _| h != name || !vars.contains_key(v));
                host.vars.extend(vars);
            }
            None => self.hosts.push(Host {
                name: name.to_string(),
                vars,
            }),
        }
    }

    pub(super) fn group_mut(&mut self, name: &str) -> &mut Group {
        if let Some(pos) = self.groups.iter().position(|g| g.name == name) {
            return &mut self.groups[pos];
        }
        self.groups.push(Group {
            name: name.to_string(),
            ..Group::default()
        });
        self.groups.last_mut().expect("just pushed")
    }

    pub(super) fn group(&self, name: &str) -> Option<&Group> {
        self.groups.iter().find(|g| g.name == name)
    }

    /// Hosts of a group and of its descendants, in the order `ansible-inventory` produces: the
    /// group's own hosts first, then each descendant group's own hosts, breadth-first over
    /// `:children`, without duplicates. This is not the inventory's global host order — a group
    /// nested two levels down can list hosts declared earlier in the file than a shallower
    /// sibling, and Ansible's traversal order (not the file's) is what wins.
    pub(super) fn group_hosts(&self, group: &Group) -> Vec<String> {
        if group.name == "all" {
            return self.all_hosts();
        }
        let children: Vec<&Group> = group
            .children
            .iter()
            .filter_map(|c| self.group(c))
            .collect();
        self.hosts_breadth_first(&group.name, &group.hosts, children)
    }

    /// `all`'s children are implicit: `ungrouped` first, then every other group nobody's
    /// `:children` section lists, in the order those groups were first declared.
    fn all_children(&self) -> Vec<&Group> {
        let mut out = Vec::new();
        if let Some(ungrouped) = self.group("ungrouped") {
            out.push(ungrouped);
        }
        let has_parent: HashSet<&str> = self
            .groups
            .iter()
            .filter(|g| g.name != "all")
            .flat_map(|g| g.children.iter().map(String::as_str))
            .collect();
        // The groups a source put under `all` itself come first, then the ones nobody holds.
        let explicit = self
            .group("all")
            .map(|g| g.children.as_slice())
            .unwrap_or_default();
        out.extend(
            explicit
                .iter()
                .filter(|c| c.as_str() != "ungrouped")
                .filter_map(|c| self.group(c)),
        );
        for g in &self.groups {
            if g.name != "all"
                && g.name != "ungrouped"
                && !has_parent.contains(g.name.as_str())
                && !explicit.contains(&g.name)
            {
                out.push(g);
            }
        }
        out
    }

    /// Every host in the inventory, in `ansible-inventory`'s order for the pattern `all`: not the
    /// order hosts were declared, but a breadth-first walk of the implicit group tree rooted at
    /// `all` (see `all_children`). Measured against the reference: a plain, ungrouped inventory
    /// happens to produce file order, but one with nested `:children` groups does not.
    ///
    /// `all_children` only lists groups no other group's `:children` names, so a group cycle
    /// (every member has a parent, hence none is a root) is invisible to the walk above and its
    /// hosts would otherwise never enter `all` at all. Ansible's own `all` holds every host
    /// unconditionally, so anything the walk missed is appended here, in declaration order,
    /// rather than silently dropped.
    pub(super) fn all_hosts(&self) -> Vec<String> {
        let own_hosts = self
            .group("all")
            .map(|g| g.hosts.clone())
            .unwrap_or_default();
        let mut hosts = self.hosts_breadth_first("all", &own_hosts, self.all_children());
        let reached: HashSet<&str> = hosts.iter().map(String::as_str).collect();
        let missed: Vec<String> = self
            .hosts
            .iter()
            .map(|h| h.name.clone())
            .filter(|n| !reached.contains(n.as_str()))
            .collect();
        hosts.extend(missed);
        hosts
    }

    /// Ansible's `Group._get_hosts`: the root's own hosts, then a breadth-first walk of
    /// `:children`, each group contributing its own hosts (not its descendants') in turn, hosts
    /// deduplicated by first appearance.
    pub(super) fn hosts_breadth_first(
        &self,
        root_name: &str,
        root_hosts: &[String],
        initial_children: Vec<&Group>,
    ) -> Vec<String> {
        let mut ordered_groups: Vec<&Group> = Vec::new();
        let mut seen_groups: HashSet<&str> = HashSet::from([root_name]);
        for g in &initial_children {
            if seen_groups.insert(g.name.as_str()) {
                ordered_groups.push(g);
            }
        }
        let mut frontier = initial_children;
        while !frontier.is_empty() {
            let mut next = Vec::new();
            for g in &frontier {
                for c in g.children.iter().filter_map(|c| self.group(c)) {
                    if seen_groups.insert(c.name.as_str()) {
                        ordered_groups.push(c);
                        next.push(c);
                    }
                }
            }
            frontier = next;
        }
        let mut hosts = Vec::new();
        let mut seen_hosts: HashSet<&str> = HashSet::new();
        for h in root_hosts
            .iter()
            .chain(ordered_groups.iter().flat_map(|g| g.hosts.iter()))
        {
            if seen_hosts.insert(h.as_str()) {
                hosts.push(h.clone());
            }
        }
        hosts
    }

    /// The groups holding `host`, `all` included, in the order the reference layers their
    /// variables (`sort_groups`): shallower first, then lower `ansible_group_priority`, then by
    /// name, so the last one wins.
    pub(super) fn sorted_groups_of(&self, host: &str) -> Vec<&Group> {
        let depth = self.group_depths();
        let mut groups: Vec<&Group> = self
            .groups
            .iter()
            .filter(|g| self.group_hosts(g).iter().any(|h| h == host))
            .collect();
        groups.sort_by_key(|g| {
            (
                depth.get(&g.name).copied().unwrap_or(1),
                g.priority.unwrap_or(1),
                g.name.clone(),
            )
        });
        groups
    }

    /// The groups holding `host` but `all`, in the order their `group_vars` files are layered.
    pub fn layered_groups_of(&self, host: &str) -> Vec<String> {
        self.sorted_groups_of(host)
            .into_iter()
            .filter(|g| g.name != "all")
            .map(|g| g.name.clone())
            .collect()
    }

    /// Distance from the top: `all` sits at depth 0 and is not an implicit parent of anything,
    /// so every other group nobody lists as a child still sits at depth 1, their children at 2.
    fn group_depths(&self) -> HashMap<String, usize> {
        let children: HashSet<&str> = self
            .groups
            .iter()
            .flat_map(|g| g.children.iter().map(String::as_str))
            .collect();
        let mut depth = HashMap::new();
        let mut visited = HashSet::new();
        let mut queue: VecDeque<(&Group, usize)> = self
            .groups
            .iter()
            .filter(|g| g.name != "all" && !children.contains(g.name.as_str()))
            .map(|g| (g, 1))
            .collect();
        if let Some(all) = self.group("all") {
            visited.insert(all.name.as_str());
            depth.insert(all.name.clone(), 0);
        }
        while let Some((g, d)) = queue.pop_front() {
            if !visited.insert(g.name.as_str()) {
                // Same guard as `group_hosts`: a cyclic `:children` chain must not requeue.
                continue;
            }
            depth.insert(g.name.clone(), d);
            queue.extend(
                g.children
                    .iter()
                    .filter_map(|c| self.group(c))
                    .map(|c| (c, d + 1)),
            );
        }
        depth
    }

    /// Groups the host belongs to, sorted, without `all`: Ansible's `group_names`.
    pub fn group_names_of(&self, host: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .groups
            .iter()
            .filter(|g| g.name != "all" && self.group_hosts(g).iter().any(|h| h == host))
            .map(|g| g.name.clone())
            .collect();
        names.sort();
        names
    }

    /// Every group with its member hosts, including `all` and `ungrouped`: Ansible's `groups`.
    pub fn groups(&self) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = self
            .groups
            .iter()
            .map(|g| (g.name.clone(), self.group_hosts(g)))
            .collect();
        out.entry("all".to_string())
            .or_insert_with(|| self.all_hosts());
        out.entry("ungrouped".to_string()).or_default();
        out
    }
}
