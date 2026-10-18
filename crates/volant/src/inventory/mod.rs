// SPDX-License-Identifier: GPL-3.0-or-later
//! Inventories, read from every source the way ansible-core 2.19.12's inventory manager reads
//! them: each source offered to the enabled plugins in turn, directories walked, several
//! sources merged into one inventory.

mod auto;
mod collection;
mod constructed;
mod generator;
mod host_list;
mod ini;
pub mod ranges;
mod script;
mod sources;
mod toml;
mod yaml;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::Value;

pub use sources::{
    InventoryPlugin, Loaded, Note, ParseCx, PluginConfig, PluginError, Source, load_sources,
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
    pub vars: BTreeMap<String, Value>,
}

#[derive(Debug, Default, Clone)]
struct Group {
    name: String,
    hosts: Vec<String>,
    children: Vec<String>,
    vars: BTreeMap<String, Value>,
    /// `ansible_group_priority`, which the reference keeps out of the group's variables. A
    /// group without one ranks at 1.
    priority: Option<i64>,
}

/// A computed value as `--list` prints it: every string in it wrapped, mapping keys left alone.
fn mark_unsafe(value: &mut Value, computed: &HashSet<String>) {
    match value {
        Value::String(text) if computed.contains(text.as_str()) => {
            *value = serde_json::json!({ "__ansible_unsafe": text });
        }
        Value::Array(items) => items.iter_mut().for_each(|v| mark_unsafe(v, computed)),
        Value::Object(map) => map.values_mut().for_each(|v| mark_unsafe(v, computed)),
        _ => {}
    }
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
}

