// SPDX-License-Identifier: GPL-3.0-or-later
//! Variable sources and their precedence, from inventory groups up to extra vars, plus the
//! magic variables Ansible defines for every host.

mod facts;
mod host_vars;
mod include_dir;
mod layers;
mod listed;
mod load;
mod magic;
mod play_view;
mod refresh;
pub(crate) mod reserved;
mod shared;
mod store;

pub(crate) use host_vars::PENDING_PYTHON;

/// The `hostvars` map: every host's view, each one shared on its own. A host whose variables
/// changed gets a new entry while a render still holding the previous map keeps it whole, and
/// that copy is one pointer per host rather than every host's variables: measured on a thousand
/// local hosts, copying the views themselves on each `set_fact` cost 11 s of a 15 s run.
pub type HostvarsMap = indexmap::IndexMap<String, Arc<Value>>;

/// The hosts holding a name that came from a managed host, each with those names: what a read
/// of `hostvars[host]` takes as data.
pub type UntrustedHosts = BTreeMap<String, BTreeSet<String>>;

/// The two names that carry a host's own name, data when the name came from data.
const HOST_NAMES: [&str; 2] = ["inventory_hostname", "inventory_hostname_short"];

/// A plain map of host views as a [`HostvarsMap`].
#[cfg(test)]
pub fn hostvars_map(map: Map<String, Value>) -> HostvarsMap {
    map.into_iter()
        .map(|(host, view)| (host, Arc::new(view)))
        .collect()
}
pub use host_vars::{HostSettings, HostVars, host_setting, omit_token, settled, settled_all};
pub use include_dir::{
    Chain, Failure, Scan, include_dir, join_path, python_regex_error, read_failure,
};
pub use listed::ListedOrigin;
pub use load::{
    VaultFile, include_vars_paths, load_play_vars_file, load_vars_file, parse_extra_vars,
    read_vars_document, set_hash_merge,
};
pub(crate) use load::{combine_one, extend, hash_merge, load_vars_dir, merge_hash};
pub use magic::ansible_version;
pub(crate) use play_view::PlayView;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::bridge::Bridge;
use crate::inventory::{Inventory, PluginConfig, Source};
use crate::stats::CustomStats;
use crate::template::Templar;

/// What a play and a task contribute on top of inventory-level sources.
#[derive(Debug, Default, Clone)]
pub struct Scope {
    pub play_vars: Map<String, Value>,
    pub vars_files: Arc<VarsFiles>,
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
    pub play_hosts: Arc<[String]>,
    /// Hosts of the batch being played that have not failed. Without `serial` a play is one
    /// batch and this is the same list as `play_hosts`; with it, the two part company as soon as
    /// the first batch ends. This is `ansible_play_batch`.
    pub batch_hosts: Arc<[String]>,
    /// Every host the play resolved, whether it is still in it or not.
    pub all_play_hosts: Arc<[String]>,
}

/// A play's `vars_files` as one host reads them: merged in the order the play lists them, a
/// later file winning, and split once so that a host's tasks share them rather than copy them.
/// Copied into every host's map for every task, a 7 MB file held 5 GB on 50 hosts.
#[derive(Debug, Default)]
pub struct VarsFiles {
    /// The names whose value holds no template, `!unsafe` or `!vault` text, which no render
    /// changes: the [`HostVars::base`] of every task of the hosts that read these files.
    pub plain: Arc<Map<String, Value>>,
    /// The names whose value does, copied into the host's map where they are rendered.
    pub templated: Map<String, Value>,
}

impl VarsFiles {
    pub fn new(files: &[Arc<Map<String, Value>>]) -> Self {
        // Merged mappings cannot be split between two layers read one after the other, so under
        // `hash_behaviour = merge` every name is copied into the map.
        if hash_merge() {
            let mut templated = Map::new();
            for file in files {
                extend(&mut templated, file);
            }
            return Self {
                plain: Arc::default(),
                templated,
            };
        }
        if let [file] = files
            && !file.values().any(crate::template::holds_template)
        {
            return Self {
                plain: Arc::clone(file),
                templated: Map::new(),
            };
        }
        let (mut plain, mut templated) = (Map::new(), Map::new());
        for file in files {
            for (name, value) in file.iter() {
                let (into, other) = if crate::template::holds_template(value) {
                    (&mut templated, &mut plain)
                } else {
                    (&mut plain, &mut templated)
                };
                other.shift_remove(name);
                into.insert(name.clone(), value.clone());
            }
        }
        Self {
            plain: Arc::new(plain),
            templated,
        }
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.plain.contains_key(name) || self.templated.contains_key(name)
    }

