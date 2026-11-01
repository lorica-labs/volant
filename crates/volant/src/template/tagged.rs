// SPDX-License-Identifier: GPL-3.0-or-later
//! The tags of the values one render reads, kept by the address of each value's allocation.
//!
//! A string converted with its tags is an `Arc<str>`, and a list or a mapping an `Arc`'d object:
//! a render that passes one through hands out the same allocation, so its address finds its tags
//! again. A value an operation builds is a new allocation and has none, as in Python.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::{Arc, Mutex, PoisonError};

use minijinja::Value;
use minijinja::value::ValueKind;

use crate::tags::{Children, OriginId, TagSet, TagSetId, TagTree};

/// What `Value::from_pairs` builds with the `preserve_order` feature.
type Mapping = indexmap::IndexMap<Value, Value>;

/// A number by kind and bits: integers apart from floats.
type NumberKey = (bool, u128);

/// Hashes an address or a number with one multiplication: the keys are never chosen by a
/// playbook, so they need no protection from collisions made on purpose.
#[derive(Default)]
struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }

    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }
}

type Keyed<K, V> = HashMap<K, V, BuildHasherDefault<KeyHasher>>;

#[derive(Debug, Default)]
pub(crate) struct TagRegistry {
    /// By address, with a clone that keeps the allocation alive, so that no other value takes
    /// its address while the render runs.
    held: Mutex<Keyed<usize, (TagSetId, Value)>>,
    /// Numbers have no allocation, so they are keyed by value, and a number two different sets
    /// meet in one render has none: `x: 1` and `y: 1` written on two lines leave `1` untagged.
    /// The ceiling of keying by value; the way past it is a tagged number type of its own.
    numbers: Mutex<Keyed<NumberKey, Option<TagSetId>>>,
}

impl TagRegistry {
    pub(crate) fn tag(&self, value: &Value, id: TagSetId) {
        if id.is_none() {
            return;
        }
        if let Some(key) = number_key(value) {
            let mut numbers = self.numbers.lock().unwrap_or_else(PoisonError::into_inner);
            let entry = numbers.entry(key).or_insert(Some(id));
            if *entry != Some(id) {
                *entry = None;
            }
        } else if let Some(at) = address(value) {
            self.held
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(at, (id, value.clone()));
        }
    }

    pub(crate) fn tags_of(&self, value: &Value) -> TagSetId {
        let found = if let Some(key) = number_key(value) {
            self.numbers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&key)
                .copied()
                .flatten()
        } else {
            address(value).and_then(|at| {
                self.held
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&at)
                    .map(|(id, _)| *id)
            })
        };
        found.unwrap_or(TagSetId::NONE)
    }

    fn is_empty(&self) -> bool {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
            && self
                .numbers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty()
    }

    /// The tags of `value` and of what it holds, as far as this render knows them.
    pub(crate) fn tree_of(&self, value: &Value) -> Option<TagTree> {
        if self.is_empty() {
            return None;
        }
        self.walk(value)
    }

    fn walk(&self, value: &Value) -> Option<TagTree> {
        let own = self.tags_of(value);
        let children = if let Some(items) = value.downcast_object_ref::<Vec<Value>>() {
            let kids: Vec<_> = items
                .iter()
                .enumerate()
                .filter_map(|(i, item)| Some((i, self.walk(item)?)))
                .collect();
            Children::Seq(kids)
        } else if let Some(map) = value.downcast_object_ref::<Mapping>() {
            let kids: Vec<_> = map
                .iter()
                .filter_map(|(key, item)| {
                    let name = key.as_str().map_or_else(|| key.to_string(), str::to_string);
                    Some((name, self.walk(item)?))
                })
                .collect();
            Children::Map(kids)
        } else {
            Children::Seq(Vec::new())
        };
        TagTree::node(own, children)
    }
}

/// The tags of a template's result, `found` being those the render knows for it: a result that
/// passed a value through keeps that value's, and one the render built gets `origin` (where the
/// template was written) and no trust.
pub(super) fn with_origin(found: Option<TagTree>, origin: Option<OriginId>) -> Option<TagTree> {
    if found.as_ref().is_some_and(|t| !t.own().is_none()) {
        return found;
    }
    let own = crate::tags::intern(TagSet {
        origin,
        ..TagSet::default()
    });
    match found {
        Some(TagTree::Node(_, children)) => TagTree::node(own, children),
        _ => TagTree::node(own, Children::Seq(Vec::new())),
    }
}

