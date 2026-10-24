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
    ) -> Option<Value> {
        let mut vars = vars_of(name);
        for ((host, var), computed) in &self.unsafe_vars {
            if host == name
                && let Some(value) = vars.get_mut(var)
            {
                mark_unsafe(value, computed);
            }
        }
        (!vars.is_empty()).then(|| show_unsafe(&Value::Object(vars.into_iter().collect())))
    }

    /// What `ansible-inventory --host` prints for a host: its variables, `{}` when it has none.
    pub fn host_dump(
        &self,
        name: &str,
        vars_of: &dyn Fn(&str) -> BTreeMap<String, Value>,
    ) -> Value {
        self.shown_vars(name, vars_of)
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
            .filter_map(|h| Some((h.name.clone(), self.shown_vars(&h.name, vars_of)?)))
            .collect();
        out.insert(
            "_meta".into(),
            serde_json::json!({ "hostvars": hostvars, "profile": "inventory_legacy" }),
        );
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
}
