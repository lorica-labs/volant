// SPDX-License-Identifier: GPL-3.0-or-later
//! The precedence layers: one host's merged variables and the inventory-wide values every host
//! shares.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Map, Value};

use super::PENDING_PYTHON;
use super::facts::no_facts;
use super::is_data_name;
#[cfg(test)]
use super::load::extend;
use super::load::extend_except;
use super::{Added, HostEntry, HostFacts, HostSources, Scope, VarStore};
use crate::tags::Tagged;
use crate::template::HOSTVARS_KEYS;

/// One host's variables as `layered_for_host` hands them out.
#[derive(Debug)]
pub struct Layers {
    /// Every layer but the two below.
    pub map: Tagged,
    /// The inventory-wide values: `shared_values`.
    pub shared: Arc<Map<String, Value>>,
    /// The gathered facts that answer.
    pub facts: Arc<Map<String, Value>>,
    /// Which of `shared` list a name from a managed host.
    pub data_lists: Arc<BTreeSet<String>>,
    /// The `vars_files` values no render changes, read after `map`: `VarsFiles::plain`.
    pub files: Arc<Tagged>,
}

impl VarStore {
    /// The lists of `shared_values` that now carry a host name or a group name `add_host` or
    /// `group_by` took from data, and the list of `hostvars`' hosts. The read as a whole is data,
    /// where the reference tags each string.
    fn data_lists(&self, scope: &Scope) -> Arc<BTreeSet<String>> {
        if self.data_hosts.is_empty() && self.data_groups.is_empty() {
            return Arc::default();
        }
        let listed = |hosts: &[String]| hosts.iter().any(|h| self.data_hosts.contains(h));
        let all_hosts = self.play_hosts_all(scope);
        let reads = [
            ("groups", true),
            (HOSTVARS_KEYS, !self.data_hosts.is_empty()),
            ("ansible_play_hosts", listed(&scope.play_hosts)),
            ("ansible_play_hosts_all", listed(&all_hosts)),
            ("ansible_play_batch", listed(&scope.batch_hosts)),
            ("play_hosts", listed(&scope.batch_hosts)),
        ];
        Arc::new(
            reads
                .into_iter()
                .filter(|(_, data)| *data)
                .map(|(name, _)| name.to_string())
                .collect(),
        )
    }