/// The tags the render `state` belongs to knows for `value`.
pub(super) fn tags_of(state: &minijinja::State, value: &Value) -> TagSetId {
    state
        .lookup(super::CONTEXT_KEY)
        .and_then(|root| {
            root.downcast_object_ref::<super::context::Context>()
                .map(|ctx| ctx.registry.tags_of(value))
        })
        .unwrap_or(TagSetId::NONE)
}

/// Tags `value` with `id` for the rest of the render `state` belongs to.
pub(super) fn tag(state: &minijinja::State, value: &Value, id: TagSetId) {
    if let Some(root) = state.lookup(super::CONTEXT_KEY)
        && let Some(ctx) = root.downcast_object_ref::<super::context::Context>()
    {
        ctx.registry.tag(value, id);
    }
}

/// Records the tags of `value`, the value the last expression over `ctx` gave, or that the last
/// render gave text.
pub(super) fn set_result(ctx: &Value, value: Option<&Value>) {
    if let Some(root) = ctx.downcast_object_ref::<super::context::Context>() {
        let tree = value.and_then(|v| root.registry.tree_of(v));
        *root.result.lock().unwrap_or_else(PoisonError::into_inner) = tree;
    }
}

/// What [`set_result`] recorded last over `ctx`, taken.
pub(super) fn take_result(ctx: &Value) -> Option<TagTree> {
    let root = ctx.downcast_object_ref::<super::context::Context>()?;
    root.result
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
}

fn number_key(value: &Value) -> Option<NumberKey> {
    if value.kind() != ValueKind::Number {
        return None;
    }
    if value.is_integer() {
        let n = i128::try_from(value.clone()).ok()?;
        Some((false, n.cast_unsigned()))
    } else {
        let f = f64::try_from(value.clone()).ok()?;
        Some((true, u128::from(f.to_bits())))
    }
}

/// Where the allocation of `value` lives: its text for a string, its object for a list or a
/// mapping. An inline string's text lives in the value itself, never at a held allocation.
fn address(value: &Value) -> Option<usize> {
    match value.kind() {
        ValueKind::String => value.as_str().map(|s| s.as_ptr() as usize),
        ValueKind::Seq => value
            .downcast_object_ref::<Vec<Value>>()
            .map(|items| std::ptr::from_ref(items) as usize),
        ValueKind::Map => value
            .downcast_object_ref::<Mapping>()
            .map(|map| std::ptr::from_ref(map) as usize),
        _ => None,
    }
}

/// `to_template`, with each tagged part of `value` built as an allocation of its own and
/// registered with its tags.
pub(crate) fn to_template_tagged(
    value: &serde_json::Value,
    tree: Option<&TagTree>,
    registry: &TagRegistry,
) -> Value {
    use serde_json::Value as Json;
    let Some(tree) = tree else {
        return super::bigint::to_template(value);
    };
    let out = match (value, tree) {
        (Json::String(s), _) if crate::yaml::binary_text(s).is_none() => {
            Value::from(Arc::<str>::from(s.as_str()))
        }
        (Json::Array(items), TagTree::Node(_, Children::Seq(kids))) => {
            let kids: HashMap<usize, &TagTree> = kids.iter().map(|(i, t)| (*i, t)).collect();
            items
                .iter()
                .enumerate()
                .map(|(i, item)| to_template_tagged(item, kids.get(&i).copied(), registry))
                .collect()
        }
        (Json::Object(map), TagTree::Node(_, Children::Map(kids)))
            if super::typed_keys::keys(map).is_none() =>
        {
            let kids: HashMap<&str, &TagTree> = kids.iter().map(|(k, t)| (k.as_str(), t)).collect();
            Value::from_pairs(map.iter().map(|(key, item)| {
                let tree = kids.get(key.as_str()).copied();
                (
                    Value::from(key.as_str()),
                    to_template_tagged(item, tree, registry),
                )
            }))
        }
        _ => super::bigint::to_template(value),
    };
    registry.tag(&out, tree.own());
    out
}

#[cfg(test)]
mod tests;
