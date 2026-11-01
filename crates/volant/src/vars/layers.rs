// SPDX-License-Identifier: GPL-3.0-or-later
//! The precedence layers: one host's merged variables and the inventory-wide values every host
//! shares.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Map, Value};

use super::PENDING_PYTHON;
use super::facts::no_facts;
use super::load::{extend, extend_except};
use super::{Added, HostFacts, Scope, VarStore};
use crate::inventory::{LOCALHOST_NAMES, is_localhost};
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
    /// The `vars_files` values no render changes, read after `map`: `VarsFiles::plain`.
    pub files: Arc<Tagged>,
}

impl VarStore {
    /// The names of `host`'s merged view that came from a managed host: the `set_fact` layer's
    /// untrusted names, and every gathered one that no layer above it overrides. A play variable
    /// that shadows a gathered name answers with the author's value, and a template in it renders.
    pub fn untrusted_of(&self, host: &str, scope: &Scope) -> BTreeSet<String> {
        let mut names = self.untrusted.get(host).cloned().unwrap_or_default();
        self.data_names_read_by(host, scope, &mut names);
        let Some(gathered) = self.gathered.get(host) else {
            return names;
        };
        let above = [
            Some(&scope.play_vars),
            Some(&scope.role_vars),
            Some(&scope.task_vars),
            self.included.get(host),
            self.facts.get(host),
            Some(&scope.role_params),
            Some(&self.extra),
        ];
        for name in gathered.keys() {
            let shadowed = above.iter().flatten().any(|m| m.contains_key(name))
                || scope.vars_files.contains_key(name);
            if !shadowed {
                names.insert(name.clone());
            }
        }
        names
    }

    /// The names `host` reads that now carry a host name or a group name `add_host` or
    /// `group_by` took from data: `groups`, the host lists of the play, `group_names`, the two
    /// `inventory_hostname` names, and the list of `hostvars`' hosts. The read as a whole is
    /// data, where the reference tags each string. A read of one host through `hostvars[name]`
    /// is the case of `untrusted_hosts`, so a nested template of any other host still renders.
    fn data_names_read_by(&self, host: &str, scope: &Scope, names: &mut BTreeSet<String>) {
        if self.data_hosts.is_empty() && self.data_groups.is_empty() {
            return;
        }
        let listed = |hosts: &[String]| hosts.iter().any(|h| self.data_hosts.contains(h));
        let all_hosts = self.play_hosts_all(scope);
        let reads = [
            ("groups", true),
            (HOSTVARS_KEYS, !self.data_hosts.is_empty()),
            ("inventory_hostname", self.data_hosts.contains(host)),
            ("inventory_hostname_short", self.data_hosts.contains(host)),
            (
                "group_names",
                self.group_names
                    .get(host)
                    .is_some_and(|g| g.iter().any(|n| self.data_groups.contains(n))),
            ),
            ("ansible_play_hosts", listed(&scope.play_hosts)),
            ("ansible_play_hosts_all", listed(&all_hosts)),
            ("ansible_play_batch", listed(&scope.batch_hosts)),
            ("play_hosts", listed(&scope.batch_hosts)),
        ];
        names.extend(
            reads
                .into_iter()
                .filter(|(_, data)| *data)
                .map(|(name, _)| name.to_string()),
        );
    }

