// SPDX-License-Identifier: GPL-3.0-or-later
//! The facts a host answers with: what ansible-core's `clean_facts()` keeps, and the store's
//! writes and reads of them.

use std::sync::Arc;

use serde_json::{Map, Value};

use super::{VarStore, holds_template};
use crate::inventory::is_localhost;
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
/// it through [`host_setting`](super::host_setting) like every other name that picks what runs,
/// so the scan guarding this list stays total. A host that set it could only pick another backend
/// of a closed list, which its own `pkg_mgr` fact already does.
///
/// `ansible_async_dir` is where a job writes its status file and the directory `async_status`
/// removes one from in `cleanup`: a host that named it for itself would choose where the agent,
/// escalated or not, writes and deletes.
///
/// `ansible_sudo_*`, `ansible_su_*` and `ansible_su_prompt_l10n` are the escalation settings of
/// one method, read like their `ansible_become_*` twins: a host must not name its own `exe`,
/// `user` or password.
const ENGINE_RESTRICTED_FACTS: [&str; 12] = [
    "ansible_remote_tmp",
    "ansible_package_use",
    "ansible_async_dir",
    "ansible_sudo_exe",
    "ansible_sudo_flags",
    "ansible_sudo_user",
    "ansible_sudo_pass",
    "ansible_su_exe",
    "ansible_su_flags",
    "ansible_su_user",
    "ansible_su_pass",
    "ansible_su_prompt_l10n",
];

/// Whether ansible-core's `clean_facts()` removes a fact of this name before it becomes a
/// variable: a connection or escalation setting, `ansible_<connection plugin>_*` unless it ends
/// in `_bridge` or `_gwbridge`, any `ansible_become_*`, any `ansible_*_interpreter`, and the
/// engine's own result keys - except `ansible_ssh_host_key_*`, which `setup` reports and which
/// it always keeps.
pub(super) fn restricted_fact(key: &str) -> bool {
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
pub(super) fn split_facts(
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
pub(super) fn no_facts() -> &'static Arc<Map<String, Value>> {
    static EMPTY: std::sync::OnceLock<Arc<Map<String, Value>>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(|| {
        Arc::new(Map::from_iter([(
            "ansible_facts".to_string(),
            Value::Object(Map::new()),
        )]))
    })
}

impl VarStore {
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
}
