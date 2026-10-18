// SPDX-License-Identifier: GPL-3.0-or-later
//! Building a `VarStore` from an inventory, its settings, and the inventory changes a play makes.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;
use std::sync::Arc;

use serde_json::{Map, Value};

use super::load::load_vars_dir;
use super::{Added, VarStore, holds_template};
use crate::bridge::Bridge;
use crate::inventory::{Inventory, PluginConfig, Source};
use crate::stats::CustomStats;
/// The four maps a store keeps of an inventory: its groups, each host's own variables, the
/// variables written on its line, and the names of the groups it belongs to.
#[expect(
    clippy::type_complexity,
    reason = "one value per map the store keeps, built together and assigned together"
)]
fn inventory_maps(
    inventory: &Inventory,
) -> (
    BTreeMap<String, Vec<String>>,
    BTreeMap<String, Map<String, Value>>,
    BTreeMap<String, Map<String, Value>>,
    BTreeMap<String, Vec<String>>,
) {
    let groups = inventory.groups();
    let mut inventory_vars = BTreeMap::new();
    let mut host_line_vars = BTreeMap::new();
    let mut group_names = BTreeMap::new();
    for host in &groups["all"] {
        // Not `resolve`: a name that is both a group and a host resolves as the group, and
        // an empty group would leave this host without its own inventory variables.
        let resolved = inventory.host_with_vars(host);
        inventory_vars.insert(host.clone(), resolved.vars.into_iter().collect());
        host_line_vars.insert(
            host.clone(),
            inventory
                .host_line_vars(host)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect(),
        );
        // In layering order; the `group_names` magic variable sorts its own copy.
        group_names.insert(host.clone(), inventory.layered_groups_of(host));
    }
    (groups, inventory_vars, host_line_vars, group_names)
}

impl VarStore {
    /// A store over one inventory file, or none.
    #[cfg(test)]
    pub fn new(
        inventory: &Inventory,
        inventory_path: Option<&Path>,
        playbook_dir: &Path,
        extra: Map<String, Value>,
    ) -> anyhow::Result<Self> {
        let sources: Vec<Source> = inventory_path
            .map(|p| Source::new(&p.to_string_lossy()))
            .into_iter()
            .collect();
        Self::with_sources(inventory, &sources, playbook_dir, extra)
    }

    /// A store over the inventory `sources` made. Each source's directory gives its own
    /// `group_vars` and `host_vars`, a later source's winning, and the playbook's win over all
    /// of them within the same kind of file, as the reference ranks them.
    pub fn with_sources(
        inventory: &Inventory,
        sources: &[Source],
        playbook_dir: &Path,
        extra: Map<String, Value>,
    ) -> anyhow::Result<Self> {
        let mut group_files = Vec::new();
        let mut host_files = Vec::new();
        let roots = sources.iter().filter_map(Source::vars_dir);
        for root in roots.chain([playbook_dir.to_path_buf()]) {
            group_files.push(load_vars_dir(&root.join("group_vars"))?);
            host_files.push(load_vars_dir(&root.join("host_vars"))?);
        }
        // The first file source, for a host no file source added (`add_host`'s).
        let inventory_file = sources
            .iter()
            .find(|s| s.path().is_file())
            .map(|s| s.path().to_path_buf());
        let (groups, inventory_vars, host_line_vars, group_names) = inventory_maps(inventory);
        Ok(Self {
            inventory_dir: inventory_file
                .as_deref()
                .and_then(Path::parent)
                .map(Path::to_path_buf),
            inventory_file,
            playbook_dir: playbook_dir.to_path_buf(),
            group_files,
            host_files,
            inventory_vars,
            host_line_vars,
            group_names,
            groups,
            facts: BTreeMap::new(),
            gathered: BTreeMap::new(),
            warned_restricted: BTreeSet::new(),
            persisted: BTreeMap::new(),
            fact_cache: None,
            smart: false,
            cache_failure: None,
            untrusted: inventory.untrusted_vars(),
            extra,
            forks: crate::config::DEFAULT_FORKS,
            run_tags: vec!["all".to_string()],
            skip_tags: Vec::new(),
            check_mode: false,
            diff_mode: false,
            hostvars: None,
            stale: BTreeSet::new(),
            host_facts: BTreeMap::new(),
            #[cfg(test)]
            facts_built: BTreeMap::new(),
            #[cfg(test)]
            views_built: BTreeMap::new(),
            hostvars_with: BTreeMap::new(),
            untrusted_hosts: None,
            shared: None,
            templated_facts: false,
            dynamic: inventory.clone(),
            added: Vec::new(),
            reload: None,
            limit: None,
            play_hosts_all: None,
            inventory_changed: false,
            custom_stats: CustomStats::default(),
        })
    }