    /// Hosts holding at least one untrusted name, for the `hostvars[other]` path. Read once per
    /// host and per task, under the store's lock, so it is kept rather than rebuilt: the set is
    /// the size of the inventory and every rebuild allocated a string per host inside the
    /// critical section. A spelling of the controller is in the set when any spelling is. Dropped
    /// by `forget_hostvars`, which every write to the facts calls.
    pub fn untrusted_hosts(&mut self) -> Arc<super::UntrustedHosts> {
        if let Some(hosts) = &self.untrusted_hosts {
            return Arc::clone(hosts);
        }
        let mut hosts = super::UntrustedHosts::new();
        for (host, names) in self.untrusted.iter().filter(|(_, names)| !names.is_empty()) {
            hosts
                .entry(host.clone())
                .or_default()
                .extend(names.iter().cloned());
        }
        // The facts a host gathered are its own text, every one of them.
        for (host, facts) in &self.gathered {
            let names = hosts.entry(host.clone()).or_default();
            names.extend(facts.keys().cloned());
            names.insert("ansible_facts".into());
        }
        // A host named by data is a host whose name is data.
        for host in &self.data_hosts {
            hosts
                .entry(host.clone())
                .or_default()
                .extend(super::HOST_NAMES.map(String::from));
        }
        // The three spellings of the controller read one host through `hostvars`, so what one of
        // them holds, the others hold.
        let controller: BTreeSet<String> = hosts
            .iter()
            .filter(|(host, _)| is_localhost(host))
            .flat_map(|(_, names)| names.iter().cloned())
            .collect();
        if !controller.is_empty() {
            for name in LOCALHOST_NAMES {
                hosts
                    .entry(name.to_string())
                    .or_default()
                    .extend(controller.iter().cloned());
            }
        }
        let hosts = Arc::new(hosts);
        self.untrusted_hosts = Some(Arc::clone(&hosts));
        hosts
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
    /// Trust does not move with them. `untrusted_of` still names every gathered name that
    /// answers, and a template reads that set before it looks a name up anywhere.
    pub fn layered_for_host(&mut self, host: &str, scope: &Scope) -> Layers {
        let shared = self.shared_values(scope);
        let magic = self.magic(host);
        // A host that gathered nothing still answers `ansible_facts`, empty: see `no_facts`.
        let gathered = self.gathered.get(host).unwrap_or_else(|| no_facts());
        let (shadowed, answering): (BTreeSet<String>, BTreeSet<String>) = {
            let above = [
                Some(scope.play_vars.as_map()),
                Some(scope.role_vars.as_map()),
                Some(scope.task_vars.as_map()),
                self.included.get(host).map(Tagged::as_map),
                self.facts.get(host).map(Tagged::as_map),
                Some(scope.role_params.as_map()),
                Some(self.extra.as_map()),
                Some(&magic),
            ];
            gathered.keys().cloned().partition(|name| {
                above.iter().flatten().any(|m| m.contains_key(name))
                    || scope.vars_files.contains_key(name)
            })
        };
        let mut map = Tagged::default();
        map.extend_except(&scope.role_defaults, &answering);
        map.extend_except(&self.host_base(host), &answering);
        map.extend(&scope.play_vars);
        // The plain half of the `vars_files` is read after this map, so a name it holds leaves
        // the layers below it here, and every layer above it lands in the map.
        let files = Arc::clone(&scope.vars_files.plain);
        if !files.is_empty() {
            map.retain(|name, _| !files.contains_key(name));
        }
        map.extend(&scope.vars_files.templated);
        map.extend(&scope.role_vars);
        map.extend(&scope.task_vars);
        if let Some(included) = self.included.get(host) {
            map.extend(included);
        }
        if let Some(facts) = self.facts.get(host) {
            map.extend(facts);
        }
        map.extend(&scope.role_params);
        map.extend(&self.extra);
        for (name, value) in magic {
            map.insert(name, value, None);
        }
        let facts = self.facts_for(host, shadowed);
        crate::profile::count(crate::profile::Counter::LayersBuilt, 1);
        crate::profile::count(
            crate::profile::Counter::LayersEntries,
            u64::try_from(map.len()).unwrap_or(u64::MAX),
        );
        Layers {
            map,
            shared,
            facts,
            files,
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
    pub fn play_scope(&self, play_vars: &Map<String, Value>) -> Map<String, Value> {
        let mut vars = play_vars.clone();
        extend(&mut vars, &self.extra);
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
            let mut all = self.dynamic.group_vars_of("all");
            all.remove("ansible_group_priority");
            vars.extend_untagged(&all);
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
    pub(super) fn host_view(&self, host: &str) -> Map<String, Value> {
        let mut base = self.host_base_in(host, true).into_values();
        match self.gathered.get(host) {
            Some(gathered) => extend(&mut base, gathered),
            None => extend(&mut base, no_facts()),
        }
        if let Some(included) = self.included.get(host) {
            extend(&mut base, included);
        }
        if let Some(facts) = self.facts.get(host) {
            extend(&mut base, facts);
        }
        extend(&mut base, &self.extra);
        // The host's magic variables as well, measured on ansible-core 2.19.12: `hostvars[x]`
        // carries `group_names`, `inventory_dir`, `playbook_dir` and the run's options, an
        // `add_host` host's too. Not `omit`, which only a task's own variables hold.
        let mut magic = self.magic(host);
        magic.remove("omit");
        base.extend(magic);
        base
    }

    /// The shared inventory-wide map is deliberately not dropped here: the host lists are keyed
    /// on in `shared_values`, and `groups` only moves in `refresh_inventory`, which drops
    /// `self.shared` itself.
    pub(super) fn forget_hostvars(&mut self) {
        self.hostvars = None;
        self.stale.clear();
        self.hostvars_with.clear();
        self.untrusted_hosts = None;
    }

    /// What a write to one host's facts leaves behind: that host's `hostvars` entry is out of
    /// date, and nobody else's is. The completed maps of hosts outside the inventory carry every
    /// host's entry and go whole; there is normally at most one of them.
    pub(super) fn forget_host(&mut self, host: &str) {
        self.stale.insert(host.to_string());
        self.hostvars_with.clear();
        self.untrusted_hosts = None;
    }

    /// `host_view`, counted for the test that proves only the written host's entry is rebuilt.
    pub(super) fn view_value(&mut self, host: &str) -> Value {
        #[cfg(test)]
        {
            *self.views_built.entry(host.to_string()).or_default() += 1;
        }
        Value::Object(self.host_view(host))
    }
}
