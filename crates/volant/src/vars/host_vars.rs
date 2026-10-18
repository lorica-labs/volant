// SPDX-License-Identifier: GPL-3.0-or-later
//! One host's merged variables and the readers of the settings that decide how a task runs.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::{Map, Value};

use super::facts::split_facts;
use crate::template::Vars;
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
