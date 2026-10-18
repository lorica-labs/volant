// SPDX-License-Identifier: GPL-3.0-or-later
//! Variable sources and their precedence, from inventory groups up to extra vars, plus the
//! magic variables Ansible defines for every host.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use serde_json::{Map, Value};

use crate::bridge::Bridge;
use crate::inventory::{Inventory, PluginConfig, Source, is_localhost};
use crate::stats::CustomStats;
use crate::template::{Templar, Vars};
use crate::yaml;

/// One host's merged variables, with the inventory-wide view its templates read `hostvars`
/// from. The two travel together because every render a host does needs both, and the view is
/// shared rather than copied into the map: see `crate::template::Vars`.
///
/// There is deliberately no `Deref` to `map`: a gathered fact is in `facts`, so a caller that
/// reads the map as the host's variables misses it. `get` reads all three.
#[derive(Debug, Clone, Default)]
pub struct HostVars {
    pub map: Map<String, Value>,
    pub hostvars: Arc<Map<String, Value>>,
    /// `groups` and the play's host lists, shared for the same reason and in the same way.
    pub shared: Arc<Map<String, Value>>,
    /// The host's gathered facts that answer, shared with the store rather than copied: read
    /// after `shared` and before `map`. See `VarStore::layered_for_host`.
    pub facts: Arc<Map<String, Value>>,
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
    /// the merge produced, the inventory-wide values and the facts included: a name that
    /// collides with one of them leaves this host's copy of that layer, which is how the host's
    /// own value gets to answer.
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
        if self.facts.contains_key(&key) {
            let mut facts = Map::clone(&self.facts);
            facts.shift_remove(&key);
            self.facts = Arc::new(facts);
        }
        self.map.insert(key, value)
    }

    /// Lays the `ansible_facts` a module just returned over these variables, as ansible-core's
    /// `TaskExecutor._execute` does to the copy `changed_when`, `failed_when` and `until` read:
    /// the flat names through `clean_facts()`, and the namespaced copy one level down, under
    /// `ansible_facts.ansible_facts`. That nesting is the reference's own: it hands
    /// `namespace_facts()`, which returns `{'ansible_facts': ...}`, straight to `combine_vars`.
    /// Measured on ansible-core 2.19.12: a module returning `ansible_y` is `ansible_y` and
    /// `ansible_facts.ansible_facts.y` to its own `failed_when`, and `ansible_facts.y` is
    /// undefined there. Every name is a managed host's.
    pub fn returned_facts(&mut self, facts: &Map<String, Value>) {
        let (flat, namespaced, _) = split_facts(facts);
        let mut namespace = self
            .get("ansible_facts")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        namespace.insert("ansible_facts".into(), Value::Object(namespaced));
        self.insert_untrusted("ansible_facts".into(), Value::Object(namespace));
        for (key, value) in flat {
            self.insert_untrusted(key, value);
        }
    }

    /// Reads a name the way a template reads it: the inventory-wide values first, because they
    /// were merged last, then the facts no layer of the map outranks, then the host's own map.
    /// Everything that asks for a name by hand rather than through a render goes through here,
    /// so the two answer alike.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.shared
            .get(key)
            .or_else(|| self.facts.get(key))
            .or_else(|| self.map.get(key))
    }
}