    /// Both halves, which hold no name in common.
    pub fn maps(&self) -> [&Map<String, Value>; 2] {
        [&self.plain, &self.templated]
    }
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
    /// What `include_vars` read, per host: the reference's vars cache, a layer of its own above a
    /// task's `vars:` and under `facts`, so a `set_fact` of the same name wins whichever ran last.
    included: BTreeMap<String, Map<String, Value>>,
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
    /// The host names and group names that are data: a name of the inventory or one `add_host`
    /// and `group_by` made that holds `{`, and a name those two took from an argument that read
    /// a managed host's value. Reading one of them back
    /// through `groups`, `hostvars`, `group_names`, a play's host list or `inventory_hostname`
    /// taints the render, so the name is shown as it is and never evaluated.
    data_hosts: BTreeSet<String>,
    data_groups: BTreeSet<String>,
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
    hostvars: Option<Arc<HostvarsMap>>,
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
    hostvars_with: BTreeMap<String, Arc<HostvarsMap>>,
    /// The spelling the implicit controller host answers to in `hostvars`: the first one a host
    /// ran under, `localhost` until one has. The reference makes one such host and names it after
    /// the first spelling asked for.
    implicit_controller: Option<String>,
    /// The hosts that hold at least one untrusted name, built on demand and dropped by the same
    /// rule as `hostvars`: every write that can change it already goes through `set_fact`.
    untrusted_hosts: Option<Arc<UntrustedHosts>>,
    /// The inventory-wide values, with the three host lists they were built from. `groups` only
    /// moves when `meta: refresh_inventory` reads the inventory again, but the lists come from the coordinator's last published progress and
    /// move whenever a host drops out, so the map is good for one triple and no longer.
    shared: Option<(SharedKey, Arc<Map<String, Value>>)>,
    /// See [`VarStore::templated_facts`]. Never goes back to `false`: the fact outlives the play.
    templated_facts: bool,
    /// `ansible_verbosity`: how many `-v` the run was given.
    verbosity: u8,
    /// `ansible_inventory_sources`: every inventory source in the order given, made absolute.
    inventory_sources: Vec<String>,
    /// `ansible_config_file`: the `ansible.cfg` the run read, absolute, or none.
    config_file: Option<String>,
    /// The inventory as `add_host`, `group_by` and a refresh left it: the one the plays behind
    /// the current one resolve their hosts against.
    dynamic: Inventory,
    /// What `add_host` and `group_by` did, in order, which a refresh applies again to the
    /// sources it reads anew.
    added: Vec<Added>,
    /// The hosts the play's pattern found after a refresh, which `ansible_play_hosts_all` shows
    /// in place of the play's starting list until the play ends.
    play_hosts_all: Option<Vec<String>>,
    /// The hosts `add_host` brought into the inventory while the play runs, in order: they join
    /// `ansible_play_hosts_all` behind the play's list, and the next batch finds them through
    /// the play's pattern ([`VarStore::start_batch`]).
    added_play_hosts: Vec<String>,
    /// Whether `add_host` grew the inventory in this play.
    play_grew: bool,
    /// `--limit`'s pattern and the hosts it narrows the run to, which a refresh resolves again
    /// and applies to the play's hosts too.
    limit: Option<(String, HashSet<String>)>,
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
    Host(String, Map<String, Value>),
    Member(String, String),
    Child(String, String),
}

/// Whether any string in `value`, at any depth, is a template.
pub(crate) fn holds_template(value: &Value) -> bool {
    match value {
        Value::String(text) => Templar::is_stored_template(text),
        Value::Array(items) => items.iter().any(holds_template),
        Value::Object(map) => map.values().any(holds_template),
        _ => false,
    }
}

/// Whether a host or group name holds template syntax: any `{`, so two names that the playbook
/// joins into a template are text too. The reference takes every name as data, whatever it holds.
pub(crate) fn is_data_name(name: &str) -> bool {
    name.contains('{')
}

/// The play and batch host lists one `shared` map was built from.
type SharedKey = (Arc<[String]>, Arc<[String]>, Arc<[String]>);

/// One host's gathered facts less the names a layer above them answers for, and those names.
#[derive(Debug)]
struct HostFacts {
    shadowed: BTreeSet<String>,
    map: Arc<Map<String, Value>>,
}

#[cfg(test)]
mod tests;
