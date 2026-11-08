// SPDX-License-Identifier: GPL-3.0-or-later
//! A layer of variables with the tags of its values beside it.
//!
//! The map is read through `Deref` and written only through the methods here, each of which
//! updates or drops the written name's tree: a value written without its tree must never keep
//! the tags of the value it replaced, which may have been trusted. The trees are shared, and
//! copied only when a layer that shares them is written.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Map, Value};

use super::{TagMap, TagTree};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Tagged {
    values: Map<String, Value>,
    tags: Arc<TagMap>,
}

impl std::ops::Deref for Tagged {
    type Target = Map<String, Value>;

    fn deref(&self) -> &Map<String, Value> {
        &self.values
    }
}

/// Values with no tags.
impl From<Map<String, Value>> for Tagged {
    fn from(values: Map<String, Value>) -> Self {
        Self {
            values,
            tags: Arc::default(),
        }
    }
}

/// The trees of the entries of a mapping's tree, by key.
pub fn top_level(tree: Option<TagTree>) -> TagMap {
    match tree {
        Some(TagTree::Node(_, super::Children::Map(entries))) => entries.into_iter().collect(),
        _ => TagMap::new(),
    }
}

/// Values with no tags.
impl FromIterator<(String, Value)> for Tagged {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(iter: I) -> Self {
        Self::from(iter.into_iter().collect::<Map<String, Value>>())
    }
}

impl<'a> IntoIterator for &'a Tagged {
    type Item = (&'a String, &'a Value);
    type IntoIter = serde_json::map::Iter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.values.iter()
    }
}

impl Tagged {
    /// `values` with the trees of `tags` whose name it holds.
    pub fn new(values: Map<String, Value>, mut tags: TagMap) -> Self {
        tags.retain(|name, _| values.contains_key(name));
        Self {
            values,
            tags: Arc::new(tags),
        }
    }

    /// Puts `values` in place of these, the trees of `dropped` and of any name `values` lacks
    /// left out.
    pub fn replace_values(&mut self, values: Map<String, Value>, dropped: &[String]) {
        let same_names = values.len() == self.values.len();
        self.values = values;
        self.drop_trees(dropped);
        if !same_names {
            let values = &self.values;
            let gone: Vec<String> = self
                .tags
                .keys()
                .filter(|name| !values.contains_key(*name))
                .cloned()
                .collect();
            self.drop_trees(&gone);
        }
    }

    /// `values` with `tags`, shared rather than copied. Names `values` lacks are never read.
    pub fn with_shared_tags(values: Map<String, Value>, tags: Arc<TagMap>) -> Self {
        Self { values, tags }
    }

    /// Whether the value of `name` came from a managed host.
    pub fn is_from_host(&self, name: &str) -> bool {
        self.tags
            .get(name)
            .is_some_and(|tree| super::is_from_host(tree.own()))
    }

    /// Writes a value a managed host sent under `name`: it is no template, whatever it holds.
    pub fn insert_from_host(&mut self, name: String, value: Value) -> Option<Value> {
        let tree = super::from_host(None);
        self.insert(name, value, Some(tree))
    }

    /// Marks the value of `name` as a managed host's, when there is one.
    pub fn mark_from_host(&mut self, name: &str) {
        if self.values.contains_key(name) {
            let tags = Arc::make_mut(&mut self.tags);
            let tree = tags.get(name).cloned();
            tags.insert(name.to_string(), super::from_host(tree));
        }
    }

    pub fn as_map(&self) -> &Map<String, Value> {
        &self.values
    }

    pub fn into_values(self) -> Map<String, Value> {
        self.values
    }

    pub fn into_parts(self) -> (Map<String, Value>, TagMap) {
        (self.values, Arc::unwrap_or_clone(self.tags))
    }

    pub fn tags(&self) -> &Arc<TagMap> {
        &self.tags
    }

    pub fn tree(&self, name: &str) -> Option<&TagTree> {
        self.tags.get(name)
    }

    /// Writes `value` under `name` with `tree`; no tree leaves the value untagged.
    pub fn insert(&mut self, name: String, value: Value, tree: Option<TagTree>) -> Option<Value> {
        match tree {
            Some(tree) => _ = Arc::make_mut(&mut self.tags).insert(name.clone(), tree),
            None if self.tags.contains_key(&name) => {
                _ = Arc::make_mut(&mut self.tags).shift_remove(&name);
            }
            None => {}
        }
        self.values.insert(name, value)
    }

