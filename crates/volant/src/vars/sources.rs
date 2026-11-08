// SPDX-License-Identifier: GPL-3.0-or-later
//! One host's layers in two steps: what the store holds for the host, read under the store's
//! lock, then laid over the play's and the task's own layers once the lock is released. Every
//! host of a batch reads the store under that one lock, so the merge, which copies every name
//! of every layer with its tags, is kept out of it.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Map, Value};

use super::layers::Layers;
use super::{Scope, Tagged};

/// What the store holds for one host, as [`VarStore::host_sources`](super::VarStore) reads it.
#[derive(Debug)]
pub struct HostSources {
    pub(super) base: Tagged,
    pub(super) included: Option<Arc<Tagged>>,
    pub(super) facts: Option<Arc<Tagged>>,
    pub(super) extra: Tagged,
    pub(super) magic: Map<String, Value>,
    /// The names of `magic` that carry a name a managed host sent.
    pub(super) data_magic: Vec<&'static str>,
    /// The gathered facts that beat the layers below `play_vars`.
    pub(super) answering: BTreeSet<String>,
    pub(super) shared: Arc<Map<String, Value>>,
    pub(super) data_lists: Arc<BTreeSet<String>>,
    pub(super) gathered: Arc<Map<String, Value>>,
}

impl HostSources {
    /// The host's layers, in precedence order, lowest first.
    pub fn layer(self, scope: &Scope) -> Layers {
        let mut map = Tagged::default();
        map.extend_except(&scope.role_defaults, &self.answering);
        map.extend_except(&self.base, &self.answering);
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
        if let Some(included) = &self.included {
            map.extend(included);
        }
        if let Some(facts) = &self.facts {
            map.extend(facts);
        }
        map.extend(&scope.role_params);
        map.extend(&self.extra);
        for (name, value) in self.magic {
            map.insert(name, value, None);
        }
        for name in self.data_magic {
            map.mark_from_host(name);
        }
        crate::profile::count(crate::profile::Counter::LayersBuilt, 1);
        crate::profile::count(
            crate::profile::Counter::LayersEntries,
            u64::try_from(map.len()).unwrap_or(u64::MAX),
        );
        Layers {
            map,
            shared: self.shared,
            data_lists: self.data_lists,
            facts: self.gathered,
            files,
        }
    }
}
