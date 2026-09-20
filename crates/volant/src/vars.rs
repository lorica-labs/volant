// SPDX-License-Identifier: GPL-3.0-or-later
//! Variable sources and their precedence, from inventory groups up to extra vars, plus the
//! magic variables Ansible defines for every host.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use serde_json::{Map, Value};

use crate::inventory::Inventory;
use crate::template::Vars;
use crate::yaml;

/// One host's merged variables, with the inventory-wide view its templates read `hostvars`
/// from. The two travel together because every render a host does needs both, and the view is
/// shared rather than copied into the map: see `crate::template::Vars`.
#[derive(Debug, Clone, Default)]
pub struct HostVars {
    pub map: Map<String, Value>,
    pub hostvars: Arc<Map<String, Value>>,
    /// `groups` and the play's host lists, shared for the same reason and in the same way.
    pub shared: Arc<Map<String, Value>>,
    /// Names in `map` that came from a managed host, for this host and this task.
    pub untrusted: BTreeSet<String>,
    /// Hosts holding at least one such name, for the `hostvars[other]` path.
    pub untrusted_hosts: Arc<BTreeSet<String>>,
}

impl HostVars {
    /// `insert`, for a name whose value came from a managed host: a `register`, `result`, a loop
    /// item built from one. The name is data from here on and is never rendered again.
    pub fn insert_untrusted(&mut self, key: String, value: Value) -> Option<Value> {
        // After, not before, for the reason `VarStore::set_untrusted_fact` gives: `insert`
        // clears the name.
        let previous = self.insert(key.clone(), value);
        self.untrusted.insert(key);
        previous
    }

    /// Writes a name into the host's own map once the map is built - a loop variable, a
    /// `register` name, `result`. Such a write is the last one there is and beats everything
    /// the merge produced, the inventory-wide values included: a name that collides with one of
    /// them leaves this host's shared map, which is how the host's own value gets to answer.
    /// The map is left alone when there is no collision, so the ordinary write costs nothing.
    pub fn insert(&mut self, key: String, value: Value) -> Option<Value> {
        // The name gets its trust back, the way `VarStore::set_fact` gives it back: this write
        // is author content whatever the name held before.
        self.untrusted.remove(&key);
        if self.shared.contains_key(&key) {
            let mut shared = Map::clone(&self.shared);
            shared.remove(&key);
            self.shared = Arc::new(shared);
        }
        self.map.insert(key, value)
    }

    /// Reads a name the way a template reads it: the inventory-wide values first, because they
    /// were merged last, then the host's own map. Everything that asks this map for a name by
    /// hand rather than through a render goes through here, so the two answer alike.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.shared.get(key).or_else(|| self.map.get(key))
    }
}

impl std::ops::Deref for HostVars {
    type Target = Map<String, Value>;

    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

impl std::ops::DerefMut for HostVars {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.map
    }
}

impl<'a> From<&'a HostVars> for Vars<'a> {
    fn from(vars: &'a HostVars) -> Self {
        Vars {
            map: &vars.map,
            hostvars: Some(&vars.hostvars),
            shared: Some(&vars.shared),
            untrusted: Some(&vars.untrusted),
            untrusted_hosts: Some(&vars.untrusted_hosts),
        }
    }
}

/// What a play and a task contribute on top of inventory-level sources.
#[derive(Debug, Default, Clone)]
pub struct Scope {
    pub play_vars: Map<String, Value>,
    pub vars_files: Vec<Map<String, Value>>,
    pub task_vars: Map<String, Value>,
    /// A role's `defaults/main.yml`, the lowest layer of all: measured, it loses even to an
    /// inventory variable of the same name.
    pub role_defaults: Map<String, Value>,
    /// A role's `vars/main.yml`: measured, it beats the play's own `vars:` and loses to a task's
    /// `vars:`, to `set_fact` and to `-e`.
    pub role_vars: Map<String, Value>,
    /// The free keys written on a role entry: measured, they beat a `set_fact` and lose to `-e`,
    /// and they are gone again once the role is over.
    pub role_params: Map<String, Value>,
    /// Hosts of the whole play that have not failed, in inventory order - the hosts of the
    /// batches still to come included. This is `ansible_play_hosts`.
    pub play_hosts: Vec<String>,
    /// Hosts of the batch being played that have not failed. Without `serial` a play is one
    /// batch and this is the same list as `play_hosts`; with it, the two part company as soon as
    /// the first batch ends. This is `ansible_play_batch`.
    pub batch_hosts: Vec<String>,
    /// Every host the play resolved, whether it is still in it or not.
    pub all_play_hosts: Vec<String>,
}

