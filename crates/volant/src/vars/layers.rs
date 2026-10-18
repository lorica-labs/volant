// SPDX-License-Identifier: GPL-3.0-or-later
//! The precedence layers: one host's merged variables and the inventory-wide values every host
//! shares.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Map, Value};

use super::facts::no_facts;
use super::load::{extend, extend_except};
use super::magic::short_name;
use super::{HostFacts, Scope, VarStore};
/// One host's variables as `layered_for_host` hands them out.
#[derive(Debug)]
pub struct Layers {
    /// Every layer but the two below.
    pub map: Map<String, Value>,
    /// The inventory-wide values: `shared_values`.
    pub shared: Arc<Map<String, Value>>,
    /// The gathered facts that answer.
    pub facts: Arc<Map<String, Value>>,
}

impl VarStore {
    /// The names of `host`'s merged view that came from a managed host: the `set_fact` layer's
    /// untrusted names, and every gathered one that no layer above it overrides. A play variable
    /// that shadows a gathered name answers with the author's value, and a template in it renders.
    pub fn untrusted_of(&self, host: &str, scope: &Scope) -> BTreeSet<String> {
        let mut names = self.untrusted.get(host).cloned().unwrap_or_default();
        let Some(gathered) = self.gathered.get(host) else {
            return names;
        };
        let facts = self.facts.get(host);
        let above = [
            Some(&scope.play_vars),
            Some(&scope.role_vars),
            Some(&scope.task_vars),
            facts,
            Some(&scope.role_params),
            Some(&self.extra),
        ];
        for name in gathered.keys() {
            let shadowed = above.iter().flatten().any(|m| m.contains_key(name))
                || scope.vars_files.iter().any(|m| m.contains_key(name));
            if !shadowed {
                names.insert(name.clone());
            }
        }
        names
    }

    /// Hosts holding at least one untrusted name, for the `hostvars[other]` path. Read once per
    /// host and per task, under the store's lock, so it is kept rather than rebuilt: the set is
    /// the size of the inventory and every rebuild allocated a string per host inside the
    /// critical section. Dropped by `forget_hostvars`, which every write to the facts calls.
    pub fn untrusted_hosts(&mut self) -> Arc<BTreeSet<String>> {
        if let Some(hosts) = &self.untrusted_hosts {
            return Arc::clone(hosts);
        }
        let hosts: Arc<BTreeSet<String>> = Arc::new(
            self.untrusted
                .iter()
                .filter(|(_, names)| !names.is_empty())
                .map(|(host, _)| host)
                .chain(self.gathered.keys())
                .cloned()
                .collect(),
        );
        self.untrusted_hosts = Some(Arc::clone(&hosts));
        hosts
    }