    /// Every variable map the inventory, its `group_vars` and `host_vars` and the command line
    /// define, for every host at once: what the executor scans for a name whose value reads
    /// another host.
    pub fn static_maps(&self) -> impl Iterator<Item = &Map<String, Value>> {
        self.group_files
            .iter()
            .chain(&self.host_files)
            .flat_map(BTreeMap::values)
            .chain(self.inventory_vars.values())
            .chain(self.host_line_vars.values())
            .chain([&self.extra])
    }

    /// Keeps the sources `meta: refresh_inventory` reads again.
    pub fn set_reload(
        &mut self,
        sources: Vec<Source>,
        config: PluginConfig,
        bridge: Arc<Bridge>,
        limit: Option<HashSet<String>>,
    ) {
        self.reload = Some((sources, config, bridge));
        self.limit = limit;
    }

    /// After a refresh, `ansible_play_hosts_all` lists `hosts`: new hosts show at once, removed
    /// ones are gone, and none of them changes who runs the rest of the play.
    pub fn set_play_hosts_all(&mut self, mut hosts: Vec<String>) {
        if let Some(limit) = &self.limit {
            hosts.retain(|h| limit.contains(h));
        }
        self.play_hosts_all = Some(hosts);
        self.shared = None;
    }

    /// The play is over: `ansible_play_hosts_all` is the next play's own list again.
    pub fn end_play(&mut self) {
        if self.play_hosts_all.take().is_some() {
            self.shared = None;
        }
    }

    /// The sources and plugin settings a refresh reads again, when there are some.
    pub fn reload(&self) -> Option<(Vec<Source>, PluginConfig, Arc<Bridge>)> {
        self.reload.clone()
    }

    /// Replaces what the store knows of the inventory with `inventory`'s groups, hosts and
    /// variables, as `meta: refresh_inventory` does mid-play, and puts back the hosts and groups
    /// `add_host` and `group_by` made. The facts hosts gathered and the facts the playbook set
    /// stay where they are: the reference's refresh re-reads its sources and touches neither.
    /// Returns the inventory the store now holds.
    pub fn refresh_inventory(&mut self, inventory: &Inventory) -> Inventory {
        self.dynamic = inventory.clone();
        for change in &self.added {
            match change {
                Added::Host(name, vars) => self.dynamic.add_host(name, vars.clone()),
                Added::Member(host, group) => self.dynamic.add_member(host, group),
                Added::Child(parent, child) => self.dynamic.add_child(parent, child),
            }
        }
        (
            self.groups,
            self.inventory_vars,
            self.host_line_vars,
            self.group_names,
        ) = inventory_maps(&self.dynamic);
        self.inventory_changed = false;
        self.forget_hostvars();
        self.shared = None;
        self.dynamic.clone()
    }

    /// `add_host`: the host joins the inventory with `vars` on its line and the groups named,
    /// or, when it is already there, gains them. Visible to `hostvars` and `groups` at once;
    /// the hosts of the play being run are not changed by it.
    pub fn add_host(&mut self, name: &str, groups: &[String], vars: &Map<String, Value>) {
        // A value that still holds a template after the task rendered it was built from text the
        // playbook did not write (a fact, a registered result): it is data, kept as an untrusted
        // fact of the host and never rendered again. The rest sits on the host's inventory line.
        let (data, line): (Vec<_>, Vec<_>) = vars.iter().partition(|(_, v)| holds_template(v));
        let line: BTreeMap<String, Value> = line
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        self.dynamic.add_host(name, line.clone());
        self.added.push(Added::Host(name.to_string(), line));
        for group in groups.iter().filter(|g| *g != "all") {
            self.dynamic.add_member(name, group);
            self.added
                .push(Added::Member(name.to_string(), group.clone()));
        }
        self.inventory_moved();
        for (key, value) in data {
            self.set_untrusted_fact(name, key, value.clone());
        }
    }