impl<'a> From<&'a HostVars> for Vars<'a> {
    fn from(vars: &'a HostVars) -> Self {
        Vars {
            map: &vars.map,
            hostvars: Some(&vars.hostvars),
            shared: Some(&vars.shared),
            facts: Some(&vars.facts),
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
    /// Group vars from `group_vars/` directories: one per inventory source, in source order,
    /// then the playbook's, last.
    group_files: Vec<BTreeMap<String, Map<String, Value>>>,
    /// Host vars from `host_vars/` directories, in the same order as `group_files`.
    host_files: Vec<BTreeMap<String, Map<String, Value>>>,
    inventory_vars: BTreeMap<String, Map<String, Value>>,
    host_line_vars: BTreeMap<String, Map<String, Value>>,
    group_names: BTreeMap<String, Vec<String>>,
    groups: BTreeMap<String, Vec<String>>,
    facts: BTreeMap<String, Map<String, Value>>,
    /// What modules returned under `ansible_facts`, flat and namespaced: the reference's host
    /// facts, a layer of their own that ranks over the inventory and under the play. Every name
    /// in it is untrusted.
    /// Shared: a host's task reads them through its own `Arc` rather than a copy.
    gathered: BTreeMap<String, Arc<Map<String, Value>>>,
    /// Restricted fact names already warned about: the reference says each once per run.
    warned_restricted: BTreeSet<String>,
    /// What the fact cache holds per host: the gathered facts as the modules returned them, and
    /// the `set_fact` ones that said `cacheable`, tagged as the file holds them. A host with an
    /// entry is a host `gathering = smart` does not gather again.
    persisted: BTreeMap<String, Map<String, Value>>,
    /// The `jsonfile` cache the entries are written to, when the run has one.
    fact_cache: Option<crate::fact_cache::FactCache>,
    /// `gathering = smart`.
    smart: bool,
    /// A cache file that could not be read, reported when the first play starts.
    cache_failure: Option<crate::fact_cache::CacheFailure>,
    /// Of the `set_fact` layer's facts, the names that came from a managed host rather than
    /// from the playbook.
    untrusted: BTreeMap<String, BTreeSet<String>>,
    extra: Map<String, Value>,
    /// What `ansible_forks` reports, which the reference sets from the run's own `forks`.
    forks: usize,
    /// What `ansible_run_tags` and `ansible_skip_tags` report: the run's own tag selection.
    run_tags: Vec<String>,
    skip_tags: Vec<String>,
    /// What `ansible_check_mode` and `ansible_diff_mode` report: `--check` and `--diff`, never a
    /// play's or a task's own keyword, measured.
    check_mode: bool,
    diff_mode: bool,
    /// Every inventory host's view, as `hostvars` shows it. Built on demand; a write to one host's
    /// facts marks that host in `stale` and only its entry is rebuilt, while a rebase drops the
    /// whole map.
    hostvars: Option<Arc<Map<String, Value>>>,
    /// Hosts whose entry in `hostvars` no longer matches their facts.
    stale: BTreeSet<String>,
    /// Per host whose facts some layer above them outranks, the facts less those names, kept
    /// with the names it leaves out. A host none of whose facts is outranked needs no entry: its
    /// tasks share the gathered map itself. Dropped when the host gathers again, and when it is
    /// no longer in the play.
    host_facts: BTreeMap<String, HostFacts>,
    /// How many times each host's `host_facts` entry was built, for the test that proves one host
    /// leaving the play costs the others nothing.
    #[cfg(test)]
    facts_built: BTreeMap<String, usize>,
    /// How many times each host's `hostvars` entry was built, for the test that proves a write
    /// on one host leaves the other hosts' entries alone.
    #[cfg(test)]
    views_built: BTreeMap<String, usize>,
    /// The same view completed with a host the inventory does not carry - an implicit
    /// `localhost` - one map per such host. There is normally at most one of them in a run.
    hostvars_with: BTreeMap<String, Arc<Map<String, Value>>>,
    /// The hosts that hold at least one untrusted name, built on demand and dropped by the same
    /// rule as `hostvars`: every write that can change it already goes through `set_fact`.
    untrusted_hosts: Option<Arc<BTreeSet<String>>>,
    /// The inventory-wide values, with the three host lists they were built from. `groups` only
    /// moves when `meta: refresh_inventory` reads the inventory again, but the lists come from the coordinator's last published progress and
    /// move whenever a host drops out, so the map is good for one triple and no longer.
    shared: Option<(SharedKey, Arc<Map<String, Value>>)>,
    /// See [`VarStore::templated_facts`]. Never goes back to `false`: the fact outlives the play.
    templated_facts: bool,
    /// The inventory as `add_host`, `group_by` and a refresh left it: the one the plays behind
    /// the current one resolve their hosts against.
    dynamic: Inventory,
    /// What `add_host` and `group_by` did, in order, which a refresh applies again to the
    /// sources it reads anew.
    added: Vec<Added>,
    /// The hosts the play's pattern found after a refresh, which `ansible_play_hosts_all` shows
    /// in place of the play's starting list until the play ends.
    play_hosts_all: Option<Vec<String>>,
    /// The hosts `--limit` narrows the run to, which a refresh applies to the play's hosts too.
    limit: Option<HashSet<String>>,
    /// The sources and plugin settings a refresh reads again.
    reload: Option<(Vec<Source>, PluginConfig, Arc<Bridge>)>,
    /// Whether `dynamic` moved since the command line last took it.
    inventory_changed: bool,
    /// What `set_stats` collected, printed after the recap when the run asks for it.
    custom_stats: CustomStats,
}

/// One change a play made to the inventory.
#[derive(Debug)]
enum Added {
    Host(String, BTreeMap<String, Value>),
    Member(String, String),
    Child(String, String),
}

/// Whether any string in `value`, at any depth, is a template.
pub(crate) fn holds_template(value: &Value) -> bool {
    match value {
        Value::String(text) => Templar::is_template(text),
        Value::Array(items) => items.iter().any(holds_template),
        Value::Object(map) => map.values().any(holds_template),
        _ => false,
    }
}

/// The play and batch host lists one `shared` map was built from.
type SharedKey = (Vec<String>, Vec<String>, Vec<String>);

/// One host's gathered facts less the names a layer above them answers for, and those names.
#[derive(Debug)]
struct HostFacts {
    shadowed: BTreeSet<String>,
    map: Arc<Map<String, Value>>,
}

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

/// The value Ansible substitutes for `omit`: a parameter equal to it is dropped from the task.
/// Ansible generates a fresh random suffix per run; a stable one changes nothing for playbooks
/// and keeps output reproducible.
pub fn omit_token() -> &'static str {
    "__omit_place_holder__6d24c2dd6a8f2e0e2a6e5cce6b1f9c4a"
}