/// Every variable source that does not depend on the play, resolved once per run, plus the
/// facts hosts accumulate (`set_fact`, `register`) while the run goes.
#[derive(Debug)]
pub struct VarStore {
    inventory_dir: Option<PathBuf>,
    inventory_file: Option<PathBuf>,
    playbook_dir: PathBuf,
    /// Group vars from `group_vars/` directories: inventory-side then playbook-side.
    group_files: [BTreeMap<String, Map<String, Value>>; 2],
    /// Host vars from `host_vars/` directories: inventory-side then playbook-side.
    host_files: [BTreeMap<String, Map<String, Value>>; 2],
    inventory_vars: BTreeMap<String, Map<String, Value>>,
    host_line_vars: BTreeMap<String, Map<String, Value>>,
    group_names: BTreeMap<String, Vec<String>>,
    groups: BTreeMap<String, Vec<String>>,
    facts: BTreeMap<String, Map<String, Value>>,
    /// Of those facts, the names that came from a managed host rather than from the playbook.
    untrusted: BTreeMap<String, BTreeSet<String>>,
    extra: Map<String, Value>,
    /// What `ansible_forks` reports, which the reference sets from the run's own `forks`.
    forks: usize,
    /// Every inventory host's view, as `hostvars` shows it. Built on demand and dropped
    /// whenever something below it changes, which only a fact or a rebase does.
    hostvars: Option<Arc<Map<String, Value>>>,
    /// The same view completed with a host the inventory does not carry - an implicit
    /// `localhost` - one map per such host. There is normally at most one of them in a run.
    hostvars_with: BTreeMap<String, Arc<Map<String, Value>>>,
    /// The hosts that hold at least one untrusted name, built on demand and dropped by the same
    /// rule as `hostvars`: every write that can change it already goes through `set_fact`.
    untrusted_hosts: Option<Arc<BTreeSet<String>>>,
    /// The inventory-wide values, with the three host lists they were built from. `groups` is
    /// fixed for the run, but the lists come from the coordinator's last published progress and
    /// move whenever a host drops out, so the map is good for one triple and no longer.
    shared: Option<(SharedKey, Arc<Map<String, Value>>)>,
}

/// The play and batch host lists one `shared` map was built from.
type SharedKey = (Vec<String>, Vec<String>, Vec<String>);

/// The value Ansible substitutes for `omit`: a parameter equal to it is dropped from the task.
/// Ansible generates a fresh random suffix per run; a stable one changes nothing for playbooks
/// and keeps output reproducible.
pub fn omit_token() -> &'static str {
    "__omit_place_holder__6d24c2dd6a8f2e0e2a6e5cce6b1f9c4a"
}

/// An inventory name without its domain, which is what `inventory_hostname_short` reports.
fn short_name(host: &str) -> String {
    host.split('.').next().unwrap_or(host).to_string()
}

