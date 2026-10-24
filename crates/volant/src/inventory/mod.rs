// SPDX-License-Identifier: GPL-3.0-or-later
//! Inventories, read from every source the way ansible-core 2.19.12's inventory manager reads
//! them: each source offered to the enabled plugins in turn, directories walked, several
//! sources merged into one inventory.

mod auto;
mod collection;
mod constructed;
mod generator;
mod graph;
mod host_list;
mod ini;
mod listed;
mod patterns;
pub mod ranges;
mod script;
mod sources;
mod toml;
mod toml_write;
mod unsafe_text;
mod yaml;

pub use listed::{GraphItem, VarsOf};
pub use patterns::Resolution;
pub use toml_write::dumps as toml_dumps;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::Value;

pub(crate) use patterns::glob_match;
use patterns::{apply_subscript, split_subscript, split_terms};
pub use sources::{
    InventoryPlugin, Note, ParseCx, PluginConfig, PluginError, Source, load_sources,
    load_sources_with,
};

/// Every inventory plugin this release knows, the ones `enable_plugins` picks from.
const PLUGINS: &[&dyn InventoryPlugin] = &[
    &host_list::HostList { advanced: false },
    &host_list::HostList { advanced: true },
    &script::Script,
    &auto::Auto,
    &yaml::YamlInventory,
    &ini::Ini,
    &toml::Toml,
    &constructed::Constructed,
    &generator::Generator,
];

/// The names of the builtin inventory plugins this release reads.
pub fn plugin_names() -> impl Iterator<Item = &'static str> {
    PLUGINS.iter().map(|p| p.name())
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Host {
    pub name: String,
    pub vars: serde_json::Map<String, Value>,
}

#[derive(Debug, Default, Clone)]
struct Group {
    name: String,
    hosts: Vec<String>,
    children: Vec<String>,
    vars: serde_json::Map<String, Value>,
    /// `ansible_group_priority`, which the reference keeps out of the group's variables. A
    /// group without one ranks at 1.
    priority: Option<i64>,
}

#[derive(Debug, Default, Clone)]
pub struct Inventory {
    hosts: Vec<Host>,
    groups: Vec<Group>,
    /// The file each host was first read from, for its `inventory_file` and `inventory_dir`.
    origins: HashMap<String, PathBuf>,
    /// The `(host, variable)` pairs a `constructed` compose built from a computed string, which
    /// `--list` marks `__ansible_unsafe`.
    unsafe_vars: HashMap<(String, String), HashSet<String>>,
    /// A source failed and none parsed: its groups stay, but `all` holds nothing.
    detached: bool,
}

#[derive(Debug)]
pub struct InventoryError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for InventoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for InventoryError {}

impl Inventory {
    #[cfg(test)]
    pub fn empty() -> Self {
        Self::default()
    }