    /// `group_by`: `host` joins `group`, which is made a child of each of `parents`.
    pub fn add_to_group(&mut self, host: &str, group: &str, parents: &[String]) {
        if group != "all" {
            for parent in parents.iter().filter(|p| *p != "all") {
                self.dynamic.add_child(parent, group);
                self.added
                    .push(Added::Child(parent.clone(), group.to_string()));
            }
            self.dynamic.add_member(host, group);
            self.added
                .push(Added::Member(host.to_string(), group.to_string()));
        }
        self.inventory_moved();
    }

    fn inventory_moved(&mut self) {
        (
            self.groups,
            self.inventory_vars,
            self.host_line_vars,
            self.group_names,
        ) = inventory_maps(&self.dynamic);
        self.inventory_changed = true;
        self.forget_hostvars();
        self.shared = None;
    }

    /// The inventory after the additions of the play that just ended, once; `None` when nothing
    /// was added since it was last taken.
    pub fn take_inventory_changes(&mut self) -> Option<Inventory> {
        std::mem::take(&mut self.inventory_changed).then(|| self.dynamic.clone())
    }

    /// Records one `set_stats` result of `host`.
    pub fn record_custom_stats(&mut self, host: &str, stats: &Value) {
        self.custom_stats.record(host, stats);
    }

    pub fn custom_stats(&self) -> &CustomStats {
        &self.custom_stats
    }

    /// Forgets the facts modules gathered for `hosts`, which is what `meta: clear_facts` clears.
    /// The facts `set_fact` wrote stay: measured on ansible-core 2.19.12, `ansible_facts` is empty
    /// afterwards and a fact set by hand still reads back.
    pub fn clear_gathered(&mut self, hosts: &[String]) {
        for host in hosts {
            self.gathered.remove(host);
            self.host_facts.remove(host);
            self.forget_host(host);
        }
    }

    /// What `ansible_forks` reports for this run.
    pub fn set_forks(&mut self, forks: usize) {
        self.forks = forks;
        self.forget_hostvars();
    }

    /// What `ansible_check_mode` and `ansible_diff_mode` report for this run.
    pub fn set_modes(&mut self, check_mode: bool, diff_mode: bool) {
        self.check_mode = check_mode;
        self.diff_mode = diff_mode;
        self.forget_hostvars();
    }

    /// What `ansible_run_tags` and `ansible_skip_tags` report for this run. Measured on
    /// ansible-core 2.19.12: `['all']` and `[]` with no tag option, `['kubeconfig']` under
    /// `--tags kubeconfig`, `['foo']` as the skip list under `--skip-tags foo`.
    pub fn set_tags(&mut self, run: &[String], skip: &[String]) {
        self.run_tags = run.to_vec();
        self.skip_tags = skip.to_vec();
        self.forget_hostvars();
    }

    pub fn playbook_dir(&self) -> &Path {
        &self.playbook_dir
    }

    /// Moves the playbook-side roots to another playbook's directory, keeping the facts hosts
    /// have gathered so far. Inventory-side sources do not move.
    pub fn rebase(&mut self, playbook_dir: &Path) -> anyhow::Result<()> {
        let last = self.group_files.len() - 1;
        self.group_files[last] = load_vars_dir(&playbook_dir.join("group_vars"))?;
        self.host_files[last] = load_vars_dir(&playbook_dir.join("host_vars"))?;
        self.playbook_dir = playbook_dir.to_path_buf();
        self.forget_hostvars();
        Ok(())
    }
}