impl VarStore {
    pub fn new(
        inventory: &Inventory,
        inventory_path: Option<&Path>,
        playbook_dir: &Path,
        extra: Map<String, Value>,
    ) -> anyhow::Result<Self> {
        let inventory_dir = inventory_path
            .and_then(|p| p.parent())
            .map(Path::to_path_buf);
        let roots: [Option<&Path>; 2] = [inventory_dir.as_deref(), Some(playbook_dir)];
        let mut group_files = [BTreeMap::new(), BTreeMap::new()];
        let mut host_files = [BTreeMap::new(), BTreeMap::new()];
        for (i, root) in roots.iter().enumerate() {
            if let Some(root) = root {
                group_files[i] = load_vars_dir(&root.join("group_vars"))?;
                host_files[i] = load_vars_dir(&root.join("host_vars"))?;
            }
        }
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
            group_names.insert(host.clone(), inventory.group_names_of(host));
        }
        Ok(Self {
            inventory_dir,
            inventory_file: inventory_path.map(Path::to_path_buf),
            playbook_dir: playbook_dir.to_path_buf(),
            group_files,
            host_files,
            inventory_vars,
            host_line_vars,
            group_names,
            groups,
            facts: BTreeMap::new(),
            untrusted: BTreeMap::new(),
            extra,
            forks: crate::config::DEFAULT_FORKS,
            hostvars: None,
            hostvars_with: BTreeMap::new(),
            untrusted_hosts: None,
            shared: None,
        })
    }

    /// What `ansible_forks` reports for this run.
    pub fn set_forks(&mut self, forks: usize) {
        self.forks = forks;
        self.forget_hostvars();
    }

    pub fn playbook_dir(&self) -> &Path {
        &self.playbook_dir
    }

    /// Moves the playbook-side roots to another playbook's directory, keeping the facts hosts
    /// have gathered so far. Inventory-side sources do not move.
    pub fn rebase(&mut self, playbook_dir: &Path) -> anyhow::Result<()> {
        self.group_files[1] = load_vars_dir(&playbook_dir.join("group_vars"))?;
        self.host_files[1] = load_vars_dir(&playbook_dir.join("host_vars"))?;
        self.playbook_dir = playbook_dir.to_path_buf();
        self.forget_hostvars();
        Ok(())
    }

    /// Writes a fact the playbook wrote: `include_vars`, and the engine's own bookkeeping. The
    /// name gets its trust back, because a write from an author-side source is author content
    /// whatever the name held before.
    pub fn set_fact(&mut self, host: &str, key: &str, value: Value) {
        if let Some(names) = self.untrusted.get_mut(host) {
            names.remove(key);
        }
        self.facts
            .entry(host.to_string())
            .or_default()
            .insert(key.to_string(), value);
        self.forget_hostvars();
    }

    /// Writes a fact that came from a managed host: a module result, a `register`, a `set_fact`.
    /// The name is data from here on, and a template that reads it is not rendered again.
    pub fn set_untrusted_fact(&mut self, host: &str, key: &str, value: Value) {
        // After, not before: `set_fact` clears the name, because a write from an author-side
        // source gives the trust back. The cache `set_fact` dropped covers this write too,
        // because nothing can read it between the two lines.
        self.set_fact(host, key, value);
        self.untrusted
            .entry(host.to_string())
            .or_default()
            .insert(key.to_string());
    }

    pub fn untrusted_of(&self, host: &str) -> BTreeSet<String> {
        self.untrusted.get(host).cloned().unwrap_or_default()
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
                .map(|(host, _)| host.clone())
                .collect(),
        );
        self.untrusted_hosts = Some(Arc::clone(&hosts));
        hosts
    }

    /// The merged view for one host, lowest precedence first: a role's `defaults`, inventory
    /// `all`, `group_vars/all` (inventory then playbook), inventory groups by depth and name,
    /// `group_vars/<group>` (inventory then playbook), inventory host vars, `host_vars/<host>`
    /// (inventory then playbook), play vars, vars_files, a role's `vars`, task vars, facts, a
    /// role's parameters, extra vars, then the magic variables.
    ///
    /// The three role layers sit where ansible-core 2.19.12 puts them, each measured against the
    /// layer on either side of it rather than derived from the documented numbering: `defaults`
    /// under an inventory variable, `vars` above the play's `vars:` and under a task's, and a
    /// role parameter above a `set_fact` and under `-e`.
    ///
    /// The inventory-wide magic variables are not in this map: `shared_values` builds them, and
    /// a template reads them from there first, which is the place in the order they had here.
    pub fn for_host(&mut self, host: &str, scope: &Scope) -> Map<String, Value> {
        let mut vars = scope.role_defaults.clone();
        extend(&mut vars, &self.host_base(host));
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
        self.add_magic(&mut vars, host);
        vars
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

    /// The shared inventory-wide map is deliberately not dropped here: `groups` is assigned in
    /// `new` and never touched again, and the host lists are keyed on in `shared_values`. The
    /// day a module adds a host to the inventory mid-run, `groups` starts moving and this has
    /// to drop `self.shared` too, or the run keeps reading the inventory it started with.
    fn forget_hostvars(&mut self) {
        self.hostvars = None;
        self.hostvars_with.clear();
        self.untrusted_hosts = None;
    }

    /// The `hostvars` view every host of this run renders against, as one shared map: templates
    /// read a host out of it rather than carrying a copy of it.
    ///
    /// `host` is the host about to render. An implicit `localhost` belongs to no group, so it is
    /// not in the inventory-wide map, and `hostvars[inventory_hostname]` has to answer for it
    /// all the same: such a host gets its own completed map, kept beside the shared one.
    pub fn hostvars_shared(&mut self, host: &str) -> Arc<Map<String, Value>> {
        let base = if let Some(base) = &self.hostvars {
            Arc::clone(base)
        } else {
            let names = self.groups["all"].clone();
            let mut map = Map::new();
            for name in names {
                let view = self.host_view(&name);
                map.insert(name, Value::Object(view));
            }
            let base = Arc::new(map);
            self.hostvars = Some(Arc::clone(&base));
            base
        };
        if base.contains_key(host) {
            return base;
        }
        if let Some(completed) = self.hostvars_with.get(host) {
            return Arc::clone(completed);
        }
        let mut map = (*base).clone();
        map.insert(host.to_string(), Value::Object(self.host_view(host)));
        let completed = Arc::new(map);
        self.hostvars_with
            .insert(host.to_string(), Arc::clone(&completed));
        completed
    }

    /// The values every host of a batch reads the same: `groups`, which is fixed for the run,
    /// and the three live host lists, which are fixed for as long as nobody drops out. Each is
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
        map.insert(
            "ansible_play_hosts_all".to_string(),
            serde_json::to_value(&scope.all_play_hosts).unwrap_or_default(),
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

    /// The magic variables that belong to one host. The inventory-wide ones are not here: see
    /// `shared_values`.
    fn add_magic(&self, vars: &mut Map<String, Value>, host: &str) {
        let short = short_name(host);
        let group_names = self.group_names.get(host).cloned().unwrap_or_default();
        vars.insert(
            "inventory_hostname".to_string(),
            Value::String(host.to_string()),
        );
        vars.insert("inventory_hostname_short".to_string(), Value::String(short));
        vars.insert(
            "group_names".to_string(),
            serde_json::to_value(group_names).unwrap_or_default(),
        );
        vars.insert(
            "playbook_dir".to_string(),
            Value::String(self.playbook_dir.display().to_string()),
        );
        if let Some(dir) = &self.inventory_dir {
            vars.insert(
                "inventory_dir".to_string(),
                Value::String(dir.display().to_string()),
            );
        }
        if let Some(file) = &self.inventory_file {
            vars.insert(
                "inventory_file".to_string(),
                Value::String(file.display().to_string()),
            );
        }
        vars.insert("omit".to_string(), Value::String(omit_token().to_string()));
        vars.insert("ansible_check_mode".to_string(), Value::Bool(false));
        vars.insert("ansible_diff_mode".to_string(), Value::Bool(false));
        vars.insert("ansible_forks".to_string(), Value::from(self.forks));
        vars.insert("ansible_version".to_string(), ansible_version());
        vars.insert(
            "volant_version".to_string(),
            Value::String(env!("CARGO_PKG_VERSION").to_string()),
        );
    }
}

/// The ansible-core release Volant reproduces, as the `ansible_version` dictionary roles
/// consult. The number lives in tests/golden/ANSIBLE_VERSION and nowhere else.
pub fn ansible_version() -> Value {
    let full = include_str!("../tests/golden/ANSIBLE_VERSION").trim();
    let mut parts = full.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    let (major, minor, revision) = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    serde_json::json!({ "full": full, "major": major, "minor": minor, "revision": revision, "string": full })
}

fn extend(target: &mut Map<String, Value>, source: &Map<String, Value>) {
    for (k, v) in source {
        target.insert(k.clone(), v.clone());
    }
}

/// `group_vars/` or `host_vars/`: for each entry, a file `name`, `name.yml`, `name.yaml`,
/// `name.json`, or a directory `name/` whose files are merged in name order. Hidden files,
/// editor backups and `.retry` files are ignored, like Ansible does.
fn load_vars_dir(dir: &Path) -> anyhow::Result<BTreeMap<String, Map<String, Value>>> {
    let mut out: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(out);
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if ignored(file_name) {
            continue;
        }
        let name = strip_vars_extension(file_name).to_string();
        let target = out.entry(name).or_default();
        if path.is_dir() {
            let mut inner: Vec<PathBuf> = std::fs::read_dir(&path)?
                .filter_map(Result::ok)
                .map(|e| e.path())
                .collect();
            inner.sort();
            for file in inner.into_iter().filter(|p| p.is_file()) {
                if file
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(ignored)
                {
                    continue;
                }
                extend(target, &load_vars_file(&file)?);
            }
        } else {
            extend(target, &load_vars_file(&path)?);
        }
    }
    Ok(out)
}