/// Reads one host variable that decides where, as whom or under what a task runs: the
/// transport, the escalation and the interpreter choice read theirs through here and nowhere
/// else. A managed host must never be able to set such a name for itself, so every name read
/// here has to be one `restricted_fact` strips from gathered facts. The test
/// `every_host_setting_is_stripped_from_gathered_facts` reads the names off the source, so a
/// new reader is covered the day it is written.
pub fn host_setting<'a, V: HostSettings + ?Sized>(vars: &'a V, name: &str) -> Option<&'a Value> {
    vars.setting(name)
}

/// The three shapes a host's variables are read from: the merged map, a task's view of it, and
/// the inventory object the pre-flight reads.
pub trait HostSettings {
    fn setting(&self, name: &str) -> Option<&Value>;
}

impl HostSettings for Map<String, Value> {
    fn setting(&self, name: &str) -> Option<&Value> {
        self.get(name)
    }
}

impl HostSettings for BTreeMap<String, Value> {
    fn setting(&self, name: &str) -> Option<&Value> {
        self.get(name)
    }
}

impl HostSettings for HostVars {
    fn setting(&self, name: &str) -> Option<&Value> {
        self.get(name)
    }
}

/// The connection plugins ansible-core 2.19.12 ships, as its `connection_loader.all()` lists
/// them on a host with no collection installed.
const CONNECTION_PLUGINS: [&str; 6] = [
    "_paramiko_ssh",
    "local",
    "paramiko_ssh",
    "psrp",
    "ssh",
    "winrm",
];

/// ansible-core 2.19.12's `MAGIC_VARIABLE_MAPPING` values, which include every
/// `COMMON_CONNECTION_VARS` name, then `RESTRICTED_RESULT_KEYS` and `INTERNAL_RESULT_KEYS`.
const RESTRICTED_FACTS: [&str; 39] = [
    "ansible_become",
    "ansible_become_exe",
    "ansible_become_flags",
    "ansible_become_method",
    "ansible_become_pass",
    "ansible_become_password",
    "ansible_become_user",
    "ansible_connection",
    "ansible_connection_user",
    "ansible_docker_extra_args",
    "ansible_host",
    "ansible_module_compression",
    "ansible_network_os",
    "ansible_password",
    "ansible_pipelining",
    "ansible_port",
    "ansible_private_key_file",
    "ansible_scp_extra_args",
    "ansible_sftp_extra_args",
    "ansible_shell_executable",
    "ansible_shell_type",
    "ansible_ssh_common_args",
    "ansible_ssh_executable",
    "ansible_ssh_extra_args",
    "ansible_ssh_host",
    "ansible_ssh_pass",
    "ansible_ssh_pipelining",
    "ansible_ssh_port",
    "ansible_ssh_private_key_file",
    "ansible_ssh_timeout",
    "ansible_ssh_transfer_method",
    "ansible_ssh_user",
    "ansible_timeout",
    "ansible_user",
    "ansible_rsync_path",
    "ansible_playbook_python",
    "ansible_facts",
    "add_host",
    "add_group",
];

