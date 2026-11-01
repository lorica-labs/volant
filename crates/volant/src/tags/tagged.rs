// SPDX-License-Identifier: GPL-3.0-or-later
//! A layer of variables with the tags of its values beside it.
//!
//! The map is read through `Deref` and written only through the methods here, each of which
//! updates or drops the written name's tree: a value written without its tree must never keep
//! the tags of the value it replaced, which may have been trusted.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use super::{TagMap, TagTree};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Tagged {
    values: Map<String, Value>,
    tags: TagMap,
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
            tags: TagMap::new(),
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
        Self { values, tags }
    }

    pub fn as_map(&self) -> &Map<String, Value> {
        &self.values
    }

    pub fn into_values(self) -> Map<String, Value> {
        self.values
    }

    pub fn into_parts(self) -> (Map<String, Value>, TagMap) {
        (self.values, self.tags)
    }

    pub fn tags(&self) -> &TagMap {
        &self.tags
    }

    pub fn tree(&self, name: &str) -> Option<&TagTree> {
        self.tags.get(name)
    }

    /// Writes `value` under `name` with `tree`; no tree leaves the value untagged.
    pub fn insert(&mut self, name: String, value: Value, tree: Option<TagTree>) -> Option<Value> {
        match tree {
            Some(tree) => _ = self.tags.insert(name.clone(), tree),
            None => _ = self.tags.shift_remove(&name),
        }
        self.values.insert(name, value)
    }

    pub fn remove(&mut self, name: &str) -> Option<Value> {
        self.tags.shift_remove(name);
        self.values.shift_remove(name)
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&String, &mut Value) -> bool) {
        self.values.retain(|name, value| keep(name, value));
        let values = &self.values;
        self.tags.retain(|name, _| values.contains_key(name));
    }

    pub fn clear(&mut self) {
        self.values.clear();
        self.tags.clear();
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
        for (name, value) in &other.values {
            self.combine_one(name, value, other.tags.get(name));
        }
    }

    /// [`Tagged::extend`], leaving out the names in `skip`.
    pub fn extend_except(&mut self, other: &Tagged, skip: &BTreeSet<String>) {
        for (name, value) in &other.values {
            if !skip.contains(name) {
                self.combine_one(name, value, other.tags.get(name));
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