fn ignored(file_name: &str) -> bool {
    file_name.starts_with('.') || file_name.ends_with('~') || file_name.ends_with(".retry")
}

fn strip_vars_extension(file_name: &str) -> &str {
    for ext in [".yml", ".yaml", ".json"] {
        if let Some(stem) = file_name.strip_suffix(ext) {
            return stem;
        }
    }
    file_name
}

/// A YAML or JSON file whose document is a mapping. An empty file is an empty mapping.
pub fn load_vars_file(path: &Path) -> anyhow::Result<Map<String, Value>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let docs = yaml::load(&text, &path.display().to_string())?;
    match docs.first() {
        None => Ok(Map::new()),
        Some(doc) => match yaml::to_json(doc).with_context(|| format!("in {}", path.display()))? {
            Value::Object(map) => Ok(map),
            Value::Null => Ok(Map::new()),
            _ => bail!(
                "{}: a variables file must contain a mapping",
                path.display()
            ),
        },
    }
}

/// Every path an `include_vars` naming `name` looks in, in the order it looks.
///
/// Measured on ansible-core 2.19.12 by reading the list a missing file prints. From a task inside
/// a role whose `tasks/` wrote the statement, the six are `<role>/vars/`, `<role>/`,
/// `<role>/tasks/vars/`, `<role>/tasks/`, `<playbook>/vars/` and `<playbook>/`: three bases, each
/// with its `vars/` first. From a playbook task the role base is absent and the other two are the
/// same directory, which is why the reference's own message lists the playbook's two **twice** -
/// reproduced here rather than deduplicated, because the message is what the operator reads and
/// the list is the message.
///
/// An absolute name short-circuits the walk: it is the one path, and the only one worth naming.
pub fn include_vars_paths(
    file_dir: &Path,
    role_dir: Option<&Path>,
    playbook_dir: &Path,
    name: &str,
) -> Vec<PathBuf> {
    let named = Path::new(name);
    if named.is_absolute() {
        return vec![named.to_path_buf()];
    }
    let mut out = Vec::new();
    for base in role_dir.into_iter().chain([file_dir, playbook_dir]) {
        out.push(base.join("vars").join(name));
        out.push(base.join(name));
    }
    out
}