/// Names this engine strips although ansible-core keeps them, because it reads them where the
/// reference does not. `ansible_remote_tmp` is where the agent is cached, and for an escalated
/// link that is the target user's own `remote_tmp`, checked only by the version line the cached
/// file prints: a connecting user who could name it could plant a script there that `sudo` then
/// runs as the `become_user`. Under the reference the same name only moves the connecting user's
/// own module files, which that user can already change.
///
/// `ansible_package_use` is here for the scan's sake rather than for a privilege: `package` reads
/// it through [`host_setting`] like every other name that picks what runs, so the scan guarding
/// this list stays total. A host that set it could only pick another backend of a closed list,
/// which its own `pkg_mgr` fact already does.
///
/// `ansible_async_dir` is where a job writes its status file and the directory `async_status`
/// removes one from in `cleanup`: a host that named it for itself would choose where the agent,
/// escalated or not, writes and deletes.
const ENGINE_RESTRICTED_FACTS: [&str; 3] = [
    "ansible_remote_tmp",
    "ansible_package_use",
    "ansible_async_dir",
];

/// Whether ansible-core's `clean_facts()` removes a fact of this name before it becomes a
/// variable: a connection or escalation setting, `ansible_<connection plugin>_*` unless it ends
/// in `_bridge` or `_gwbridge`, any `ansible_become_*`, any `ansible_*_interpreter`, and the
/// engine's own result keys - except `ansible_ssh_host_key_*`, which `setup` reports and which
/// it always keeps.
fn restricted_fact(key: &str) -> bool {
    if key.starts_with("ansible_ssh_host_key_") {
        return false;
    }
    let plugin_setting = CONNECTION_PLUGINS.iter().any(|plugin| {
        key.strip_prefix("ansible_")
            .and_then(|rest| rest.strip_prefix(plugin))
            .is_some_and(|rest| rest.starts_with('_'))
    }) && !key.ends_with("_bridge")
        && !key.ends_with("_gwbridge");
    // `^ansible_.*_interpreter$`: both underscores are its own, so `ansible_interpreter` is not.
    let interpreter = key.len() >= "ansible__interpreter".len()
        && key.starts_with("ansible_")
        && key.ends_with("_interpreter");
    RESTRICTED_FACTS.contains(&key)
        || ENGINE_RESTRICTED_FACTS.contains(&key)
        || key.starts_with("ansible_become_")
        || plugin_setting
        || interpreter
}

/// ansible-core's `strip_internal_keys()`: every `_ansible_*` key, at any depth, goes without a
/// word.
fn strip_internal_keys(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|key, _| !key.starts_with("_ansible_"));
            map.values_mut().for_each(strip_internal_keys);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_internal_keys),
        _ => {}
    }
}

/// A module's `ansible_facts` the two ways the reference files them: the names that land flat,
/// through `clean_facts()`, and the copy `namespace_facts()` makes, the prefix `setup` returned
/// taken off except on `ansible_local`. The third list holds the restricted names
/// `clean_facts()` took out of the flat half.
fn split_facts(
    facts: &Map<String, Value>,
) -> (Map<String, Value>, Map<String, Value>, Vec<String>) {
    let mut flat = Map::new();
    let mut namespaced = Map::new();
    let mut restricted = Vec::new();
    for (key, value) in facts {
        if restricted_fact(key) {
            restricted.push(key.clone());
        } else if !key.starts_with("_ansible_") {
            let mut value = value.clone();
            strip_internal_keys(&mut value);
            flat.insert(key.clone(), value);
        }
        let bare = match key.strip_prefix("ansible_") {
            Some(bare) if key != "ansible_local" => bare,
            _ => key,
        };
        namespaced.insert(bare.to_string(), value.clone());
    }
    (flat, namespaced, restricted)
}

