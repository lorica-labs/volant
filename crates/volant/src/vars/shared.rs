// SPDX-License-Identifier: GPL-3.0-or-later
//! The inventory-wide values every host of a run reads: the `hostvars` map and the shared lists.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Map, Value};

use super::{HostvarsMap, Scope, VarStore};
use crate::inventory::{LOCALHOST_NAMES, is_localhost};
use crate::tags::{TagMap, TagTree, Tagged};

/// One host's entry in `hostvars`: its variables as one object, which is what most readers want,
/// and their tags beside it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct HostEntry {
    value: Value,
    tags: Arc<TagMap>,
}

impl std::ops::Deref for HostEntry {
    type Target = Value;

    fn deref(&self) -> &Value {
        &self.value
    }
}

impl From<Tagged> for HostEntry {
    fn from(vars: Tagged) -> Self {
        let tags = Arc::clone(vars.tags());
        Self {
            value: Value::Object(vars.into_values()),
            tags,
        }
    }
}

impl HostEntry {
    /// A view with no tags: none of its strings is a template.
    pub fn untagged(value: Value) -> Self {
        Self {
            value,
            tags: Arc::default(),
        }
    }

    pub fn tags(&self) -> &Arc<TagMap> {
        &self.tags
    }

    /// Whether every string of the host that holds a template may be rendered as one.
    pub fn all_templates_trusted(&self) -> bool {
        fn all(value: &Value, tree: Option<&TagTree>) -> bool {
            match value {
                Value::Array(items) => items
                    .iter()
                    .enumerate()
                    .all(|(i, item)| all(item, tree.and_then(|t| t.index(i)))),
                Value::Object(map) => map
                    .iter()
                    .all(|(key, item)| all(item, tree.and_then(|t| t.at(key)))),
                other => {
                    !crate::template::holds_template(other)
                        || crate::template::holds_trusted_template(other, tree)
                }
            }
        }
        self.value
            .as_object()
            .is_none_or(|map| map.iter().all(|(k, v)| all(v, self.tags.get(k))))
    }
}

impl VarStore {
    /// The `hostvars` view every host of this run renders against, as one shared map: templates
    /// read a host out of it rather than carrying a copy of it.
    ///
    /// `host` is the host about to render. The three spellings of the controller are in the map
    /// whether the inventory lists them or not, as `hostvars` answers for them in the reference:
    /// an implicit one belongs to no group and is not in `groups`. Any other name the inventory
    /// does not carry (a `delegate_to` target) gets its own completed map, kept beside the
    /// shared one.
    ///
    /// A host whose facts changed since the map was built has its own entry rebuilt, and only
    /// that one. The map is changed in place when nobody else holds it; a render still holding
    /// the previous map keeps it whole, which is what it saw when it took it, and the store
    /// then carries on with a copy.
    pub fn hostvars_shared(&mut self, host: &str) -> Arc<HostvarsMap> {
        // The reference keeps one implicit controller host, named after the first spelling that
        // asked for it, and answers the other two spellings with that same host.
        if self.implicit_controller.is_none() && self.is_implicit_localhost(host) {
            self.implicit_controller = Some(host.to_string());
            self.forget_hostvars();
        }
        let base = self.hostvars_base();
        if base.contains_key(host) {
            return base;
        }
        if let Some(completed) = self.hostvars_with.get(host) {
            return Arc::clone(completed);
        }
        let mut map = (*base).clone();
        map.insert(host.to_string(), Arc::new(self.view_value(host)));
        let completed = Arc::new(map);
        self.hostvars_with
            .insert(host.to_string(), Arc::clone(&completed));
        completed
    }

    /// `groups` as the reference orders its keys: `all`, `ungrouped`, then each group in the order
    /// the inventory first named it, measured on ansible-core 2.19.12.
    fn groups_in_order(&self) -> Map<String, Value> {
        let mut out = Map::new();
        for name in self.dynamic.group_order() {
            if let Some(members) = self.groups.get(&name) {
                out.insert(name, serde_json::to_value(members).unwrap_or_default());
            }
        }
        for (name, members) in &self.groups {
            if !out.contains_key(name) {
                out.insert(
                    name.clone(),
                    serde_json::to_value(members).unwrap_or_default(),
                );
            }
        }
        out
    }

    /// `hostvars_shared` without a host to complete it for: the inventory's own hosts and the
    /// three spellings of the controller.
    pub(super) fn hostvars_base(&mut self) -> Arc<HostvarsMap> {
        let base = if let Some(mut base) = self.hostvars.take() {
            let stale = std::mem::take(&mut self.stale);
            for name in &stale {
                if base.contains_key(name) && self.controller_alias(name).is_none() {
                    let view = Arc::new(self.view_value(name));
                    Arc::make_mut(&mut base).insert(name.clone(), view);
                }
            }
            // The spellings that stand for a host of the inventory, or for the implicit one,
            // show whatever that host shows now.
            if stale.iter().any(|name| is_localhost(name)) {
                self.add_controller_views(Arc::make_mut(&mut base));
            }
            base
        } else {
            self.stale.clear();
            let names = self.groups["all"].clone();
            let mut map = HostvarsMap::new();
            for name in names {
                let view = Arc::new(self.view_value(&name));
                map.insert(name, view);
            }
            self.add_controller_views(&mut map);
            Arc::new(map)
        };
        self.hostvars = Some(Arc::clone(&base));
        base
    }

    /// Puts the three spellings of the controller that the inventory does not list into the
    /// `hostvars` map, as the reference answers `hostvars` for them whether the inventory lists
    /// them or not. A spelling stands for the first host of the inventory that has one of the
    /// three names, and for the one implicit host when there is none. None of them is in
    /// `groups`: `Hostvars` leaves them out of an iteration.
    fn add_controller_views(&self, map: &mut HostvarsMap) {
        let implicit = self.implicit_controller.as_deref().unwrap_or("localhost");
        let mut implicit_view = None;
        for spelling in LOCALHOST_NAMES {
            if self.inventory_vars.contains_key(spelling) {
                continue;
            }
            let view = match self.controller_alias(spelling) {
                Some(alias) => map.get(alias).cloned().unwrap_or_default(),
                None => implicit_view
                    .get_or_insert_with(|| Arc::new(HostEntry::from(self.host_view(implicit))))
                    .clone(),
            };
            map.insert(spelling.to_string(), view);
        }
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
        let live: BTreeSet<&String> = scope
            .play_hosts
            .iter()
            .chain(scope.batch_hosts.iter())
            .collect();
        self.host_facts.retain(|host, _| live.contains(host));
        let batch = serde_json::to_value(&*scope.batch_hosts).unwrap_or_default();
        let mut map = Map::new();
        map.insert("groups".to_string(), Value::Object(self.groups_in_order()));
        map.insert(
            "ansible_play_hosts".to_string(),
            serde_json::to_value(&*scope.play_hosts).unwrap_or_default(),
        );
        map.insert(
            "ansible_play_hosts_all".to_string(),
            serde_json::to_value(&*self.play_hosts_all(scope)).unwrap_or_default(),
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