#[derive(Debug, Default)]
pub struct Resolution {
    pub hosts: Vec<Host>,
    pub unmatched: Vec<String>,
    pub warnings: Vec<String>,
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
                    Note::Verbose(..) | Note::Deprecation(_) => None,
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
        vars: BTreeMap<String, Value>,
        file: Option<&Path>,
    ) -> bool {
        if let Some(host) = self.hosts.iter_mut().find(|h| h.name == name) {
            self.unsafe_vars
                .retain(|(h, v), _| h != name || !vars.contains_key(v));
            host.vars.extend(vars);
            return false;
        }
        let mut all = BTreeMap::new();
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

    /// What `ansible-inventory --list` prints: every host's variables with its groups' folded
    /// in, `all`'s top groups, and each group's own hosts and children. An empty group has no
    /// entry of its own.
    pub fn dump(&self) -> Value {
        let mut out = serde_json::Map::new();
        let hostvars: serde_json::Map<String, Value> = self
            .hosts
            .iter()
            .map(|h| (h.name.clone(), self.host_with_vars(&h.name).vars))
            .map(|(name, mut vars)| {
                for ((host, var), computed) in &self.unsafe_vars {
                    if *host == name
                        && let Some(value) = vars.get_mut(var)
                    {
                        mark_unsafe(value, computed);
                    }
                }
                (name, vars)
            })
            .filter(|(_, vars)| !vars.is_empty())
            .map(|(name, vars)| {
                (
                    name,
                    show_unsafe(&Value::Object(vars.into_iter().collect())),
                )
            })
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

    /// An inventory holding the groups `groups` lists, in that order, and `all`'s hosts in the
    /// order it gives them: what `lookup('inventory_hostnames')` matches a pattern against.
    pub fn from_groups(groups: &BTreeMap<String, Vec<String>>) -> Self {
        let mut inv = Inventory::default();
        for name in groups.get("all").into_iter().flatten() {
            inv.add_host(name, BTreeMap::new());
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
        for name in self
            .hosts
            .iter()
            .map(|h| h.name.as_str())
            .filter(|n| self.groups.iter().any(|g| g.name == *n))
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
        for name in regular {
            for h in self.matching(name, &mut res) {
                if !selected.contains(&h) {
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
        for name in selected {
            res.hosts
                .push(if self.hosts.iter().any(|h| h.name == name) {
                    self.host_with_vars(&name)
                } else {
                    implicit_localhost(&name)
                });
        }
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
        let regex = term
            .strip_prefix('~')
            .map(|r| Regex::new(&format!("^(?:{r})")));
        if regex.is_some() || term.contains(['*', '?', '[']) {
            let matches = |name: &str| match &regex {
                Some(Ok(re)) => re.is_match(name),
                Some(Err(_)) => false,
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
            return vec![term.to_string()];
        }
        res.unmatched.push(term.to_string());
        Vec::new()
    }

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

    fn group_mut(&mut self, name: &str) -> &mut Group {
        if let Some(pos) = self.groups.iter().position(|g| g.name == name) {
            return &mut self.groups[pos];
        }
        self.groups.push(Group {
            name: name.to_string(),
            ..Group::default()
        });
        self.groups.last_mut().expect("just pushed")
    }

    fn group(&self, name: &str) -> Option<&Group> {
        self.groups.iter().find(|g| g.name == name)
    }

    /// Hosts of a group and of its descendants, in the order `ansible-inventory` produces: the
    /// group's own hosts first, then each descendant group's own hosts, breadth-first over
    /// `:children`, without duplicates. This is not the inventory's global host order — a group
    /// nested two levels down can list hosts declared earlier in the file than a shallower
    /// sibling, and Ansible's traversal order (not the file's) is what wins.
    fn group_hosts(&self, group: &Group) -> Vec<String> {
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
    fn all_hosts(&self) -> Vec<String> {
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
    fn hosts_breadth_first(
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

    /// Group vars from the outermost group to the innermost, then host vars on top.
    pub fn host_with_vars(&self, name: &str) -> Host {
        let host = self
            .hosts
            .iter()
            .find(|h| h.name == name)
            .cloned()
            .unwrap_or_default();
        let mut vars = BTreeMap::new();
        for g in self.sorted_groups_of(name) {
            vars.extend(g.vars.clone());
        }
        vars.extend(host.vars);
        Host {
            name: host.name,
            vars,
        }
    }

    /// The groups holding `host`, `all` included, in the order the reference layers their
    /// variables (`sort_groups`): shallower first, then lower `ansible_group_priority`, then by
    /// name, so the last one wins.
    fn sorted_groups_of(&self, host: &str) -> Vec<&Group> {
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

    /// The variables set on the host line itself, before any group is merged in.
    pub fn host_line_vars(&self, name: &str) -> Option<&BTreeMap<String, Value>> {
        self.hosts.iter().find(|h| h.name == name).map(|h| &h.vars)
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

/// Splits a pattern on `,` and `:` the way Ansible's own pattern splitter does: not inside a
/// `[...]` subscript, so `web[0:1]` stays one term instead of being torn into `web[0` and `1]` at
/// the colon that is part of its range, not a term separator. Bracket mode is only entered when
/// the very next `]` closes *this* `[` before another `[` opens: an unbalanced `[` (`web[1:db`,
/// no `]` at all) or one whose only later `]` actually belongs to a subsequent bracket
/// (`a[,b[1]`, where the first `[` would otherwise borrow the second one's close and swallow both
/// terms into one) now just fails to match on its own, instead of corrupting every later term.
fn split_terms(pattern: &str) -> Vec<&str> {
    let mut terms = Vec::new();
    let mut start = 0;
    let mut in_bracket = false;
    for (i, c) in pattern.char_indices() {
        match c {
            '[' if !in_bracket => {
                let rest = &pattern[i + 1..];
                let closes_before_reopening = match (rest.find(']'), rest.find('[')) {
                    (Some(close), Some(open)) => close < open,
                    (Some(_), None) => true,
                    (None, _) => false,
                };
                if closes_before_reopening {
                    in_bracket = true;
                }
            }
            ']' if in_bracket => in_bracket = false,
            ',' | ':' if !in_bracket => {
                terms.push(&pattern[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    terms.push(&pattern[start..]);
    terms
}

/// The three names ansible-core 2.19.12 conjures a host for when the inventory has none of that
/// name. Measured against the reference: `hosts: ::1` with no `::1` in the inventory runs
/// `changed: [::1]` on the controller, like `localhost` and `127.0.0.1`. One copy of the rule,
/// read both here and by the store that gives such a host its local connection.
/// A value as `ansible-inventory --list` prints it: text that is data, never rendered, as
/// `{"__ansible_unsafe": text}`.
fn show_unsafe(value: &Value) -> Value {
    match value {
        Value::String(text) => crate::yaml::unsafe_text(text).map_or_else(
            || value.clone(),
            |text| serde_json::json!({ "__ansible_unsafe": text }),
        ),
        Value::Array(items) => Value::Array(items.iter().map(show_unsafe).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), show_unsafe(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

pub fn is_localhost(name: &str) -> bool {
    matches!(name, "localhost" | "127.0.0.1" | "::1")
}

fn implicit_localhost(name: &str) -> Host {
    let mut vars = BTreeMap::new();
    vars.insert(
        "ansible_connection".to_string(),
        Value::String("local".into()),
    );
    Host {
        name: name.to_string(),
        vars,
    }
}

/// `fnmatch` as Python does it for host patterns: `*` any run, `?` one character, `[abc]` and
/// `[a-z]` sets, `[!abc]` negated sets. No escaping, like Ansible. Ansible only reaches this for
/// bracket content that isn't a bare `[N]`/`[N:M]` subscript; see `split_subscript`.
///
/// Iterative, not recursive: a left-to-right scan that remembers only the most recent `*` and
/// where it last resumed matching from (the standard glob-matching algorithm), so the cost is
/// `O(pattern * text)`. The previous recursive form tried every split point of every `*` and
/// revisited the same `(pattern, text)` position through more than one path, which made a pattern
/// with many stars (`*a*a*a*a*a*a*a*a*a*a*b` against a 40-character miss) cost time exponential in
/// the number of stars — a hang, not an error, on an ordinary `--limit` pattern.
///
/// Measured against CPython's own `fnmatch.fnmatchcase` (`ansible-core` 2.19.12's bundled
/// 3.13.15), contrary to this function's own earlier doc comment and unit test: an unclosed `[`,
/// an empty `[]`, and a bare `[!]` are not "matches nothing" — they are literal text, because
/// CPython's `fnmatch.translate` falls back to a literal `[` (and re-scans the rest of the pattern
/// normally) whenever no closing `]` can be found. `parse_class` below reproduces that closing-`]`
/// search exactly, including its one subtlety: a `]` immediately after `[` or after `[!` counts as
/// a literal first set member, not the terminator, which is also why an empty `[]` and a bare
/// `[!]` fail to find a real close and fall back to literal text.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    // The pattern index of the most recent unconsumed `*`, and the text index it last resumed
    // matching from — `None` until the first `*` is seen, at which point backtracking on a
    // mismatch means "let that `*` eat one more character" instead of failing outright.
    let mut backtrack: Option<(usize, usize)> = None;
    while ti < t.len() {
        if p.get(pi) == Some(&'*') {
            backtrack = Some((pi, ti));
            pi += 1;
            continue;
        }
        if pi < p.len() {
            let (hit, consumed) = atom_matches(&p, pi, t[ti]);
            if hit {
                pi += consumed;
                ti += 1;
                continue;
            }
        }
        match backtrack {
            Some((star, resumed_from)) => {
                ti = resumed_from + 1;
                pi = star + 1;
                backtrack = Some((star, ti));
            }
            None => return false,
        }
    }
    while p.get(pi) == Some(&'*') {
        pi += 1;
    }
    pi == p.len()
}

/// Whether the pattern atom at `p[i]` (never `*`, the caller handles that separately) matches
/// `c`, and how many pattern characters that atom occupies: `1` for a literal or `?`, or the
/// whole bracket for a `[...]` class that actually closes, falling back to a single literal `[`
/// otherwise.
fn atom_matches(p: &[char], i: usize, c: char) -> (bool, usize) {
    match p[i] {
        '?' => (true, 1),
        '[' => match parse_class(p, i + 1) {
            Some((negate, set, end)) => (class_hit(set, c) != negate, end - i),
            None => (c == '[', 1),
        },
        literal => (c == literal, 1),
    }
}

/// A `[...]` class body starting right after the `[`, following CPython's `fnmatch.translate`:
/// a leading `!` negates, and the `]` immediately after that (or after a plain `[`) is a literal
/// first set member rather than the closing delimiter — the rule that makes an empty `[]` and a
/// bare `[!]` never find a close and fall back to literal text. Returns the negation flag, the
/// raw set body, and the pattern index right after the closing `]`; `None` when no `]` closes it.
fn parse_class(p: &[char], start: usize) -> Option<(bool, &[char], usize)> {
    let (negate, mut j) = match p.get(start) {
        Some('!') => (true, start + 1),
        _ => (false, start),
    };
    let body_start = j;
    if p.get(j) == Some(&']') {
        j += 1;
    }
    while j < p.len() && p[j] != ']' {
        j += 1;
    }
    if j >= p.len() {
        return None;
    }
    Some((negate, &p[body_start..j], j + 1))
}

/// Ceiling, measured against CPython's `fnmatch.fnmatchcase` over 400,000 random cases: a
/// reversed range such as `c-!` never matches anything here, while CPython's `fnmatch.translate`
/// merges it into no constraint at all, so `[?-*c-!!]?*` matches `b[b` there but not here.
fn class_hit(set: &[char], c: char) -> bool {
    let mut hit = false;
    let mut i = 0;
    while i < set.len() {
        if i + 2 < set.len() && set[i + 1] == '-' {
            hit |= set[i] <= c && c <= set[i + 2];
            i += 3;
        } else {
            hit |= set[i] == c;
            i += 1;
        }
    }
    hit
}

/// A single index (`[N]`, `N` may be negative) or an inclusive range (`[N:M]` or the deprecated
/// `[N-M]`, `N` and `M` plain digits, `M` optional meaning "to the end"). Measured against
/// `ansible-core`'s `PATTERN_WITH_SUBSCRIPT` regex: the range's own bounds are never negative in
/// text, only a lone index may be; anything else with a `[` (letters, a bare `:` with no leading
/// digit, `~` regexes) is not a subscript and falls through to `glob_match` instead, exactly as
/// the reference falls through to fnmatch for the same inputs.
enum Subscript {
    Index(i64),
    Range(i64, Option<i64>),
}

fn split_subscript(term: &str) -> Option<(&str, Subscript)> {
    if !term.ends_with(']') {
        return None;
    }
    let open = term.rfind('[')?;
    if open == 0 {
        return None;
    }
    let base = &term[..open];
    let inner = &term[open + 1..term.len() - 1];
    let is_signed_digits = |s: &str| {
        let s = s.strip_prefix('-').unwrap_or(s);
        !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
    };
    if is_signed_digits(inner) {
        return Some((base, Subscript::Index(inner.parse().ok()?)));
    }
    let sep = inner.find([':', '-'])?;
    let (start, end) = (&inner[..sep], &inner[sep + 1..]);
    let is_digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    if !is_digits(start) || (!end.is_empty() && !is_digits(end)) {
        return None;
    }
    let end = if end.is_empty() {
        None
    } else {
        Some(end.parse().ok()?)
    };
    Some((base, Subscript::Range(start.parse().ok()?, end)))
}

/// Ansible's `_apply_subscript`: a Python-style negative index wraps from the end; a range's
/// missing end means "to the last host" and, unlike a Rust or Python range, its given end is
/// inclusive. Anything out of bounds is empty, not an error — the reference does not fail a
/// playbook run over an over-large subscript, it just selects nothing.
fn apply_subscript(names: &[String], sub: Subscript) -> Vec<String> {
    let len = names.len() as i64;
    let wrap = |i: i64| if i < 0 { i + len } else { i };
    match sub {
        Subscript::Index(i) => {
            let i = wrap(i);
            if (0..len).contains(&i) {
                vec![names[i as usize].clone()]
            } else {
                Vec::new()
            }
        }
        Subscript::Range(start, end) => {
            let start = start.clamp(0, len);
            let stop = (end.unwrap_or(len - 1) + 1).clamp(0, len);
            if start >= stop {
                Vec::new()
            } else {
                names[start as usize..stop as usize].to_vec()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use serde_json::json;

    const SAMPLE: &str = r#"
# hosts without a group land in "ungrouped"
lonely ansible_connection=local

[web]
web1 ansible_host=10.0.0.1 role="front end"
web2

[db]
db1

[web:vars]
http_port=80
tier=web

[prod:children]
web
db

[prod:vars]
tier=prod
env=prod
"#;

    fn names(hosts: &[Host]) -> Vec<&str> {
        hosts.iter().map(|h| h.name.as_str()).collect()
    }

    const ALL_VARS: &str = "[all:vars]\nx=1\ny=1\n\n[web]\nweb1\n\n[web:vars]\nx=2\n";
    // Both groups hold web1 at depth 1: "zeta" is applied after "alpha", so it wins.
    const EQUAL_DEPTH: &str =
        "[zeta]\nweb1\n[alpha]\nweb1\n[zeta:vars]\nk=zeta\n[alpha:vars]\nk=alpha\n";

    /// A host's variable comes from the layer that wins: its own line, the deepest group, then
    /// the shallower ones, `all` last, equal depths in alphabetical order.
    #[rstest]
    #[case::own_line(SAMPLE, "web1", "ansible_host", json!("10.0.0.1"))]
    #[case::quoted_value(SAMPLE, "web1", "role", json!("front end"))]
    #[case::group_var(SAMPLE, "web1", "http_port", json!(80))]
    #[case::group_var_second_host(SAMPLE, "web2", "http_port", json!(80))]
    #[case::child_wins_over_parent(SAMPLE, "web1", "tier", json!("web"))]
    #[case::parent_reaches_child(SAMPLE, "web1", "env", json!("prod"))]
    #[case::parent_alone(SAMPLE, "db1", "tier", json!("prod"))]
    #[case::all_loses_to_a_group(ALL_VARS, "web1", "x", json!(2))]
    #[case::all_reaches_every_host(ALL_VARS, "web1", "y", json!(1))]
    #[case::equal_depth_alphabetical(EQUAL_DEPTH, "web1", "k", json!("zeta"))]
    #[case::defined_localhost_wins("localhost ansible_connection=ssh\n", "localhost", "ansible_connection", json!("ssh"))]
    fn a_host_variable_comes_from_the_layer_that_wins(
        #[case] ini: &str,
        #[case] host: &str,
        #[case] key: &str,
        #[case] want: Value,
    ) {
        let inv = Inventory::parse_ini(ini).unwrap();
        let found = inv.resolve(host);
        assert_eq!(found.hosts[0].vars[key], want, "{host}.{key}");
    }

    /// The pattern language: groups, unions, exclusions, intersections, wildcards over hosts and
    /// groups, subscripts, in inventory order.
    #[rstest]
    #[case::all("all", &["lonely", "web1", "web2", "db1"])]
    #[case::group("prod", &["web1", "web2", "db1"])]
    #[case::comma_union("db,web1", &["db1", "web1"])]
    #[case::colon_union("web:db", &["web1", "web2", "db1"])]
    #[case::unknown_term_is_skipped("nope,web2", &["web2"])]
    #[case::exclusion("all:!web", &["lonely", "db1"])]
    #[case::intersection("prod:&web", &["web1", "web2"])]
    #[case::intersection_then_exclusion("prod:&web:!web2", &["web1"])]
    #[case::bare_exclusion_starts_from_all("!web", &["lonely", "db1"])]
    #[case::empty("", &[])]
    #[case::comma_only(",", &[])]
    #[case::colon_only(":", &[])]
    #[case::host_wildcard("w*b1", &["web1"])]
    #[case::group_wildcard("pro*", &["web1", "web2", "db1"])]
    #[case::regex_host("~w.*1", &["web1"])]
    #[case::regex_group("~pro", &["web1", "web2", "db1"])]
    #[case::regex_is_anchored_at_the_start("~b1", &[])]
    #[case::index("web[0]", &["web1"])]
    #[case::negative_index("web[-1]", &["web2"])]
    #[case::slice("web[0:1]", &["web1", "web2"])]
    #[case::out_of_range_selects_nothing("web[12]", &[])]
    fn a_pattern_selects_in_inventory_order(#[case] pattern: &str, #[case] want: &[&str]) {
        let inv = Inventory::parse_ini(SAMPLE).unwrap();
        assert_eq!(names(&inv.resolve(pattern).hosts), want, "{pattern}");
    }

    /// The controller is an implicit local host outside `all`; `::1` is cut by the pattern
    /// separator, a known divergence (the reference runs it on the controller).
    #[test]
    fn the_controller_is_an_implicit_local_host() {
        let inv = Inventory::empty();
        for name in ["localhost", "127.0.0.1"] {
            let res = inv.resolve(name);
            assert_eq!(res.hosts.len(), 1, "{name} matched nothing");
            assert_eq!(res.hosts[0].name, name);
            assert_eq!(res.hosts[0].vars["ansible_connection"], json!("local"));
            assert!(res.unmatched.is_empty(), "{name}: {:?}", res.unmatched);
        }
        assert!(
            inv.resolve("all").hosts.is_empty(),
            "implicit localhost is not part of all"
        );
        assert_eq!(split_terms("::1"), ["", "", "1"]);
        let res = inv.resolve("::1");
        assert!(res.hosts.is_empty(), "{:?}", names(&res.hosts));
        assert_eq!(res.unmatched, ["1"]);
    }

    #[rstest]
    #[case::unterminated_quote("[web]\nweb1 role=\"unterminated\n", 2)]
    #[case::no_equals_sign("[web:vars]\nno_equals_sign\n", 2)]
    #[case::empty_host_name("[web]\n\"\"\n", 2)]
    #[case::unbalanced_header("[web\nweb1\n", 1)]
    fn a_syntax_error_carries_its_line_number(#[case] ini: &str, #[case] line: usize) {
        assert_eq!(Inventory::parse_ini(ini).unwrap_err().line, line);
    }

    /// `ok` is the ceiling an int still parses to; the rest are Python literals when they parse
    /// as such and text when they do not.
    #[rstest]
    #[case::i64_max("ok=9223372036854775807", "ok", json!(i64::MAX))]
    #[case::past_i64("huge=9223372036854775808", "huge", json!("9223372036854775808"))]
    #[case::int("port=8080", "port", json!(8080))]
    #[case::float("ratio=0.5", "ratio", json!(0.5))]
    #[case::boolean("flag=True", "flag", json!(true))]
    #[case::none("nothing=None", "nothing", json!(null))]
    #[case::quoted("name='quoted text'", "name", json!("quoted text"))]
    #[case::yes_is_text("word=yes", "word", json!("yes"))]
    #[case::list("list=[1,2]", "list", json!([1, 2]))]
    #[case::first_equals_splits("raw=a=b", "raw", json!("a=b"))]
    fn an_ini_value_is_a_python_literal_when_it_parses_as_one(
        #[case] words: &str,
        #[case] key: &str,
        #[case] want: Value,
    ) {
        let inv = Inventory::parse_ini(&format!("h {words}\n")).unwrap();
        assert_eq!(inv.resolve("h").hosts[0].vars[key], want, "{words}");
    }

    /// Groups are listed; a circular group resolves, and a host inside the cycle reaches `all`.
    #[test]
    fn groups_are_listed_and_cycles_terminate() {
        let inv = Inventory::parse_ini(SAMPLE).unwrap();
        assert_eq!(inv.group_names_of("web1"), ["prod", "web"]);
        let groups = inv.groups();
        assert_eq!(groups["web"], ["web1", "web2"]);
        assert_eq!(groups["prod"], ["web1", "web2", "db1"]);
        assert_eq!(groups["all"], ["lonely", "web1", "web2", "db1"]);
        assert_eq!(groups["ungrouped"], ["lonely"]);

        let inv = Inventory::parse_ini("[a:children]\nb\n\n[b:children]\na\n").unwrap();
        let res = inv.resolve("a");
        assert!(res.hosts.is_empty() && res.unmatched.is_empty());
        let inv =
            Inventory::parse_ini("[a:children]\nb\n\n[b:children]\na\n\n[a]\ntrapped\n").unwrap();
        assert_eq!(names(&inv.resolve("all").hosts), ["trapped"]);
    }

    /// Unknown terms are reported, not fatal; a host sharing a group's name means the host and
    /// warns.
    #[test]
    fn unmatched_terms_and_homonyms_are_reported() {
        let inv = Inventory::parse_ini(SAMPLE).unwrap();
        let res = inv.resolve("nope,web2");
        assert_eq!(res.unmatched, ["nope"]);
        let inv = Inventory::parse_ini("[same]\nsame\nother\n").unwrap();
        let res = inv.resolve("same");
        assert_eq!(names(&res.hosts), ["same"]);
        assert_eq!(res.warnings.len(), 1);
    }

    #[test]
    fn glob_matching_follows_fnmatch() {
        assert!(
            glob_match("web*", "web12")
                && glob_match("web?", "web1")
                && !glob_match("web?", "web12")
        );
        assert!(glob_match("web[12]", "web2") && !glob_match("web[12]", "web3"));
        assert!(glob_match("web[!1]", "web2") && !glob_match("web[!1]", "web1"));
        assert!(glob_match("w[a-z]b1", "web1") && !glob_match("w[a-z]b1", "w3b1"));
        // Measured against CPython's fnmatch.fnmatchcase (ansible-core 2.19.12's bundled
        // 3.13.15): an unclosed `[`, an empty `[]`, and a bare `[!]` are all literal text.
        assert!(glob_match("web[", "web["));
        assert!(glob_match("a[]", "a[]") && !glob_match("a[]", "a"));
        assert!(
            glob_match("a[!]", "a[!]") && !glob_match("a[!]", "a!") && !glob_match("a[!]", "a]")
        );
        // Ten stars and a miss: the scan is O(pattern * text), not a recursion per split point.
        assert!(!glob_match("*a*a*a*a*a*a*a*a*a*a*b", &"a".repeat(40)));
    }

    #[test]
    fn split_terms_does_not_swallow_terms_on_an_unbalanced_bracket() {
        // No closing `]` anywhere: the `:` still separates `db` into its own term.
        assert_eq!(split_terms("web[1:db"), ["web[1", "db"]);
        // The first `[` must not borrow the later bracket's close.
        assert_eq!(split_terms("a[,b[1]"), ["a[", "b[1]"]);
        // A real subscript keeps the `:` inside the term.
        assert_eq!(split_terms("web[0:1]"), ["web[0:1]"]);
    }

    /// The reference's own answer for a value ambiguous in Rust, taken from the inventory golden
    /// rather than retyped, so the two never drift apart.
    #[test]
    fn ini_integers_follow_pythons_literal_grammar() {
        let expected: Value =
            serde_json::from_str(include_str!("../../tests/golden/expected_inventory.json"))
                .unwrap();
        let web1 = &expected["hostvars"]["web1"];
        let inv = Inventory::load(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/golden/inventory.ini")
                .as_path(),
        )
        .unwrap();
        let vars = inv.host_with_vars("web1").vars;
        assert_eq!(
            vars["leading"], web1["leading"],
            "a leading zero stays text"
        );
        assert_eq!(
            vars["grouped"], web1["grouped"],
            "an underscore digit separator still parses"
        );
    }
}
