// SPDX-License-Identifier: GPL-3.0-or-later
//! Lists and mappings that a template changes in place.
//!
//! minijinja values never change. A template that calls `append` or `update` needs a value that
//! does, so a render whose template calls such a method reads every plain list and mapping of
//! the variables through [`MutList`] and [`MutDict`]. Two names for one value (`b = a`) then
//! name one object, as in Python. The change lives as long as the render of one string.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use indexmap::IndexMap;
use minijinja::value::{Enumerator, Object, ObjectRepr, ValueKind};
use minijinja::{Error, ErrorKind, State, Value};

use super::{dicts, lists};

pub(crate) type Dict = IndexMap<Value, Value>;

/// The methods that change the value they are called on. A template that calls one of them is
/// rendered with mutable values.
pub(crate) const MUTATORS: [&str; 11] = [
    "append",
    "extend",
    "insert",
    "remove",
    "pop",
    "sort",
    "reverse",
    "clear",
    "update",
    "setdefault",
    "popitem",
];

/// What a list holds, shared with the plain lists and the copies made of it until a change.
///
/// `live` says that an item may be a mutable object. While it is not, a copy of the list is
/// the same storage, and the next change to either side copies it: `out.append(big)` costs
/// nothing however long `big` is.
#[derive(Debug, Default)]
pub(crate) struct ListStore {
    items: Arc<Vec<Value>>,
    live: bool,
}

/// What a mapping holds, with the same sharing as [`ListStore`].
#[derive(Debug, Default)]
pub(crate) struct DictStore {
    entries: Arc<Dict>,
    live: bool,
}

/// A list a template can change.
#[derive(Debug, Default)]
pub(crate) struct MutList(Mutex<ListStore>);

/// A mapping a template can change.
#[derive(Debug, Default)]
pub(crate) struct MutDict(Mutex<DictStore>);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl MutList {
    /// A copy of this list that no later change reaches.
    pub(super) fn copy_of(&self) -> Value {
        Snapshot::default().list(self)
    }

    /// A list over `items`, which a plain value holds as well.
    fn sharing(items: Arc<Vec<Value>>) -> MutList {
        MutList(Mutex::new(ListStore { items, live: false }))
    }

    /// A list to run one method on, which nothing else reads afterwards.
    pub(super) fn temp(items: Arc<Vec<Value>>) -> MutList {
        MutList::sharing(items)
    }

    pub(crate) fn from_items(items: Vec<Value>) -> Value {
        let live = items.iter().any(is_mutable);
        Value::from_object(MutList(Mutex::new(ListStore {
            items: Arc::new(items),
            live,
        })))
    }

    /// Runs `f` on the items to change them.
    pub(super) fn with<R>(&self, f: impl FnOnce(&mut Vec<Value>) -> R) -> R {
        f(Arc::make_mut(&mut lock(&self.0).items))
    }

    /// Runs `f` on the items to read them. Nothing under `f` may read this list again.
    pub(super) fn read<R>(&self, f: impl FnOnce(&Vec<Value>) -> R) -> R {
        f(&lock(&self.0).items)
    }

    pub(super) fn items(&self) -> Arc<Vec<Value>> {
        Arc::clone(&lock(&self.0).items)
    }

    /// The list as a plain value that shares its storage, when no item is a mutable object.
    fn frozen(&self) -> Option<Value> {
        let store = lock(&self.0);
        (!store.live).then(|| Value::from_dyn_object(Arc::clone(&store.items)))
    }
}

impl MutDict {
    /// A copy of this mapping that no later change reaches.
    pub(super) fn copy_of(&self) -> Value {
        Snapshot::default().dict(self)
    }

    fn sharing(entries: Arc<Dict>) -> MutDict {
        MutDict(Mutex::new(DictStore {
            entries,
            live: false,
        }))
    }

    /// A mapping to run one method on, which nothing else reads afterwards.
    pub(super) fn temp(entries: Arc<Dict>) -> MutDict {
        MutDict::sharing(entries)
    }

    pub(crate) fn from_entries(entries: Dict) -> Value {
        let live = entries.values().any(is_mutable);
        Value::from_object(MutDict(Mutex::new(DictStore {
            entries: Arc::new(entries),
            live,
        })))
    }