/// `-e` values: `key=value` pairs (strings), inline JSON or YAML mappings, and `@file`.
/// Later items win over earlier ones.
pub fn parse_extra_vars(items: &[String], base: &Path) -> anyhow::Result<Map<String, Value>> {
    let mut out = Map::new();
    for item in items {
        let item = item.trim();
        if let Some(file) = item.strip_prefix('@') {
            extend(&mut out, &load_vars_file(&base.join(file))?);
        } else if item.starts_with('{') || item.starts_with('[') {
            let docs = yaml::load(item, "extra vars")?;
            match docs.first().map(yaml::to_json).transpose()? {
                Some(Value::Object(map)) => extend(&mut out, &map),
                _ => bail!("extra vars must be a mapping: {item}"),
            }
        } else {
            for word in shlex::split(item).unwrap_or_default() {
                let (k, v) = word
                    .split_once('=')
                    .with_context(|| format!("extra var '{word}' is not key=value"))?;
                out.insert(k.to_string(), Value::String(v.to_string()));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tree() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "volant-vars-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::create_dir_all(dir.join("inventory/group_vars/web")).unwrap();
        std::fs::create_dir_all(dir.join("inventory/host_vars")).unwrap();
        std::fs::create_dir_all(dir.join("play/group_vars")).unwrap();
        std::fs::create_dir_all(dir.join("play/host_vars")).unwrap();
        std::fs::write(
            dir.join("inventory/hosts.ini"),
            "[web]\nweb1 tier=ini\n[web:vars]\nfrom=ini_group\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("inventory/group_vars/all.yml"),
            "layer: inv_all\nonly_inv_all: 1\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("inventory/group_vars/web/10-first.yml"),
            "layer: inv_web\nsplit: first\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("inventory/group_vars/web/20-second.yaml"),
            "split: second\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("inventory/host_vars/web1.json"),
            r#"{"layer": "inv_host"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("play/group_vars/all"), "play_all: yes\n").unwrap();
        std::fs::write(dir.join("play/host_vars/web1.yml"), "layer: play_host\n").unwrap();
        std::fs::write(dir.join("play/group_vars/.hidden.yml"), "hidden: true\n").unwrap();
        std::fs::write(dir.join("play/group_vars/all.yml~"), "backup: true\n").unwrap();
        dir
    }

    fn rand_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn store(dir: &Path) -> (Inventory, VarStore) {
        let inv = Inventory::load(&dir.join("inventory/hosts.ini")).unwrap();
        let store = VarStore::new(
            &inv,
            Some(&dir.join("inventory/hosts.ini")),
            &dir.join("play"),
            Map::new(),
        )
        .unwrap();
        (inv, store)
    }

    fn scope(hosts: &[&str]) -> Scope {
        let names: Vec<String> = hosts.iter().map(ToString::to_string).collect();
        Scope {
            play_hosts: names.clone(),
            batch_hosts: names.clone(),
            all_play_hosts: names,
            ..Scope::default()
        }
    }

    #[test]
    fn directories_layer_in_ansible_order() {
        let dir = tree();
        let (_, mut store) = store(&dir);
        let v = store.for_host("web1", &scope(&["web1"]));
        assert_eq!(
            v["layer"],
            json!("play_host"),
            "playbook host_vars is the highest directory source"
        );
        assert_eq!(v["only_inv_all"], json!(1));
        assert_eq!(
            v["split"],
            json!("second"),
            "files in a directory apply in name order"
        );
        assert_eq!(v["from"], json!("ini_group"));
        assert_eq!(v["tier"], json!("ini"));
        assert_eq!(
            v["play_all"],
            json!(true),
            "yes as data is a boolean too, matching PyYAML"
        );
        assert!(!v.contains_key("hidden") && !v.contains_key("backup"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn play_task_fact_and_extra_vars_stack_on_top() {
        let dir = tree();
        let inv = Inventory::load(&dir.join("inventory/hosts.ini")).unwrap();
        let extra: Map<String, Value> = json!({"layer": "extra"}).as_object().unwrap().clone();
        let mut store = VarStore::new(
            &inv,
            Some(&dir.join("inventory/hosts.ini")),
            &dir.join("play"),
            extra,
        )
        .unwrap();
        let mut sc = scope(&["web1"]);
        sc.play_vars = json!({"layer": "play", "p": 1})
            .as_object()
            .unwrap()
            .clone();
        sc.vars_files = vec![json!({"p": 2, "f": 1}).as_object().unwrap().clone()];
        sc.task_vars = json!({"f": 2, "t": 1}).as_object().unwrap().clone();
        store.set_fact("web1", "t", json!(2));
        store.set_fact("web1", "fact", json!("set"));
        let v = store.for_host("web1", &sc);
        assert_eq!(v["layer"], json!("extra"), "extra vars beat everything");
        assert_eq!(v["p"], json!(2), "vars_files beat play vars");
        assert_eq!(v["f"], json!(2), "task vars beat vars_files");
        assert_eq!(v["t"], json!(2), "set_fact beats task vars");
        assert_eq!(v["fact"], json!("set"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Where a role's three layers sit, one assertion per measured relation.
    ///
    /// Every value quoted here was read off ansible-core 2.19.12 on the development machine:
    /// `defaults/main.yml` loses to an inventory host variable (`d=inv-host`), `vars/main.yml`
    /// beats the play's `vars:` and loses to a task's `vars:` (`shared=task-var` with the role
    /// still reading `v=role-var`), a role parameter beats a `set_fact` (`p=param` against a
    /// fact of `fact`) and loses to `-e` (`p=extra`).
    ///
    /// What would make this red: any one layer applied at the wrong rank. Each assertion pins
    /// one boundary, so a layer that slides past its neighbour names itself rather than leaving
    /// a single "the role's value appears" to pass on a merge that is wrong everywhere else.
    #[test]
    fn the_role_layers_sit_where_the_reference_puts_them() {
        let inv = Inventory::parse_ini("h1 d=inv-host\n").unwrap();
        let role = |extra: Map<String, Value>| Scope {
            role_defaults: json!({"d": "role-default", "shared": "role-default"})
                .as_object()
                .unwrap()
                .clone(),
            role_vars: json!({"v": "role-var", "shared": "role-var"})
                .as_object()
                .unwrap()
                .clone(),
            role_params: json!({"p": "param"}).as_object().unwrap().clone(),
            play_vars: json!({"shared": "play-var"}).as_object().unwrap().clone(),
            task_vars: extra,
            ..Scope::default()
        };

        let mut store = VarStore::new(&inv, None, Path::new("."), Map::new()).unwrap();
        store.set_fact("h1", "p", json!("fact"));
        let v = store.for_host("h1", &role(Map::new()));
        assert_eq!(
            v["d"],
            json!("inv-host"),
            "a role default loses to an inventory variable"
        );
        assert_eq!(
            v["shared"],
            json!("role-var"),
            "a role's vars beat the play's own vars:"
        );
        assert_eq!(
            v["p"],
            json!("param"),
            "a role parameter beats a fact of the same name"
        );

        let task = json!({"shared": "task-var"}).as_object().unwrap().clone();
        let v = store.for_host("h1", &role(task));
        assert_eq!(
            v["shared"],
            json!("task-var"),
            "a task's vars: beat a role's vars"
        );
        assert_eq!(
            v["v"],
            json!("role-var"),
            "and leave the rest of them alone"
        );

        let extra: Map<String, Value> = json!({"p": "extra"}).as_object().unwrap().clone();
        let mut store = VarStore::new(&inv, None, Path::new("."), extra).unwrap();
        let v = store.for_host("h1", &role(Map::new()));
        assert_eq!(
            v["p"],
            json!("extra"),
            "an extra var beats a role parameter"
        );
    }

    #[test]
    fn magic_variables_are_present() {
        let dir = tree();
        let (_, mut store) = store(&dir);
        let v = store.for_host("web1", &scope(&["web1"]));
        assert_eq!(v["inventory_hostname"], json!("web1"));
        assert_eq!(v["inventory_hostname_short"], json!("web1"));
        assert_eq!(v["group_names"], json!(["web"]));
        for name in [
            "hostvars",
            "groups",
            "ansible_play_hosts",
            "ansible_play_hosts_all",
            "ansible_play_batch",
            "play_hosts",
        ] {
            assert!(
                v.get(name).is_none(),
                "{name} is handed to templates as a shared view, not merged into the map"
            );
        }
        let shared = store.shared_values(&scope(&["web1"]));
        assert_eq!(shared["groups"]["web"], json!(["web1"]));
        assert_eq!(shared["ansible_play_hosts"], json!(["web1"]));
        assert_eq!(shared["ansible_play_hosts_all"], json!(["web1"]));
        assert_eq!(shared["ansible_play_batch"], json!(["web1"]));
        // Asking the host's variables by name, which is what an argument spec does, answers for
        // the five as well: they are somewhere else, not gone.
        let host = HostVars {
            map: v.clone(),
            hostvars: Arc::default(),
            shared,
            ..HostVars::default()
        };
        assert_eq!(
            host.get("groups").map(|g| &g["web"]),
            Some(&json!(["web1"]))
        );
        assert_eq!(host.get("inventory_hostname"), Some(&json!("web1")));
        assert_eq!(
            v["playbook_dir"],
            json!(dir.join("play").display().to_string())
        );
        assert_eq!(
            v["inventory_dir"],
            json!(dir.join("inventory").display().to_string())
        );
        assert_eq!(
            v["inventory_file"],
            json!(dir.join("inventory/hosts.ini").display().to_string())
        );
        assert_eq!(v["omit"], json!(omit_token()));
        assert_eq!(v["ansible_check_mode"], json!(false));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The four host lists are three different answers: the play's resolved hosts never move,
    /// the play's live hosts lose whoever failed, and the batch's live hosts are narrower still.
    ///
    /// Measured on ansible-core 2.19.12 with `serial: 2` over `h1, h2, h3`, `h1` failing in the
    /// first batch: `h2` then reads `ansible_play_batch` as `['h2']`, `ansible_play_hosts` as
    /// `['h2', 'h3']` and `ansible_play_hosts_all` as all three, and the deprecated `play_hosts`
    /// answers with the batch.
    ///
    /// What would make this red: `ansible_play_batch` or `play_hosts` served from the play's
    /// live list, which is what they were before `serial` existed - a task counting the hosts of
    /// its own batch would then count the ones waiting for the next one.
    #[test]
    fn the_batch_the_live_play_and_the_resolved_play_are_three_lists() {
        let inv = Inventory::parse_ini("[web]\nh1\nh2\nh3\n").unwrap();
        let mut store = VarStore::new(&inv, None, Path::new("."), Map::new()).unwrap();
        let v = store.shared_values(&Scope {
            play_hosts: vec!["h2".into(), "h3".into()],
            batch_hosts: vec!["h2".into()],
            all_play_hosts: vec!["h1".into(), "h2".into(), "h3".into()],
            ..Scope::default()
        });
        assert_eq!(v["ansible_play_hosts"], json!(["h2", "h3"]));
        assert_eq!(v["ansible_play_batch"], json!(["h2"]));
        assert_eq!(v["play_hosts"], json!(["h2"]));
        assert_eq!(v["ansible_play_hosts_all"], json!(["h1", "h2", "h3"]));
        // The map is cached, and the next batch is a different answer: asking again with other
        // lists has to rebuild it. What would make this red: a cache that never checks its key.
        let next = store.shared_values(&Scope {
            play_hosts: vec!["h3".into()],
            batch_hosts: vec!["h3".into()],
            all_play_hosts: vec!["h1".into(), "h2".into(), "h3".into()],
            ..Scope::default()
        });
        assert_eq!(next["ansible_play_hosts"], json!(["h3"]));
        assert_eq!(next["ansible_play_batch"], json!(["h3"]));
    }

    /// The view `hostvars` reads: one shared map for every host of the inventory, and a
    /// completed one for a host the inventory does not carry.
    ///
    /// What would make this red: a host's own identity dropped from its view, a view that
    /// nests `hostvars` inside itself, an implicit `localhost` left out of the map it is about
    /// to render against, or two hosts of the inventory handed two different maps.
    #[test]
    fn the_shared_hostvars_view_answers_for_every_host() {
        let dir = tree();
        let (_, mut store) = store(&dir);
        let view = store.hostvars_shared("web1");
        assert_eq!(view["web1"]["tier"], json!("ini"));
        assert!(
            view["web1"].get("hostvars").is_none(),
            "hostvars does not nest"
        );
        assert_eq!(
            view["web1"]["inventory_hostname"],
            json!("web1"),
            "the reference exposes a host's own identity through hostvars"
        );
        assert!(
            Arc::ptr_eq(&view, &store.hostvars_shared("web1")),
            "the inventory's hosts share one map"
        );
        let implicit = store.hostvars_shared("localhost");
        assert!(
            !view.contains_key("localhost"),
            "an implicit localhost is in no group"
        );
        assert_eq!(
            implicit["localhost"]["inventory_hostname"],
            json!("localhost")
        );
        assert_eq!(
            implicit["web1"]["tier"],
            json!("ini"),
            "and it still sees the inventory"
        );
    }

    #[test]
    fn a_short_hostname_drops_the_domain() {
        let inv = Inventory::parse_ini("web1.example.com\n").unwrap();
        let mut store = VarStore::new(&inv, None, Path::new("."), Map::new()).unwrap();
        let v = store.for_host("web1.example.com", &scope(&["web1.example.com"]));
        assert_eq!(v["inventory_hostname_short"], json!("web1"));
    }

    #[test]
    fn a_host_that_shares_a_group_name_still_gets_its_variables() {
        // Ansible warns "Found both group and host with same name" and runs the host.
        let inv = Inventory::parse_ini("[web]\ndb x=1\n[db]\n[web:vars]\nfrom=group\n").unwrap();
        let mut store = VarStore::new(&inv, None, Path::new("."), Map::new()).unwrap();
        let v = store.for_host("db", &scope(&["db"]));
        assert_eq!(v["inventory_hostname"], json!("db"));
        assert_eq!(v["x"], json!(1));
        assert_eq!(v["from"], json!("group"));
    }

    #[test]
    fn extra_vars_take_key_value_json_and_files() {
        let dir = tree();
        std::fs::write(dir.join("extra.yml"), "from_file: 1\nlayer: file\n").unwrap();
        let parsed = parse_extra_vars(
            &[
                "a=1 b=two".to_string(),
                r#"{"c": [1]}"#.to_string(),
                "@extra.yml".to_string(),
            ],
            &dir,
        )
        .unwrap();
        assert_eq!(parsed["a"], json!("1"), "key=value extra vars are strings");
        assert_eq!(parsed["b"], json!("two"));
        assert_eq!(parsed["c"], json!([1]));
        assert_eq!(parsed["from_file"], json!(1));
        assert_eq!(parsed["layer"], json!("file"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ansible_version_comes_from_the_reference_file() {
        let v = ansible_version();
        let reference = include_str!("../tests/golden/ANSIBLE_VERSION").trim();
        assert_eq!(v["full"], json!(reference));
        assert_eq!(v["major"], json!(2));
    }

    #[test]
    fn a_broken_vars_file_names_itself() {
        let dir = tree();
        std::fs::write(dir.join("play/group_vars/all.yml"), "a: [1,\n").unwrap();
        let inv = Inventory::load(&dir.join("inventory/hosts.ini")).unwrap();
        let err = VarStore::new(
            &inv,
            Some(&dir.join("inventory/hosts.ini")),
            &dir.join("play"),
            Map::new(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("all.yml"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