    /// One source read through the default plugin chain, or the reason it could not be.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let loaded = load_sources(
            &[Source::new(&path.to_string_lossy())],
            &PluginConfig::default(),
        );
        if let Some(fatal) = loaded.error {
            anyhow::bail!(fatal.message);
        }
        if !loaded.parsed {
            let reasons: Vec<String> = loaded
                .notes
                .into_iter()
                .filter_map(|n| match n {
                    Note::Warning(text) => Some(text),
                    Note::Verbose(..) | Note::Error(_) | Note::Deprecation(_) => None,
                })
                .collect();
            anyhow::bail!(reasons.join(
                "
"
            ));
        }
        Ok(loaded.inventory)
    }

    /// The variables a run must treat as data, by host: the ones a compose computed.
    pub fn untrusted_vars(&self) -> BTreeMap<String, std::collections::BTreeSet<String>> {
        let mut out: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
        for ((host, var), computed) in &self.unsafe_vars {
            if !computed.is_empty() {
                out.entry(host.clone()).or_default().insert(var.clone());
            }
        }
        out
    }

    /// The file `host` was first read from, when a file source added it.
    pub fn source_of(&self, host: &str) -> Option<&Path> {
        self.origins.get(host).map(PathBuf::as_path)
    }

    /// A host a source names, created the way the reference's `InventoryData.add_host` creates
    /// one, with the port its pattern gave and the file it came from, then given `vars`. Whether
    /// it is new.
    fn add_source_host(
        &mut self,
        name: &str,
        port: Option<i64>,
        vars: serde_json::Map<String, Value>,
        file: Option<&Path>,
    ) -> bool {
        if let Some(host) = self.hosts.iter_mut().find(|h| h.name == name) {
            self.unsafe_vars
                .retain(|(h, v), _| h != name || !vars.contains_key(v));
            host.vars.extend(vars);
            return false;
        }
        let mut all = serde_json::Map::new();
        if let Some(port) = port.filter(|p| *p != 0) {
            all.insert("ansible_port".to_string(), Value::from(port));
        }
        all.extend(vars);
        self.hosts.push(Host {
            name: name.to_string(),
            vars: all,
        });
        if let Some(file) = file {
            self.origins.insert(name.to_string(), file.to_path_buf());
        }
        true
    }

    /// The reference's `reconcile_inventory` for `ungrouped`, once every source is read: a host
    /// some other group holds leaves it, a host no group holds joins it.
    fn reconcile(&mut self) {
        let grouped: HashSet<String> = self
            .groups
            .iter()
            .filter(|g| g.name != "all" && g.name != "ungrouped")
            .flat_map(|g| g.hosts.iter().cloned())
            .collect();
        let alone: Vec<String> = self
            .hosts
            .iter()
            .map(|h| h.name.clone())
            .filter(|h| !grouped.contains(h))
            .collect();
        if let Some(ungrouped) = self.groups.iter_mut().find(|g| g.name == "ungrouped") {
            ungrouped.hosts.retain(|h| !grouped.contains(h));
        }
        for name in alone {
            self.add_member(&name, "ungrouped");
        }
    }

    /// An inventory holding the groups `groups` lists, in that order, and `all`'s hosts in the
    /// order it gives them: what `lookup('inventory_hostnames')` matches a pattern against.
    pub fn from_groups(groups: &BTreeMap<String, Vec<String>>) -> Self {
        let mut inv = Inventory::default();
        for name in groups.get("all").into_iter().flatten() {
            inv.add_host(name, serde_json::Map::new());
        }
        for (name, hosts) in groups {
            inv.group_mut(name).hosts.clone_from(hosts);
        }
        inv
    }

    /// Ansible's host pattern grammar: terms separated by `,` or `:`; `all` and `*`; shell
    /// wildcards (`*`, `?`, `[abc]`) on host and group names; `!term` removes, `&term` keeps the
    /// intersection; `name[N]`/`name[N:M]` indexes or slices the terms it follows (`M` inclusive,
    /// as Ansible's `--limit` slicing is, unlike a Rust range). A name that is both a host and a
    /// group means the host, as Ansible's own pattern evaluator checks hosts before groups.
    /// Ansible warns about such a homonym once per run regardless of the pattern used, because the
    /// check runs at inventory load time, not at pattern-resolution time; `resolve` matches that.
    /// `localhost` is implicit with a local connection when the inventory does not define it.
    ///
    /// Terms are not applied in the order they appear: Ansible's own `order_patterns`
    /// (`inventory/manager.py`) sorts them into plain terms first, then `&` terms, then `!` terms,
    /// regardless of how they were interleaved in the pattern text, and — measured — when there is
    /// no plain term at all (`!web`, `&web` alone) it prepends an implicit `all` so the exclusion
    /// or intersection has something to start from, rather than starting from nothing.
    pub fn resolve(&self, pattern: &str) -> Resolution {
        let mut res = Resolution::default();
        let group_names: HashSet<&str> = self.groups.iter().map(|g| g.name.as_str()).collect();
        for name in self
            .hosts
            .iter()
            .map(|h| h.name.as_str())
            .filter(|n| group_names.contains(n))
        {
            res.warnings
                .push(format!("Found both group and host with same name: {name}"));
        }
        let mut regular: Vec<&str> = Vec::new();
        let mut intersects: Vec<&str> = Vec::new();
        let mut excludes: Vec<&str> = Vec::new();
        for term in split_terms(pattern)
            .into_iter()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            match term.chars().next() {
                Some('!') => excludes.push(term[1..].trim()),
                Some('&') => intersects.push(term[1..].trim()),
                _ => regular.push(term),
            }
        }
        if regular.is_empty() && (!intersects.is_empty() || !excludes.is_empty()) {
            // A bare `!term`/`&term` pattern has nothing to start from but `all`, as Ansible's own
            // `order_patterns` inserts; a genuinely empty pattern (`hosts: ""`) has no term at all
            // and selects nothing, rather than silently meaning the whole inventory.
            regular.push("all");
        }
        let mut selected: Vec<String> = Vec::new();
        let mut chosen: HashSet<String> = HashSet::new();
        for name in regular {
            for h in self.matching(name, &mut res) {
                if chosen.insert(h.clone()) {
                    selected.push(h);
                }
            }
        }
        for name in intersects {
            let keep: HashSet<String> = self.matching(name, &mut res).into_iter().collect();
            selected.retain(|h| keep.contains(h));
        }
        for name in excludes {
            let drop: HashSet<String> = self.matching(name, &mut res).into_iter().collect();
            selected.retain(|h| !drop.contains(h));
        }
        res.hosts = self.hosts_with_vars(&selected);
        res
    }

    /// Hosts named by one term, in inventory order. Records unmatched terms. A name that exactly
    /// matches a host always means that host, checked before groups and before wildcards, exactly
    /// as Ansible's own `_evaluate_patterns` special-cases an exact host name before ever calling
    /// its group-aware matcher.
    fn matching(&self, term: &str, res: &mut Resolution) -> Vec<String> {
        if let Some((base, sub)) = split_subscript(term) {
            let names = self.matching(base, res);
            return apply_subscript(&names, sub);
        }
        if self.hosts.iter().any(|h| h.name == term) {
            return vec![term.to_string()];
        }
        if term == "all" || term == "*" {
            return self.all_hosts();
        }
        if let Some(g) = self.group(term) {
            return self.group_hosts(g);
        }
        // `~regex` is matched from the start of the name (`re.match`), hosts first, then groups.
        let regex = match term
            .strip_prefix('~')
            .map(|r| Regex::new(&format!("^(?:{r})")))
        {
            Some(Err(_)) => {
                res.invalid.get_or_insert_with(|| term.to_string());
                return Vec::new();
            }
            Some(Ok(re)) => Some(re),
            None => None,
        };
        if regex.is_some() || term.contains(['*', '?', '[']) {
            let matches = |name: &str| match &regex {
                Some(re) => re.is_match(name),
                None => glob_match(term, name),
            };
            let mut out: Vec<String> = self
                .hosts
                .iter()
                .map(|h| h.name.clone())
                .filter(|h| matches(h))
                .collect();
            for g in self.groups.iter().filter(|g| matches(&g.name)) {
                for h in self.group_hosts(g) {
                    if !out.contains(&h) {
                        out.push(h);
                    }
                }
            }
            if out.is_empty() {
                res.unmatched.push(term.to_string());
            }
            return out;
        }
        if is_localhost(term) {
            return vec![match self.controller_alias(term) {
                Some(alias) => alias.to_string(),
                None => res.implicit.get_or_insert_with(|| term.to_string()).clone(),
            }];
        }
        res.unmatched.push(term.to_string());
        Vec::new()
    }

    /// The inventory's own host that answers to `name`, when `name` is one of the three spellings
    /// of the controller, the inventory lists no host of that name and it lists one of another.
    /// The reference keeps one controller host and hands it out for every spelling the inventory
    /// does not list, so `hosts: 127.0.0.1` runs on the inventory's `localhost`, the first one
    /// it defines, with its variables.
    pub fn controller_alias(&self, name: &str) -> Option<&str> {
        if !is_localhost(name) || self.hosts.iter().any(|h| h.name == name) {
            return None;
        }
        self.hosts
            .iter()
            .map(|h| h.name.as_str())
            .find(|n| is_localhost(n))
    }

    /// The names `all` selects, in the order `resolve("all")` gives them, without their variables.
    pub fn all_host_names(&self) -> Vec<String> {
        self.all_hosts()
    }

    /// Group vars from the outermost group to the innermost, then host vars on top.
    pub fn host_with_vars(&self, name: &str) -> Host {
        let host = self
            .hosts
            .iter()
            .find(|h| h.name == name)
            .cloned()
            .unwrap_or_default();
        let mut vars = serde_json::Map::new();
        for g in self.sorted_groups_of(name) {
            vars.extend(g.vars.clone());
        }
        vars.extend(host.vars);
        Host {
            name: host.name,
            vars,
        }
    }

    /// The variables set on the host line itself, before any group is merged in.
    pub fn host_line_vars(&self, name: &str) -> Option<&serde_json::Map<String, Value>> {
        self.hosts.iter().find(|h| h.name == name).map(|h| &h.vars)
    }
}

/// The three names ansible-core 2.19.12 conjures a host for when the inventory has none of that
/// name. Measured against the reference: `hosts: ::1` with no `::1` in the inventory runs
/// `changed: [::1]` on the controller, like `localhost` and `127.0.0.1`. One copy of the rule,
/// read both here and by the store that gives such a host its local connection.
pub fn is_localhost(name: &str) -> bool {
    LOCALHOST_NAMES.contains(&name)
}

/// The three spellings of the controller host, in the order the reference lists them.
pub const LOCALHOST_NAMES: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

fn implicit_localhost(name: &str) -> Host {
    let mut vars = serde_json::Map::new();
    vars.insert(
        "ansible_connection".to_string(),
        Value::String("local".into()),
    );
    Host {
        name: name.to_string(),
        vars,
    }
}

#[cfg(test)]
mod tests;