    /// The magic variables of `host` that carry a name a managed host sent.
    fn data_magic(&self, host: &str) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.data_hosts.contains(host) {
            names.extend(super::HOST_NAMES);
        }
        let in_data_group = self
            .group_names
            .get(host)
            .is_some_and(|g| g.iter().any(|n| self.data_groups.contains(n)));
        if in_data_group {
            names.push("group_names");
        }
        names
    }

    /// Whether `host` is outside the inventory the run started from: a name nothing declared, or a
    /// host `add_host` made while the playbook ran. The name of such a host can be text a managed
    /// host returned, in `groups` or in a variable, with nothing in the render that read it to say so.
    pub fn is_runtime_host(&self, host: &str) -> bool {
        !self
            .groups
            .get("all")
            .is_some_and(|all| all.iter().any(|name| name == host))
            || self
                .added
                .iter()
                .any(|change| matches!(change, Added::Host(name, _) if name == host))
    }

    /// The merged view for one host, lowest precedence first: a role's `defaults`, inventory
    /// `all`, inventory groups by depth and name, `group_vars/all` (inventory then playbook),
    /// `group_vars/<group>` (inventory then playbook), inventory host vars - an implicit
    /// `localhost`'s `local` connection among them - `host_vars/<host>`
    /// (inventory then playbook), gathered facts, play vars, vars_files, a role's `vars`, task
    /// vars, `include_vars`, `set_fact` and registered values, a role's parameters, extra vars, then the magic
    /// variables.
    ///
    /// The three role layers sit where ansible-core 2.19.12 puts them, each measured against the
    /// layer on either side of it rather than derived from the documented numbering: `defaults`
    /// under an inventory variable, `vars` above the play's `vars:` and under a task's, and a
    /// role parameter above a `set_fact` and under `-e`.
    ///
    /// The inventory-wide magic variables are not in this map: `shared_values` builds them, and
    /// a template reads them from there first, which is the place in the order they had here.
    ///
    /// The engine reads a host through `layered_for_host`; this whole copy is what that one is
    /// held to.
    #[cfg(test)]
    pub fn for_host(&mut self, host: &str, scope: &Scope) -> Map<String, Value> {
        let mut vars = Map::new();
        extend(&mut vars, &scope.role_defaults);
        extend(&mut vars, &self.host_base(host));
        match self.gathered.get(host) {
            Some(gathered) => extend(&mut vars, gathered),
            None => extend(&mut vars, no_facts()),
        }
        extend(&mut vars, &scope.play_vars);
        for file in scope.vars_files.maps() {
            extend(&mut vars, file);
        }
        extend(&mut vars, &scope.role_vars);
        extend(&mut vars, &scope.task_vars);
        if let Some(included) = self.included.get(host) {
            extend(&mut vars, included);
        }
        if let Some(facts) = self.facts.get(host) {
            extend(&mut vars, facts);
        }
        extend(&mut vars, &scope.role_params);
        extend(&mut vars, &self.extra);
        extend(&mut vars, &self.magic(host));
        vars
    }

    /// One host's variables as three layers, which read together exactly like `for_host` laid
    /// under `shared_values`: the inventory-wide values, then the host's gathered facts, then the
    /// host's own map. `HostVars::get` and a template read them in that order.
    ///
    /// The facts are the gathered map less the names some layer above the facts answers for, so
    /// a fact left in that layer is the value `for_host` would have answered with; the host's own
    /// map leaves those names out of the layers below the facts, which cannot be read in their
    /// place. Gathered facts are most of a host's variables once `setup`, `package_facts` or
    /// `service_facts` has run, and every render copies the host's own map: shared rather than
    /// merged, they are not copied at all when nothing outranks them, and once per change of
    /// what outranks them otherwise.
    ///
    /// A read of this layer is a read of what the host sent, whatever a layer below held.
    pub fn layered_for_host(&mut self, host: &str, scope: &Scope) -> Layers {
        self.host_sources(host, scope).layer(scope)
    }

    /// What [`VarStore::layered_for_host`] reads from the store, for a caller that lays it over
    /// `scope` with [`HostSources::layer`] once the store's lock is released.
    pub fn host_sources(&mut self, host: &str, scope: &Scope) -> HostSources {
        let shared = self.shared_values(scope);
        let magic = self.magic(host);
        // A host that gathered nothing still answers `ansible_facts`, empty: see `no_facts`.
        let gathered = self.gathered.get(host).unwrap_or_else(|| no_facts());
        let (shadowed, answering): (BTreeSet<String>, BTreeSet<String>) = {
            let above = [
                Some(scope.play_vars.as_map()),
                Some(scope.role_vars.as_map()),
                Some(scope.task_vars.as_map()),
                self.included.get(host).map(|layer| layer.as_map()),
                self.facts.get(host).map(|layer| layer.as_map()),
                Some(scope.role_params.as_map()),
                Some(self.extra.as_map()),
                Some(&magic),
            ];
            gathered.keys().cloned().partition(|name| {
                above.iter().flatten().any(|m| m.contains_key(name))
                    || scope.vars_files.contains_key(name)
            })
        };
        HostSources {
            base: self.host_base(host),
            included: self.included.get(host).cloned(),
            facts: self.facts.get(host).cloned(),
            extra: self.extra.clone(),
            data_magic: self.data_magic(host),
            magic,
            answering,
            data_lists: self.data_lists(scope),
            shared,
            gathered: self.facts_for(host, shadowed),
        }
    }

    /// The facts of `host` that answer: the gathered map itself when no name of it is in
    /// `shadowed`, a copy without those names otherwise, kept until the host gathers again, the
    /// set changes or the host leaves the play.
    fn facts_for(&mut self, host: &str, shadowed: BTreeSet<String>) -> Arc<Map<String, Value>> {
        let gathered = self.gathered.get(host).unwrap_or_else(|| no_facts());
        if shadowed.is_empty() {
            return Arc::clone(gathered);
        }
        if let Some(kept) = self.host_facts.get(host)
            && kept.shadowed == shadowed
        {
            return Arc::clone(&kept.map);
        }
        let mut map = Map::new();
        extend_except(&mut map, gathered, &shadowed);
        let map = Arc::new(map);
        #[cfg(test)]
        {
            *self.facts_built.entry(host.to_string()).or_default() += 1;
        }
        self.host_facts.insert(
            host.to_string(),
            HostFacts {
                shadowed,
                map: Arc::clone(&map),
            },
        );
        map
    }

    /// What a play-level keyword renders against: the play's own `vars:` under the run's extra
    /// variables, and nothing belonging to a host.
    ///
    /// Measured on ansible-core 2.19.12: `serial: "{{ n }}"` reads `-e n=2` and does not see an
    /// inventory variable of that name - with `n=2` set on every host of the inventory it still
    /// stops the run with `Error processing keyword 'serial': 'n' is undefined`. The play is cut
    /// into batches before any host is chosen, so there is no host whose value it could take.
    pub fn play_scope(&self, play_vars: &Tagged) -> Tagged {
        let mut vars = play_vars.clone();
        vars.extend(&self.extra);
        vars
    }

    /// Inventory-level sources for one host, without play, facts, extra or magic variables.
    /// This is what other hosts see through `hostvars`.
    pub(super) fn host_base(&self, host: &str) -> Tagged {
        self.host_base_in(host, false)
    }

    /// `host_base`, and with `lazy` the implicit host's interpreter left as `PENDING_PYTHON`
    /// for `settled` to fill in. Finding the controller's interpreter starts a Python, which a
    /// run that never reads `hostvars['localhost']` must not pay for.
    fn host_base_in(&self, host: &str, lazy: bool) -> Tagged {
        // A spelling of the controller that the inventory lists under another one is that host.
        let host = self.controller_alias(host).unwrap_or(host);
        let mut vars = Tagged::default();
        // Inventory group vars and inventory host vars arrive already merged in Ansible's
        // group order from `Inventory::resolve`; the `group_vars` files slot in between, `all`
        // first, so the merged inventory view is applied first and the files after. Measured on
        // ansible-core 2.19.12, a playbook `group_vars/all` file beats a child group's variable
        // in the inventory file.
        if let Some(inv) = self.inventory_vars.get(host) {
            vars.extend(inv);
        } else if self.is_implicit_localhost(host) {
            // Measured on ansible-core 2.19.12: the implicit `localhost` is in no group, and it
            // still reads the inventory's `all` variables, those of every source.
            let mut all = self.dynamic.tagged_group_vars("all");
            all.remove("ansible_group_priority");
            vars.extend(&all);
        }
        for files in &self.group_files {
            if let Some(all) = files.get("all") {
                vars.extend(all);
            }
        }
        if let Some(names) = self.group_names.get(host) {
            for files in &self.group_files {
                for name in names {
                    if let Some(group) = files.get(name) {
                        vars.extend(group);
                    }
                }
            }
        }
        // Inventory host vars must beat `group_vars/<group>` files: re-apply them on top.
        if let Some(host_only) = self.host_line_vars.get(host) {
            vars.extend(host_only);
        }
        // What a `compose` computed read the host's variables, and stays data.
        for name in self.computed.get(host).into_iter().flatten() {
            vars.mark_from_host(name);
        }
        // A name the inventory never carried is an implicit `localhost`, which connects locally.
        // The reference sets it on the host object, that is at the inventory host var layer, and
        // that is where it sits here: measured on ansible-core 2.19.12, a `group_vars/all` file
        // and a role `defaults/` naming `ansible_connection` both lose to it and the play runs
        // `changed: [localhost]`, while a `host_vars/localhost` file, the play's `vars:` and an
        // extra var each beat it and send the play over ssh.
        if self.is_implicit_localhost(host) {
            vars.insert(
                "ansible_connection".into(),
                Value::String("local".to_string()),
                None,
            );
            // The reference also sets the controller's own interpreter there
            // (`sys.executable`), at the same layer, so the host runs Python tasks under it
            // and discovers nothing. Left unset when no interpreter resolves.
            if lazy {
                vars.insert(
                    "ansible_python_interpreter".into(),
                    Value::String(PENDING_PYTHON.to_string()),
                    None,
                );
            } else if let Some(python) = crate::runtime::resolve::controller_python() {
                vars.insert(
                    "ansible_python_interpreter".into(),
                    Value::String(python),
                    None,
                );
            }
        }
        for files in &self.host_files {
            if let Some(h) = files.get(host) {
                vars.extend(h);
            }
        }
        vars
    }

    /// What other hosts see of one host: inventory sources, its facts, then extra vars, and its
    /// own identity. The reference carries `inventory_hostname` inside `hostvars[x]`, so a task
    /// naming another host can ask who that host is.
    pub(super) fn host_view(&self, host: &str) -> Tagged {
        let mut base = self.host_base_in(host, true);
        match self.gathered.get(host) {
            Some(gathered) => {
                for (name, value) in &**gathered {
                    base.insert_from_host(name.clone(), value.clone());
                }
            }
            None => base.extend_untagged(no_facts()),
        }
        if let Some(included) = self.included.get(host) {
            base.extend(included);
        }
        if let Some(facts) = self.facts.get(host) {
            base.extend(facts);
        }
        base.extend(&self.extra);
        // The host's magic variables as well, measured on ansible-core 2.19.12: `hostvars[x]`
        // carries `group_names`, `inventory_dir`, `playbook_dir` and the run's options, an
        // `add_host` host's too. Not `omit`, which only a task's own variables hold.
        let mut magic = self.magic(host);
        magic.remove("omit");
        base.extend_untagged(&magic);
        let made = self.added.iter().any(|change| {
            matches!(change, Added::Host(name, _) | Added::Member(name, _) if name == host)
        });
        if made || is_data_name(host) {
            super::HOST_NAMES
                .iter()
                .for_each(|n| base.mark_from_host(n));
        }
        for name in self.data_magic(host) {
            base.mark_from_host(name);
        }
        base
    }

    /// The shared inventory-wide map is deliberately not dropped here: the host lists are keyed
    /// on in `shared_values`, and `groups` only moves in `refresh_inventory`, which drops
    /// `self.shared` itself.
    pub(super) fn forget_hostvars(&mut self) {
        self.hostvars = None;
        self.stale.clear();
        self.hostvars_with.clear();
    }

    /// What a write to one host's facts leaves behind: that host's `hostvars` entry is out of
    /// date, and nobody else's is. The completed maps of hosts outside the inventory carry every
    /// host's entry and go whole; there is normally at most one of them.
    pub(super) fn forget_host(&mut self, host: &str) {
        self.stale.insert(host.to_string());
        self.hostvars_with.clear();
    }

    /// `host_view`, counted for the test that proves only the written host's entry is rebuilt.
    pub(super) fn view_value(&mut self, host: &str) -> HostEntry {
        #[cfg(test)]
        {
            *self.views_built.entry(host.to_string()).or_default() += 1;
        }
        HostEntry::from(self.host_view(host))
    }
}