    pub(super) fn with<R>(&self, f: impl FnOnce(&mut Dict) -> R) -> R {
        f(Arc::make_mut(&mut lock(&self.0).entries))
    }

    /// `f` on the whole store, for a change that can make an entry mutable.
    pub(super) fn with_store<R>(&self, f: impl FnOnce(&mut DictStore) -> R) -> R {
        f(&mut lock(&self.0))
    }

    pub(super) fn read<R>(&self, f: impl FnOnce(&Dict) -> R) -> R {
        f(&lock(&self.0).entries)
    }

    pub(super) fn entries(&self) -> Arc<Dict> {
        Arc::clone(&lock(&self.0).entries)
    }

    fn frozen(&self) -> Option<Value> {
        let store = lock(&self.0);
        (!store.live).then(|| Value::from_dyn_object(Arc::clone(&store.entries)))
    }
}

impl DictStore {
    /// The value under `key`, made mutable in place when it is a plain list or mapping.
    pub(super) fn child(&mut self, key: &Value) -> Option<Value> {
        let item = self.entries.get(key)?.clone();
        match mutable(&item) {
            Some(made) => {
                Arc::make_mut(&mut self.entries).insert(key.clone(), made.clone());
                self.live = true;
                Some(made)
            }
            None => Some(item),
        }
    }

    pub(super) fn entries_mut(&mut self) -> &mut Dict {
        Arc::make_mut(&mut self.entries)
    }
}

impl Object for MutList {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Seq
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let at = key.as_usize()?;
        let mut store = lock(&self.0);
        let item = store.items.get(at)?.clone();
        // An item that is a plain list or mapping becomes mutable the first time it is read,
        // and stays so: the next read gives the same object.
        match mutable(&item) {
            Some(made) => {
                Arc::make_mut(&mut store.items)[at] = made.clone();
                store.live = true;
                Some(made)
            }
            None => Some(item),
        }
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Seq(lock(&self.0).items.len())
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &mut State<'_, '_>,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        lists::call(self, method, args)
            .unwrap_or_else(|| Err(Error::from(ErrorKind::UnknownMethod)))
    }
}

impl Object for MutDict {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Map
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        lock(&self.0).child(key)
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Values(lock(&self.0).entries.keys().cloned().collect())
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &mut State<'_, '_>,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        dicts::call(self, method, args)
            .unwrap_or_else(|| Err(Error::from(ErrorKind::UnknownMethod)))
    }
}

/// The list inside a plain value: one minijinja built from a literal, a filter or a variable.
pub(super) fn plain_list(value: &Value) -> Option<&Vec<Value>> {
    value.downcast_object_ref::<Vec<Value>>()
}

/// The mapping inside a plain value.
pub(super) fn plain_map(value: &Value) -> Option<&Dict> {
    value.downcast_object_ref::<Dict>()
}

/// A mutable copy of a plain list or mapping; `None` for any other value, which is left as it is.
pub(crate) fn mutable(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    if let Some(items) = object.downcast::<Vec<Value>>() {
        return Some(Value::from_object(MutList::sharing(items)));
    }
    object
        .downcast::<Dict>()
        .map(|entries| Value::from_object(MutDict::sharing(entries)))
}

/// Whether a value is one of the mutable objects of this module.
pub(crate) fn is_mutable(value: &Value) -> bool {
    value.downcast_object_ref::<MutList>().is_some()
        || value.downcast_object_ref::<MutDict>().is_some()
}

/// An address that names a plain list or mapping for as long as it lives, so that two reads of
/// one variable find one mutable object.
pub(crate) fn identity(value: &Value) -> Option<usize> {
    plain_list(value)
        .map(|list| std::ptr::from_ref(list) as usize)
        .or_else(|| plain_map(value).map(|map| std::ptr::from_ref(map) as usize))
}