    /// The merged view for one host, lowest precedence first: a role's `defaults`, inventory
    /// `all`, `group_vars/all` (inventory then playbook), inventory groups by depth and name,
    /// `group_vars/<group>` (inventory then playbook), inventory host vars - an implicit
    /// `localhost`'s `local` connection among them - `host_vars/<host>`
    /// (inventory then playbook), gathered facts, play vars, vars_files, a role's `vars`, task
    /// vars, `set_fact` and registered values, a role's parameters, extra vars, then the magic
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
        for file in &scope.vars_files {
            extend(&mut vars, file);
        }
        extend(&mut vars, &scope.role_vars);
        extend(&mut vars, &scope.task_vars);
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
                Some(&scope.play_vars),
                Some(&scope.role_vars),
                Some(&scope.task_vars),
                self.facts.get(host),
                Some(&scope.role_params),
                Some(&self.extra),
                Some(&magic),
            ];
            gathered.keys().cloned().partition(|name| {
                above.iter().flatten().any(|m| m.contains_key(name))
                    || scope.vars_files.iter().any(|m| m.contains_key(name))
            })
        };
        let mut map = Map::new();
        extend_except(&mut map, &scope.role_defaults, &answering);
        extend_except(&mut map, &self.host_base(host), &answering);
        extend(&mut map, &scope.play_vars);
        for file in &scope.vars_files {
            extend(&mut map, file);
        }
        extend(&mut map, &scope.role_vars);
        extend(&mut map, &scope.task_vars);
        if let Some(facts) = self.facts.get(host) {
            extend(&mut map, facts);
        }
        extend(&mut map, &scope.role_params);
        extend(&mut map, &self.extra);
        map.extend(magic);
        let facts = self.facts_for(host, shadowed);
        Layers { map, shared, facts }
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
    fn host_base(&self, host: &str) -> Map<String, Value> {
        let mut vars = Map::new();
        for files in &self.group_files {
            if let Some(all) = files.get("all") {
                extend(&mut vars, all);
            }
        }
        // Inventory group vars and inventory host vars arrive already merged in Ansible's
        // group order from `Inventory::resolve`; `group_vars/<group>` files slot in between,
        // so the merged inventory view is applied first and files for the host's groups after.
        if let Some(inv) = self.inventory_vars.get(host) {
            extend(&mut vars, inv);
        }
        if let Some(names) = self.group_names.get(host) {
            for files in &self.group_files {
                for name in names {
                    if let Some(group) = files.get(name) {
                        extend(&mut vars, group);
                    }
                }
            }
        }
        // Inventory host vars must beat `group_vars/<group>` files: re-apply them on top.
        if let Some(host_only) = self.host_line_vars.get(host) {
            extend(&mut vars, host_only);
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
            );
        }
        for files in &self.host_files {
            if let Some(h) = files.get(host) {
                extend(&mut vars, h);
            }
        }
        vars
    }

    /// What other hosts see of one host: inventory sources, its facts, then extra vars, and its
    /// own identity. The reference carries `inventory_hostname` inside `hostvars[x]`, so a task
    /// naming another host can ask who that host is.
    fn host_view(&self, host: &str) -> Map<String, Value> {
        let mut base = self.host_base(host);
        match self.gathered.get(host) {
            Some(gathered) => extend(&mut base, gathered),
            None => extend(&mut base, no_facts()),
        }
        if let Some(facts) = self.facts.get(host) {
            extend(&mut base, facts);
        }
        extend(&mut base, &self.extra);
        base.insert("inventory_hostname".into(), Value::String(host.to_string()));
        base.insert(
            "inventory_hostname_short".into(),
            Value::String(short_name(host)),
        );
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
    fn view_value(&mut self, host: &str) -> Value {
        #[cfg(test)]
        {
            *self.views_built.entry(host.to_string()).or_default() += 1;
        }
        Value::Object(self.host_view(host))
    }

    /// The `hostvars` view every host of this run renders against, as one shared map: templates
    /// read a host out of it rather than carrying a copy of it.
    ///
    /// `host` is the host about to render. An implicit `localhost` belongs to no group, so it is
    /// not in the inventory-wide map, and `hostvars[inventory_hostname]` has to answer for it
    /// all the same: such a host gets its own completed map, kept beside the shared one.
    ///
    /// A host whose facts changed since the map was built has its own entry rebuilt, and only
    /// that one. The map is changed in place when nobody else holds it; a render still holding
    /// the previous map keeps it whole, which is what it saw when it took it, and the store
    /// then carries on with a copy.
    pub fn hostvars_shared(&mut self, host: &str) -> Arc<Map<String, Value>> {
        let base = if let Some(mut base) = self.hostvars.take() {
            for name in std::mem::take(&mut self.stale) {
                if base.contains_key(&name) {
                    let view = self.view_value(&name);
                    Arc::make_mut(&mut base).insert(name, view);
                }
            }
            base
        } else {
            self.stale.clear();
            let names = self.groups["all"].clone();
            let mut map = Map::new();
            for name in names {
                let view = self.view_value(&name);
                map.insert(name, view);
            }
            Arc::new(map)
        };
        self.hostvars = Some(Arc::clone(&base));
        if base.contains_key(host) {
            return base;
        }
        if let Some(completed) = self.hostvars_with.get(host) {
            return Arc::clone(completed);
        }
        let mut map = (*base).clone();
        map.insert(host.to_string(), self.view_value(host));
        let completed = Arc::new(map);
        self.hostvars_with
            .insert(host.to_string(), Arc::clone(&completed));
        completed
    }

    /// The values every host of a batch reads the same: `groups`, which only a refresh of the
    /// inventory moves, and the three live host lists, which are fixed for as long as nobody drops out. Each is
    /// the size of the inventory, so they are built once and handed to templates as one shared
    /// object rather than copied into every host's map for every task.
    ///
    /// The deprecated `play_hosts` answers with the batch, not the play: measured on
    /// ansible-core 2.19.12, `h2` in the first batch of a `serial: 2` run over three hosts reads
    /// `['h2']` from it where `ansible_play_hosts` says `['h2', 'h3']`.
    pub fn shared_values(&mut self, scope: &Scope) -> Arc<Map<String, Value>> {
        if let Some((key, map)) = &self.shared
            && key.0 == scope.play_hosts
            && key.1 == scope.batch_hosts
            && key.2 == scope.all_play_hosts
        {
            return Arc::clone(map);
        }
        // The lists moved: a host failed, went unreachable, or a new play began. A host no longer
        // in them has no task left to read its kept facts; everyone else's entry stays.
        let live: BTreeSet<&String> = scope.play_hosts.iter().chain(&scope.batch_hosts).collect();
        self.host_facts.retain(|host, _| live.contains(host));
        let batch = serde_json::to_value(&scope.batch_hosts).unwrap_or_default();
        let mut map = Map::new();
        map.insert(
            "groups".to_string(),
            serde_json::to_value(&self.groups).unwrap_or_default(),
        );
        map.insert(
            "ansible_play_hosts".to_string(),
            serde_json::to_value(&scope.play_hosts).unwrap_or_default(),
        );
        // Known gap: measured on ansible-core 2.19.12, `ansible_play_hosts_all` grows when
        // `add_host` runs inside a `serial` play; here it stays the play's own list.
        map.insert(
            "ansible_play_hosts_all".to_string(),
            serde_json::to_value(
                self.play_hosts_all
                    .as_ref()
                    .unwrap_or(&scope.all_play_hosts),
            )
            .unwrap_or_default(),
        );
        map.insert("ansible_play_batch".to_string(), batch.clone());
        map.insert("play_hosts".to_string(), batch);
        let shared = Arc::new(map);
        self.shared = Some((
            (
                scope.play_hosts.clone(),
                scope.batch_hosts.clone(),
                scope.all_play_hosts.clone(),
            ),
            Arc::clone(&shared),
        ));
        shared
    }
}
