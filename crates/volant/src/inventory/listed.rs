// SPDX-License-Identifier: GPL-3.0-or-later
//! The inventory as `ansible-inventory` lists it: `--list`, `--host`, `--limit`, `--export` and
//! `--yaml`.

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

use super::Inventory;
use super::unsafe_text::{mark_unsafe, show_unsafe};
use crate::template::yaml_mapping;

/// Each host's, or each group's, variables as a listing shows them.
pub type VarsOf<'a> = dyn Fn(&str) -> BTreeMap<String, Value> + 'a;

impl Inventory {
    /// One host's variables as `--list` and `--host` print them, or `None` when it has none.
    fn shown_vars(
        &self,
        name: &str,
        vars_of: &dyn Fn(&str) -> BTreeMap<String, Value>,
        listing: bool,
    ) -> Option<Value> {
        let mut vars = vars_of(name);
        // Only `--list` marks the strings a compose computed.
        for ((host, var), computed) in &self.unsafe_vars {
            if listing
                && host == name
                && let Some(value) = vars.get_mut(var)
            {
                mark_unsafe(value, computed);
            }
        }
        (!vars.is_empty()).then(|| show_unsafe(&Value::Object(vars.into_iter().collect()), listing))
    }

    /// What `ansible-inventory --host` prints for a host: its variables, `{}` when it has none.
    pub fn host_dump(
        &self,
        name: &str,
        vars_of: &dyn Fn(&str) -> BTreeMap<String, Value>,
    ) -> Value {
        self.shown_vars(name, vars_of, false)
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()))
    }

    /// The inventory with only the named hosts left in it. Groups keep their children and their
    /// variables, as `ansible-inventory --limit` shows them.
    #[must_use]
    pub fn limited_to(&self, keep: &HashSet<String>) -> Inventory {
        let mut out = self.clone();
        out.hosts.retain(|h| keep.contains(&h.name));
        for group in &mut out.groups {
            group.hosts.retain(|h| keep.contains(h));
        }
        out
    }

    /// What `ansible-inventory --list` prints: every host's variables with its groups' folded
    /// in, `all`'s top groups, and each group's own hosts and children. An empty group has no
    /// entry of its own.
    pub fn dump(&self) -> Value {
        self.dump_with(
            &|host| self.host_with_vars(host).vars.into_iter().collect(),
            None,
        )
    }

    /// [`Inventory::dump`] with each host's variables taken from `vars_of`. With `--export`,
    /// `group_vars` gives each group its `vars`, left out when it has none.
    pub fn dump_with(&self, vars_of: &VarsOf<'_>, group_vars: Option<&VarsOf<'_>>) -> Value {
        if self.detached {
            // Nothing is under `all`, which is all the listing walks.
            return Inventory::default().dump_with(vars_of, group_vars);
        }
        // The listing shows the hosts the pattern `all` matches, and a host named `all` is all it
        // matches then: measured on ansible-core 2.19.12, the other groups keep their place among
        // the children and lose their hosts.
        if self.hosts.len() > 1 && self.hosts.iter().any(|h| h.name == "all") {
            return self
                .limited_to(&std::iter::once("all".to_string()).collect())
                .dump_with(vars_of, group_vars);
        }
        let shown_group_vars = |name: &str| {
            let vars = group_vars.map(|of| of(name)).unwrap_or_default();
            (!vars.is_empty())
                .then(|| show_unsafe(&Value::Object(vars.into_iter().collect()), true))
        };
        let mut out = serde_json::Map::new();
        let hostvars: serde_json::Map<String, Value> = self
            .hosts
            .iter()
            .filter_map(|h| Some((h.name.clone(), self.shown_vars(&h.name, vars_of, true)?)))
            .collect();
        out.insert(
            "_meta".into(),
            serde_json::json!({ "hostvars": hostvars, "profile": "inventory_legacy" }),
        );
        let top = self.top_groups();
        let mut all = serde_json::Map::new();
        all.insert("children".into(), serde_json::json!(top));
        if let Some(vars) = shown_group_vars("all") {
            all.insert("vars".into(), vars);
        }
        out.insert("all".into(), Value::Object(all));
        for group in self.groups.iter().filter(|g| g.name != "all") {
            let mut body = serde_json::Map::new();
            if !group.children.is_empty() {
                body.insert("children".into(), serde_json::json!(group.children));
            }
            if !group.hosts.is_empty() {
                body.insert("hosts".into(), serde_json::json!(group.hosts));
            }
            if let Some(vars) = shown_group_vars(&group.name) {
                body.insert("vars".into(), vars);
            }
            if !body.is_empty() {
                out.insert(group.name.clone(), Value::Object(body));
            }
        }
        Value::Object(out)
    }

    /// What `ansible-inventory --list --yaml` dumps, built as its `yaml_inventory` builds it:
    /// from `all` down, each group's children before its hosts, in the inventory's order. A
    /// host's variables (`host_vars`) are written where the walk first reaches it and `{}`
    /// after; a group already written is `{}`; empty `children`, `hosts` and `vars` are left
    /// out, and a group left with nothing is too. With `--export`, `group_vars` gives each
    /// group's `vars`, `None` when it has none.
    pub fn yaml_document(
        &self,
        host_vars: &mut dyn FnMut(&str) -> minijinja::Value,
        group_vars: &mut dyn FnMut(&str) -> Option<minijinja::Value>,
    ) -> minijinja::Value {
        if self.detached {
            return Inventory::default().yaml_document(host_vars, group_vars);
        }
        let (mut seen_hosts, mut seen_groups) = (HashSet::new(), HashSet::new());
        let all = self.yaml_group(
            "all",
            &mut seen_hosts,
            &mut seen_groups,
            host_vars,
            group_vars,
        );
        yaml_mapping(all.map(|body| ("all".to_string(), body)))
    }

    fn yaml_group(
        &self,
        name: &str,
        seen_hosts: &mut HashSet<String>,
        seen_groups: &mut HashSet<String>,
        host_vars: &mut dyn FnMut(&str) -> minijinja::Value,
        group_vars: &mut dyn FnMut(&str) -> Option<minijinja::Value>,
    ) -> Option<minijinja::Value> {
        let (children, hosts) = if name == "all" {
            (self.top_groups(), Vec::new())
        } else {
            self.group(name)
                .map(|g| (g.children.clone(), g.hosts.clone()))
                .unwrap_or_default()
        };
        let mut written = Vec::new();
        for child in children.into_iter().filter(|c| c != "all") {
            // The reference marks a group seen once it is written; marked before, a group is
            // never entered twice, which is the same walk on the acyclic tree the sources leave.
            if !seen_groups.insert(child.clone()) {
                written.push((child, yaml_mapping([])));
            } else if let Some(body) =
                self.yaml_group(&child, seen_hosts, seen_groups, host_vars, group_vars)
            {
                written.push((child, body));
            }
        }
        let hosts: Vec<(String, minijinja::Value)> = hosts
            .into_iter()
            .map(|host| {
                let vars = if seen_hosts.insert(host.clone()) {
                    host_vars(&host)
                } else {
                    yaml_mapping([])
                };
                (host, vars)
            })
            .collect();
        let mut body = Vec::new();
        if !written.is_empty() {
            body.push(("children".to_string(), yaml_mapping(written)));
        }
        if !hosts.is_empty() {
            body.push(("hosts".to_string(), yaml_mapping(hosts)));
        }
        if let Some(vars) = group_vars(name) {
            body.push(("vars".to_string(), vars));
        }
        (!body.is_empty()).then(|| yaml_mapping(body))
    }

    /// What `ansible-inventory --list --toml` dumps, built as its `toml_inventory` builds it:
    /// every group a table of its own in the order the walk from `all` meets it, with `children`
    /// as names, `hosts` and with `--export` `vars`. A host's variables are written the first
    /// time it is met; a group met again is walked again and replaces its first entry, its
    /// hosts then without variables.
    pub fn toml_document(
        &self,
        host_vars: &mut dyn FnMut(&str) -> serde_json::Map<String, Value>,
        group_vars: &mut dyn FnMut(&str) -> serde_json::Map<String, Value>,
    ) -> serde_json::Map<String, Value> {
        if self.detached {
            return Inventory::default().toml_document(host_vars, group_vars);
        }
        let has_ungrouped = self.group("ungrouped").is_some_and(|g| !g.hosts.is_empty());
        self.toml_group(
            "all",
            has_ungrouped,
            &mut HashSet::new(),
            host_vars,
            group_vars,
        )
    }

    fn toml_group(
        &self,
        name: &str,
        has_ungrouped: bool,
        seen_hosts: &mut HashSet<String>,
        host_vars: &mut dyn FnMut(&str) -> serde_json::Map<String, Value>,
        group_vars: &mut dyn FnMut(&str) -> serde_json::Map<String, Value>,
    ) -> serde_json::Map<String, Value> {
        let (children, hosts) = if name == "all" {
            (self.top_groups(), Vec::new())
        } else {
            self.group(name)
                .map(|g| (g.children.clone(), g.hosts.clone()))
                .unwrap_or_default()
        };
        let mut results = serde_json::Map::new();
        results.insert(name.to_string(), Value::Null);
        let mut names = Vec::new();
        for child in children.into_iter().filter(|c| c != "all") {
            if child == "ungrouped" && !has_ungrouped {
                continue;
            }
            if name != "all" {
                names.push(Value::String(child.clone()));
            }
            let sub = self.toml_group(&child, has_ungrouped, seen_hosts, host_vars, group_vars);
            results.extend(sub);
        }
        let mut hosted = serde_json::Map::new();
        for host in hosts {
            let vars = if seen_hosts.insert(host.clone()) {
                host_vars(&host)
            } else {
                serde_json::Map::new()
            };
            hosted.insert(host, Value::Object(vars));
        }
        let mut body = serde_json::Map::new();
        if !names.is_empty() {
            body.insert("children".into(), Value::Array(names));
        }
        if !hosted.is_empty() {
            body.insert("hosts".into(), Value::Object(hosted));
        }
        let vars = group_vars(name);
        if !vars.is_empty() {
            body.insert("vars".into(), Value::Object(vars));
        }
        if body.is_empty() {
            results.shift_remove(name);
        } else {
            results.insert(name.to_string(), Value::Object(body));
        }
        results
    }

    /// A group's own variables, as the inventory sources wrote them.
    pub fn group_vars_of(&self, name: &str) -> serde_json::Map<String, Value> {
        let Some(group) = self.group(name) else {
            return serde_json::Map::new();
        };
        let mut vars = group.vars.clone();
        // The reference keeps it among the group's variables, here it has a field of its own.
        if let Some(priority) = group.priority {
            vars.insert("ansible_group_priority".into(), priority.into());
        }
        vars
    }

    /// What `ansible-inventory --graph [GROUP]` prints, or `None` when `root` is not a group.
    /// `lines` gives the `{name = value}` lines under a host or a group, none without `--vars`.
    pub fn graph<E>(
        &self,
        root: &str,
        lines: &mut dyn FnMut(GraphItem<'_>) -> Result<Vec<String>, E>,
    ) -> Result<Option<String>, E> {
        if self.detached {
            return Inventory::default().graph(root, lines);
        }
        if root != "all" && root != "ungrouped" && self.group(root).is_none() {
            return Ok(None);
        }
        let mut out = String::new();
        self.graph_group(root, 0, &mut vec![root.to_string()], lines, &mut out)?;
        Ok(Some(out))
    }

    fn graph_group<E>(
        &self,
        name: &str,
        depth: usize,
        path: &mut Vec<String>,
        lines: &mut dyn FnMut(GraphItem<'_>) -> Result<Vec<String>, E>,
        out: &mut String,
    ) -> Result<(), E> {
        push_line(out, depth, &format!("@{name}:"));
        let (children, hosts) = if name == "all" {
            (self.top_groups(), Vec::new())
        } else {
            self.group(name)
                .map(|g| (g.children.clone(), g.hosts.clone()))
                .unwrap_or_default()
        };
        for child in children {
            // The sources refuse a group that would be its own ancestor, as the reference does, so this
            // only guards the walk.
            if path.contains(&child) {
                continue;
            }
            path.push(child.clone());
            self.graph_group(&child, depth + 1, path, lines, out)?;
            path.pop();
        }
        for host in hosts {
            push_line(out, depth + 1, &host);
            for line in lines(GraphItem::Host(&host))? {
                push_line(out, depth + 2, &line);
            }
        }
        for line in lines(GraphItem::Group(name))? {
            push_line(out, depth + 1, &line);
        }
        Ok(())
    }

    /// `all`'s children: `ungrouped`, then the groups no other group holds.
    pub(super) fn top_groups(&self) -> Vec<String> {
        let nested: HashSet<&str> = self
            .groups
            .iter()
            .filter(|g| g.name != "all")
            .flat_map(|g| g.children.iter().map(String::as_str))
            .collect();
        let mut top = vec!["ungrouped".to_string()];
        top.extend(
            self.groups
                .iter()
                .map(|g| g.name.clone())
                .filter(|n| n != "all" && n != "ungrouped" && !nested.contains(n.as_str())),
        );
        top
    }
}

/// What `--graph --vars` asks the variables of.
pub enum GraphItem<'a> {
    Host(&'a str),
    Group(&'a str),
}

/// One line of the tree: `item` at `depth`.
fn push_line(out: &mut String, depth: usize, item: &str) {
    out.push_str(&graph_indent(depth));
    out.push_str(item);
    out.push(char::from(10));
}

/// The text in front of an item `depth` levels down: two spaces, `|  ` for each level above, `|--`.
fn graph_indent(depth: usize) -> String {
    if depth == 0 {
        String::new()
    } else {
        format!("  {}|--", "|  ".repeat(depth - 1))
    }
}
