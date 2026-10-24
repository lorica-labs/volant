// SPDX-License-Identifier: GPL-3.0-or-later
//! What `ansible-inventory` lists for a host or a group, and where each listed variable comes from.

use serde_json::{Map, Value};

use super::VarStore;
use super::load::extend;

/// The layer a variable of `ansible-inventory --list` comes from. A group's inventory variable,
/// a `group_vars` file's and an extra variable are one value for every host that reads them,
/// which the YAML dump writes once under an anchor.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ListedOrigin {
    /// The host's own line or one of its `host_vars` files.
    Host,
    /// A group's variable in the inventory sources; [`crate::inventory::Inventory::group_setting`]
    /// names the group.
    InventoryGroup,
    /// The `group_vars` file of a group, in the files' order.
    GroupFile(usize, String),
    /// `-e`.
    Extra,
}

impl VarStore {
    /// What `ansible-inventory` prints for a host: the inventory-level variables, `group_vars` and
    /// `host_vars` files included, with the extra variables on top.
    pub fn listed_vars(&self, host: &str) -> Map<String, Value> {
        let mut vars = self.host_base(host);
        extend(&mut vars, &self.extra);
        vars
    }

    /// `listed_vars` with its names in the order the reference merges the layers, which the TOML
    /// dump keeps: the inventory's groups (`group_names`, in their merge order), the
    /// `group_vars` files, the host's own line, the `host_vars` files, then what is left. A
    /// name keeps the place of the first layer that sets it.
    pub fn listed_vars_ordered(&self, host: &str, group_names: &[String]) -> Map<String, Value> {
        let vars = self.listed_vars(host);
        let host = self.controller_alias(host).unwrap_or(host);
        let mut order: Vec<String> = group_names.to_vec();
        let mut push = |map: Option<&Map<String, Value>>| {
            order.extend(map.into_iter().flat_map(Map::keys).cloned());
        };
        for files in &self.group_files {
            push(files.get("all"));
        }
        for files in &self.group_files {
            for name in self.group_names.get(host).into_iter().flatten() {
                push(files.get(name.as_str()));
            }
        }
        push(self.host_line_vars.get(host));
        for files in &self.host_files {
            push(files.get(host));
        }
        let mut out = Map::new();
        for name in order {
            if let Some(value) = vars.get(&name) {
                out.entry(name).or_insert_with(|| value.clone());
            }
        }
        for (name, value) in vars {
            out.entry(name).or_insert(value);
        }
        out
    }

    /// A group's own variables for `ansible-inventory --graph --vars`: what the inventory sources
    /// wrote for it, then its `group_vars` files, a later source's winning.
    pub fn listed_group_vars(&self, group: &str, own: &Map<String, Value>) -> Map<String, Value> {
        let mut vars = own.clone();
        for files in &self.group_files {
            if let Some(file) = files.get(group) {
                extend(&mut vars, file);
            }
        }
        vars
    }

    /// What `ansible-inventory --export` prints for a host: what its inventory line and its
    /// `host_vars` files set, without its groups' variables or the extra ones.
    pub fn exported_vars(&self, host: &str) -> Map<String, Value> {
        let host = self.controller_alias(host).unwrap_or(host);
        let mut vars = self.host_line_vars.get(host).cloned().unwrap_or_default();
        for files in &self.host_files {
            if let Some(h) = files.get(host) {
                extend(&mut vars, h);
            }
        }
        vars
    }

    /// Where `listed_vars` takes `name` from for `host`: the layer that writes it last.
    pub fn listed_origin(&self, host: &str, name: &str) -> ListedOrigin {
        if self.extra.contains_key(name) {
            return ListedOrigin::Extra;
        }
        let host = self.controller_alias(host).unwrap_or(host);
        let has = |map: Option<&Map<String, Value>>| map.is_some_and(|m| m.contains_key(name));
        if self.host_files.iter().any(|files| has(files.get(host)))
            || has(self.host_line_vars.get(host))
        {
            return ListedOrigin::Host;
        }
        let names = self
            .group_names
            .get(host)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for (index, files) in self.group_files.iter().enumerate().rev() {
            if let Some(group) = names.iter().rev().find(|g| has(files.get(g.as_str()))) {
                return ListedOrigin::GroupFile(index, group.clone());
            }
        }
        if has(self.inventory_vars.get(host)) {
            return ListedOrigin::InventoryGroup;
        }
        match self
            .group_files
            .iter()
            .rposition(|files| has(files.get("all")))
        {
            Some(index) => ListedOrigin::GroupFile(index, "all".to_string()),
            // No layer sets it: the implicit localhost's own keys, which belong to the host.
            None => ListedOrigin::Host,
        }
    }
}
