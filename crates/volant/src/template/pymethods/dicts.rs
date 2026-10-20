// SPDX-License-Identifier: GPL-3.0-or-later
//! The methods of Python's `dict`.

use minijinja::value::ValueKind;
use minijinja::{Error, Value};

use super::args::{Call, iterate, raise};
use super::format::repr;
use super::mutable::{Dict, MutDict, is_mutable, key, snapshot};

fn none() -> Result<Value, Error> {
    Ok(Value::from(()))
}

fn list(items: Vec<Value>) -> Value {
    Value::from(items)
}

/// Runs a method of `dict` on `me`. `None` when `name` is no method of a mapping.
pub(super) fn call(me: &MutDict, name: &str, args: &[Value]) -> Option<Result<Value, Error>> {
    let call = Call::of("dict", args);
    Some(match name {
        "get" => call.between(name, 1, 2).and_then(|a| {
            let k = key(&a[0])?;
            let found = me.read(|d| d.get(&k).cloned());
            Ok(found.map_or_else(
                || a.get(1).cloned().unwrap_or_else(|| Value::from(())),
                |v| snapshot(&v),
            ))
        }),
        "keys" => call
            .none(name)
            .map(|()| list(me.read(|d| d.keys().cloned().collect()))),
        "values" => call
            .none(name)
            .map(|()| list(me.entries().values().map(snapshot).collect())),
        "items" => call.none(name).map(|()| {
            list(
                me.entries()
                    .iter()
                    .map(|(k, v)| list(vec![k.clone(), snapshot(v)]))
                    .collect(),
            )
        }),
        "copy" => call.none(name).map(|()| me.copy_of()),
        "clear" => call.none(name).map(|()| {
            me.with(Dict::clear);
            Value::from(())
        }),
        "pop" => call.between(name, 1, 2).and_then(|a| {
            let k = key(&a[0])?;
            match me.with(|d| d.shift_remove(&k)) {
                Some(v) => Ok(v),
                None => a.get(1).cloned().ok_or_else(|| raise(repr(&k))),
            }
        }),
        "popitem" => call.none(name).and_then(|()| {
            me.with(Dict::pop)
                .map(|(k, v)| list(vec![k, v]))
                .ok_or_else(|| raise("'popitem(): dictionary is empty'"))
        }),
        "setdefault" => call.between(name, 1, 2).and_then(|a| {
            let k = key(&a[0])?;
            let default = a.get(1).map_or_else(|| Value::from(()), snapshot);
            Ok(me.with_store(|store| {
                store.entries_mut().entry(k.clone()).or_insert(default);
                store.child(&k).unwrap_or_else(|| Value::from(()))
            }))
        }),
        "update" => update(me, &call),
        "fromkeys" => fromkeys(args),
        _ => return None,
    })
}

/// `dict.fromkeys(iterable, value=None)`: a mapping of every item of `iterable` to `value`. It is
/// a method of the type, so `dict.fromkeys(...)` and `{}.fromkeys(...)` are the same call.
pub(in crate::template) fn fromkeys(args: &[Value]) -> Result<Value, Error> {
    let call = Call::of("dict", args);
    let a = call.between("fromkeys", 1, 2)?;
    // A list or mapping that is a mutable object stays one, and every key holds that one object,
    // as in Python: a change through one key shows through the others.
    let value = a.get(1).map_or_else(
        || Value::from(()),
        |v| {
            if is_mutable(v) {
                v.clone()
            } else {
                snapshot(v)
            }
        },
    );
    let entries = super::super::globals::keyed(iterate(&a[0])?, &value)?;
    Ok(if is_mutable(&value) {
        MutDict::from_entries(entries.into_iter().collect())
    } else {
        Value::from_pairs(entries)
    })
}

/// `update([other], **kwargs)`: `other` is a mapping or a sequence of pairs.
fn update(me: &MutDict, call: &Call<'_>) -> Result<Value, Error> {
    if call.pos.len() > 1 {
        return Err(raise(format!(
            "update expected at most 1 argument, got {}",
            call.pos.len()
        )));
    }
    let mut pairs: Vec<(Value, Value)> = Vec::new();
    if let Some(other) = call.pos.first() {
        if other.kind() == ValueKind::Map {
            for key in other.try_iter()? {
                let value = other.get_item(&key)?;
                pairs.push((super::mutable::key(&key)?, snapshot(&value)));
            }
        } else {
            for (at, item) in iterate(other)?.iter().enumerate() {
                let pair = iterate(item).map_err(|_| {
                    raise(format!(
                        "cannot convert dictionary update sequence element #{at} to a sequence"
                    ))
                })?;
                let [key, value] = <[Value; 2]>::try_from(pair.clone()).map_err(|_| {
                    raise(format!(
                        "dictionary update sequence element #{at} has length {}; 2 is required",
                        pair.len()
                    ))
                })?;
                pairs.push((super::mutable::key(&key)?, snapshot(&value)));
            }
        }
    }
    for (key, value) in &call.kw {
        pairs.push((Value::from(key.as_str()), snapshot(value)));
    }
    me.with(|d| d.extend(pairs));
    none()
}