    pub fn remove(&mut self, name: &str) -> Option<Value> {
        if self.tags.contains_key(name) {
            Arc::make_mut(&mut self.tags).shift_remove(name);
        }
        self.values.shift_remove(name)
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&String, &mut Value) -> bool) {
        let mut dropped = Vec::new();
        self.values.retain(|name, value| {
            let kept = keep(name, value);
            if !kept {
                dropped.push(name.clone());
            }
            kept
        });
        self.drop_trees(&dropped);
    }

    /// Drops the trees of `names`, copying shared trees only when one of them has one.
    fn drop_trees(&mut self, names: &[String]) {
        if names.iter().any(|name| self.tags.contains_key(name)) {
            let tags = Arc::make_mut(&mut self.tags);
            for name in names {
                tags.swap_remove(name);
            }
        }
    }

    pub fn clear(&mut self) {
        self.values.clear();
        self.tags = Arc::default();
    }

    /// Lays `value` over the variable `name`: `combine_vars` of ansible-core, which replaces it,
    /// or merges two mappings under `hash_behaviour = merge`, each leaf keeping its own tags.
    pub fn combine_one(&mut self, name: &str, value: &Value, tree: Option<&TagTree>) {
        let merged = match (crate::vars::hash_merge(), self.values.get(name), value) {
            (true, Some(old @ Value::Object(old_map)), Value::Object(fresh)) => {
                let tree = super::merge(self.tags.get(name), tree, old, value);
                Some((Value::Object(crate::vars::merge_hash(old_map, fresh)), tree))
            }
            _ => None,
        };
        match merged {
            Some((merged, tree)) => _ = self.insert(name.to_string(), merged, tree),
            None => _ = self.insert(name.to_string(), value.clone(), tree.cloned()),
        }
    }

    /// Replaces each variable `other` holds with its value there, mappings included.
    pub fn overlay(&mut self, other: &Tagged) {
        for (name, value) in &other.values {
            self.insert(name.clone(), value.clone(), other.tags.get(name).cloned());
        }
    }

    /// Lays every variable of `other` over these.
    pub fn extend(&mut self, other: &Tagged) {
        self.extend_except(other, &BTreeSet::new());
    }

    /// [`Tagged::extend`], leaving out the names in `skip`.
    ///
    /// The layers of a host's variables are laid under the store's lock, so this one walks each
    /// side once instead of looking every name up in both maps of trees: a name of `other`
    /// replaces ours, and takes its tree, or none, with it.
    pub fn extend_except(&mut self, other: &Tagged, skip: &BTreeSet<String>) {
        if crate::vars::hash_merge() {
            for (name, value) in &other.values {
                if !skip.contains(name) {
                    self.combine_one(name, value, other.tags.get(name));
                }
            }
            return;
        }
        if self.values.is_empty() && skip.is_empty() {
            self.values.clone_from(&other.values);
            self.tags = Arc::clone(&other.tags);
            return;
        }
        // Our trees of names `other` writes untagged: none when `other` tags every name.
        if !self.tags.is_empty() && other.tags.len() < other.values.len() {
            let stale: Vec<String> = other
                .values
                .keys()
                .filter(|name| !skip.contains(*name) && !other.tags.contains_key(*name))
                .filter(|name| self.tags.contains_key(*name))
                .cloned()
                .collect();
            self.drop_trees(&stale);
        }
        for (name, value) in &other.values {
            if !skip.contains(name) {
                self.values.insert(name.clone(), value.clone());
            }
        }
        let written: Vec<(&String, &TagTree)> = other
            .tags
            .iter()
            .filter(|(name, _)| !skip.contains(*name))
            .collect();
        if !written.is_empty() {
            let tags = Arc::make_mut(&mut self.tags);
            for (name, tree) in written {
                tags.insert(name.clone(), tree.clone());
            }
        }
    }

    /// Lays untagged values over these, as values a managed host or the engine produced.
    pub fn extend_untagged(&mut self, values: &Map<String, Value>) {
        for (name, value) in values {
            self.combine_one(name, value, None);
        }
    }
}