/// Whether a value is a list or mapping of plain values whose mapping keys are all texts, so
/// that its JSON form reads back as the same value.
pub(crate) fn is_literal(value: &Value) -> bool {
    if let Some(items) = plain_list(value) {
        return items.iter().all(is_literal);
    }
    if let Some(entries) = plain_map(value) {
        return entries
            .iter()
            .all(|(k, v)| k.as_str().is_some() && is_literal(v));
    }
    matches!(
        value.kind(),
        ValueKind::None | ValueKind::Bool | ValueKind::Number | ValueKind::String
    )
}

/// A copy of a value that no later change reaches: every mutable list and mapping inside it is
/// replaced by a plain one. ansible-core copies what it hands to a call and what a call hands
/// back, and so does every method here that stores or returns a value.
///
/// A plain list or mapping holds no mutable object (a literal is built mutable, a filter's
/// answer is copied here, and the others store copies), so it is shared and not walked: a
/// copy costs what the mutable parts hold. A mutable object met twice is copied once, so a
/// structure that holds copies of itself is walked as the graph it is.
pub(crate) fn snapshot(value: &Value) -> Value {
    Snapshot::default().copy(value)
}

/// [`snapshot`] of what a filter answered. The answer is a plain list or mapping that can hold
/// the mutable objects its argument held, so it is walked.
pub(crate) fn snapshot_answer(value: &Value) -> Value {
    Snapshot {
        walk_plain: true,
        ..Snapshot::default()
    }
    .copy(value)
}

#[derive(Default)]
struct Snapshot {
    seen: HashMap<usize, Value>,
    walk_plain: bool,
}

impl Snapshot {
    fn list(&mut self, list: &MutList) -> Value {
        let at = std::ptr::from_ref(list) as usize;
        if let Some(done) = self.seen.get(&at) {
            return done.clone();
        }
        if let Some(frozen) = list.frozen() {
            return frozen;
        }
        let copied = Value::from(
            list.items()
                .iter()
                .map(|item| self.copy(item))
                .collect::<Vec<_>>(),
        );
        self.seen.insert(at, copied.clone());
        copied
    }

    fn dict(&mut self, dict: &MutDict) -> Value {
        let at = std::ptr::from_ref(dict) as usize;
        if let Some(done) = self.seen.get(&at) {
            return done.clone();
        }
        if let Some(frozen) = dict.frozen() {
            return frozen;
        }
        let copied = Value::from_pairs(
            dict.entries()
                .iter()
                .map(|(k, v)| (k.clone(), self.copy(v))),
        );
        self.seen.insert(at, copied.clone());
        copied
    }

    fn copy(&mut self, value: &Value) -> Value {
        if let Some(list) = value.downcast_object_ref::<MutList>() {
            return self.list(list);
        }
        if let Some(dict) = value.downcast_object_ref::<MutDict>() {
            return self.dict(dict);
        }
        if self.walk_plain {
            if let Some(items) = plain_list(value) {
                return Value::from(items.iter().map(|item| self.copy(item)).collect::<Vec<_>>());
            }
            if let Some(entries) = plain_map(value) {
                return Value::from_pairs(entries.iter().map(|(k, v)| (k.clone(), self.copy(v))));
            }
        }
        // A lazy list (`a + b`) still holds what it was built from.
        if value.kind() == ValueKind::Iterable
            && plain_list(value).is_none()
            && let Ok(items) = value.try_iter()
        {
            return Value::from(items.map(|item| self.copy(&item)).collect::<Vec<_>>());
        }
        value.clone()
    }
}

/// A mapping key, which is read as it is stored: copied, and never a mapping.
pub(crate) fn key(value: &Value) -> Result<Value, Error> {
    if value.kind() == ValueKind::Map {
        return Err(super::args::raise("unhashable type: 'dict'"));
    }
    Ok(snapshot(value))
}

/// Whether a value is a list the methods of this module can read: a mutable list or a plain one.
pub(super) fn is_list(value: &Value) -> bool {
    value.kind() == ValueKind::Seq
        && (value.downcast_object_ref::<MutList>().is_some() || plain_list(value).is_some())
}

/// Whether a value is a mapping the methods of this module can read.
pub(super) fn is_dict(value: &Value) -> bool {
    value.kind() == ValueKind::Map
        && (value.downcast_object_ref::<MutDict>().is_some() || plain_map(value).is_some())
}