/// What a host that has gathered nothing, or whose facts were cleared, answers for its facts:
/// `ansible_facts` is there and empty. Measured on ansible-core 2.19.12, `ansible_facts | length`
/// reads 0 on a host that never ran `setup`, and `ansible_facts is defined` is true.
fn no_facts() -> &'static Arc<Map<String, Value>> {
    static EMPTY: std::sync::OnceLock<Arc<Map<String, Value>>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(|| {
        Arc::new(Map::from_iter([(
            "ansible_facts".to_string(),
            Value::Object(Map::new()),
        )]))
    })
}

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

/// An inventory name without its domain, which is what `inventory_hostname_short` reports.
fn short_name(host: &str) -> String {
    host.split('.').next().unwrap_or(host).to_string()
}

impl VarStore {
    /// A store over one inventory file, or none.
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

    /// Writes a fact the playbook wrote: `include_vars`, and the engine's own bookkeeping. The
    /// name gets its trust back, because a write from an author-side source is author content
    /// whatever the name held before.
    ///
    /// A value still holding a template is rendered again by every task that reads it, and what
    /// it names can be another host's variable; see [`VarStore::templated_facts`].
    pub fn set_fact(&mut self, host: &str, key: &str, value: Value) {
        self.templated_facts |= holds_template(&value);
        self.write_fact(host, key, value);
    }

    /// Whether the playbook has written, while the run went, a fact that is still a template: an
    /// `include_vars` file whose values render, most often. Nothing before the run could read
    /// what such a value names, so from then on the executor makes every step a boundary.
    pub fn templated_facts(&self) -> bool {
        self.templated_facts
    }

    fn write_fact(&mut self, host: &str, key: &str, value: Value) {
        if let Some(names) = self.untrusted.get_mut(host) {
            names.remove(key);
        }
        self.facts
            .entry(host.to_string())
            .or_default()
            .insert(key.to_string(), value);
        self.forget_host(host);
    }

    /// Writes a fact that came from a managed host: a module result, a `register`, a `set_fact`.
    /// The name is data from here on, and a template that reads it is not rendered again.
    pub fn set_untrusted_fact(&mut self, host: &str, key: &str, value: Value) {
        // After, not before: `write_fact` clears the name, because a write from an author-side
        // source gives the trust back. The cache `write_fact` dropped covers this write too,
        // because nothing can read it between the two lines. Not `set_fact`: a template a host
        // returned is data and is never rendered, so it reads nothing.
        self.write_fact(host, key, value);
        self.untrusted
            .entry(host.to_string())
            .or_default()
            .insert(key.to_string());
    }

    /// Merges the `ansible_facts` a module returned into one host's facts, under both the names
    /// the reference gives them.
    ///
    /// Each key lands flat as the module spelled it, **and** under `ansible_facts` without the
    /// `ansible_` prefix `setup` puts on every name it returns: ansible-core's `clean_facts()`
    /// adds no prefix and `namespace_facts()` takes it off. So `ansible_distribution` is flat and
    /// `distribution` namespaced, and a bare `module_setup` or `packages` is bare in both.
    /// Measurement 10 of plan 1.5 is what settles the pair: `gather_facts: true` followed by a
    /// `set_fact: ansible_hostname: SHADOWED` leaves both readable, the flat name masked and
    /// `ansible_facts.hostname` still holding what the host reported. So the two are separate
    /// names and a later write to one leaves the other alone. Writing only the flat half breaks
    /// `ansible_facts['hostname']`, which is how playbooks that survived the injection being
    /// turned off read a fact.
    ///
    /// The namespace is merged rather than replaced: a run gathers with `setup` and then asks
    /// `package_facts` for more, and the second must not drop what the first found.
    ///
    /// Untrusted, every one of them, and this is the entry point that makes that true for a whole
    /// module result at once. They are a managed host's own words - a hostname the host chose, a
    /// package name it printed - so a template in a value is text from here on.
    ///
    /// Never flat under a restricted name: a managed host must not choose the address, user,
    /// `ssh` arguments or interpreter of its own next task, and gathered facts rank over the
    /// inventory that normally sets them. `restricted_fact` is ansible-core's `clean_facts()`;
    /// what it takes out is returned, the names not yet warned about in this run, for the
    /// caller's `Removed restricted key from module data` warning. The namespaced copy keeps
    /// them, as the reference's does: nothing reads a connection setting from there.
    pub fn gather_facts(&mut self, host: &str, facts: &Map<String, Value>) -> Vec<String> {
        let removed = self.merge_gathered(host, facts);
        if !self.caching() {
            return removed;
        }
        self.persisted
            .entry(host.to_string())
            .or_default()
            .extend(facts.iter().map(|(k, v)| (k.clone(), v.clone())));
        self.write_cache(host);
        removed
    }

