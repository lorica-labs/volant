// SPDX-License-Identifier: GPL-3.0-or-later
//! The inventory as `ansible-inventory` lists it: `--list`, `--host` and `--limit`.

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

use super::Inventory;
use super::unsafe_text::{mark_unsafe, show_unsafe};

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
        self.dump_with(&|host| self.host_with_vars(host).vars)
    }

    /// [`Inventory::dump`] with each host's variables taken from `vars_of`.
    pub fn dump_with(&self, vars_of: &dyn Fn(&str) -> BTreeMap<String, Value>) -> Value {
        if self.detached {
            // Nothing is under `all`, which is all the listing walks.
            return Inventory::default().dump();
        }
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
        out.insert("all".into(), serde_json::json!({ "children": top }));
        for group in self.groups.iter().filter(|g| g.name != "all") {
            let mut body = serde_json::Map::new();
            if !group.children.is_empty() {
                body.insert("children".into(), serde_json::json!(group.children));
            }
            if !group.hosts.is_empty() {
                body.insert("hosts".into(), serde_json::json!(group.hosts));
            }
            if !body.is_empty() {
                out.insert(group.name.clone(), Value::Object(body));
            }
        }
        Value::Object(out)
    }

    /// A group's own variables, as the inventory sources wrote them.
    pub fn group_vars_of(&self, name: &str) -> BTreeMap<String, Value> {
        let Some(group) = self.group(name) else {
            return BTreeMap::new();
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
            // A group that is its own ancestor is printed once, where the walk met it first.
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