    /// Sets the fact cache the run writes to and whether `gathering` is `smart`.
    pub fn set_fact_cache(&mut self, cache: Option<crate::fact_cache::FactCache>, smart: bool) {
        self.fact_cache = cache;
        self.smart = smart;
    }

    pub fn fail_cache(&mut self, failure: crate::fact_cache::CacheFailure) {
        self.cache_failure = Some(failure);
    }

    pub fn take_cache_failure(&mut self) -> Option<crate::fact_cache::CacheFailure> {
        self.cache_failure.take()
    }

    pub fn fact_cache(&self) -> Option<&crate::fact_cache::FactCache> {
        self.fact_cache.as_ref()
    }

    /// Takes what the cache held for `host` as that host's gathered facts, without writing it
    /// back.
    pub fn load_cached(&mut self, host: &str, facts: Map<String, Value>) {
        self.merge_gathered(host, &facts);
        self.persisted.insert(host.to_string(), facts);
    }

    /// Whether anything reads `persisted` back: a `jsonfile` cache, or `gathering = smart`.
    fn caching(&self) -> bool {
        self.fact_cache.is_some() || self.smart
    }

    /// Whether `gathering = smart` has no need to gather `host`: the cache holds facts a gather
    /// left, which the reference marks with `_ansible_facts_gathered`.
    pub fn skips_gathering(&self, host: &str) -> bool {
        self.smart
            && self
                .persisted
                .get(host)
                .and_then(|facts| facts.get("_ansible_facts_gathered"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
    }

    /// Adds a `set_fact` that said `cacheable: true` to `host`'s cache entry.
    /// The file is written by [`VarStore::write_cache`], once per task.
    pub fn cache_fact(&mut self, host: &str, key: &str, value: &Value) {
        if !self.caching() {
            return;
        }
        self.persisted
            .entry(host.to_string())
            .or_default()
            .insert(key.to_string(), crate::fact_cache::tag(value));
    }

    pub fn write_cache(&self, host: &str) {
        if let (Some(cache), Some(entry)) = (&self.fact_cache, self.persisted.get(host))
            && let Err(err) = cache.write(host, entry)
        {
            eprintln!("[WARNING]: Unable to write the fact cache entry of {host}: {err}");
        }
    }

    fn merge_gathered(&mut self, host: &str, facts: &Map<String, Value>) -> Vec<String> {
        // Before the write, so the store holds the only reference it can: a task still holding
        // the gathered map keeps its snapshot, and this write goes to a copy.
        self.host_facts.remove(host);
        let gathered = Arc::make_mut(self.gathered.entry(host.to_string()).or_default());
        let mut namespace = gathered
            .get("ansible_facts")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let (flat, namespaced, restricted) = split_facts(facts);
        gathered.extend(flat);
        namespace.extend(namespaced);
        gathered.insert("ansible_facts".into(), Value::Object(namespace));
        self.forget_host(host);
        let mut removed: Vec<String> = restricted
            .into_iter()
            .filter(|key| self.warned_restricted.insert(key.clone()))
            .collect();
        // The reference's order is a Python set's; name order at least reads the same each run.
        removed.sort();
        removed
    }

    /// Whether `host` is the `localhost` the reference conjures because the inventory has none.
    pub fn is_implicit_localhost(&self, host: &str) -> bool {
        !self.inventory_vars.contains_key(host) && is_localhost(host)
    }

    /// Whether every one of `hosts` already holds `key` among the facts a module returned.
    pub fn all_gathered(&self, hosts: &[String], key: &str) -> bool {
        hosts.iter().all(|host| {
            self.gathered
                .get(host)
                .is_some_and(|facts| facts.contains_key(key))
        })
    }

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
    fn forget_hostvars(&mut self) {
        self.hostvars = None;
        self.stale.clear();
        self.hostvars_with.clear();
        self.untrusted_hosts = None;
    }

    /// What a write to one host's facts leaves behind: that host's `hostvars` entry is out of
    /// date, and nobody else's is. The completed maps of hosts outside the inventory carry every
    /// host's entry and go whole; there is normally at most one of them.
    fn forget_host(&mut self, host: &str) {
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

    /// The magic variables that belong to one host. The inventory-wide ones are not here: see
    /// `shared_values`.
    fn magic(&self, host: &str) -> Map<String, Value> {
        let mut vars = Map::new();
        let short = short_name(host);
        let mut group_names = self.group_names.get(host).cloned().unwrap_or_default();
        group_names.sort();
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
        // The file the host was read from, as the reference records it on the host.
        let own = self.dynamic.source_of(host);
        let file = own.or(self.inventory_file.as_deref());
        let dir = own.and_then(Path::parent).or(self.inventory_dir.as_deref());
        if let Some(dir) = dir {
            vars.insert(
                "inventory_dir".to_string(),
                Value::String(dir.display().to_string()),
            );
        }
        if let Some(file) = file {
            vars.insert(
                "inventory_file".to_string(),
                Value::String(file.display().to_string()),
            );
        }
        vars.insert("omit".to_string(), Value::String(omit_token().to_string()));
        vars.insert(
            "ansible_check_mode".to_string(),
            Value::Bool(self.check_mode),
        );
        vars.insert("ansible_diff_mode".to_string(), Value::Bool(self.diff_mode));
        vars.insert("ansible_forks".to_string(), Value::from(self.forks));
        vars.insert(
            "ansible_run_tags".to_string(),
            Value::from(self.run_tags.clone()),
        );
        vars.insert(
            "ansible_skip_tags".to_string(),
            Value::from(self.skip_tags.clone()),
        );
        vars.insert("ansible_version".to_string(), ansible_version());
        vars.insert(
            "volant_version".to_string(),
            Value::String(env!("CARGO_PKG_VERSION").to_string()),
        );
        vars
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

/// `extend`, leaving out the names in `skip`.
fn extend_except(
    target: &mut Map<String, Value>,
    source: &Map<String, Value>,
    skip: &BTreeSet<String>,
) {
    for (k, v) in source {
        if !skip.contains(k) {
            target.insert(k.clone(), v.clone());
        }
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
                extend(target, &load_group_file(&file)?);
            }
        } else {
            extend(target, &load_group_file(&path)?);
        }
    }
    Ok(out)
}

/// A `group_vars` or `host_vars` file. The reference stops a vaulted one it cannot read with
/// exit 4 and the bare reason.
fn load_group_file(path: &Path) -> anyhow::Result<Map<String, Value>> {
    load_vars_file(path).map_err(|err| match err.downcast_ref::<VaultFile>() {
        Some(vault) => crate::stats::Refusal::at(4, &vault.reason),
        None => err,
    })
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

/// A vaulted file that could not be read, with the reference's reason.
#[derive(Debug)]
pub struct VaultFile {
    pub path: PathBuf,
    pub reason: String,
}

impl std::fmt::Display for VaultFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Invalid vars_files file '{}': {}",
            self.path.display(),
            self.reason
        )
    }
}

impl std::error::Error for VaultFile {}

/// A YAML or JSON file whose document is a mapping. An empty file is an empty mapping. A vaulted
/// file is decrypted with the run's secrets first.
pub fn load_vars_file(path: &Path) -> anyhow::Result<Map<String, Value>> {
    let mut bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if crate::vault::is_encrypted(&bytes) {
        bytes = crate::vault::decrypt(&bytes, &crate::vault::run_secrets()).map_err(|e| {
            anyhow::Error::new(VaultFile {
                path: path.to_path_buf(),
                reason: crate::vault::sentence(&e.0),
            })
        })?;
    }
    let text = String::from_utf8(bytes).with_context(|| format!("reading {}", path.display()))?;
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
            let (options, raw) = crate::splitter::parse_kv(item)?;
            if let Some(word) = raw {
                bail!("extra var '{word}' is not key=value");
            }
            for (k, v) in options {
                out.insert(k, Value::String(v));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
