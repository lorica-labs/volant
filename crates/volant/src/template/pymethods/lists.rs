// SPDX-License-Identifier: GPL-3.0-or-later
//! The methods of Python's `list`.

use minijinja::value::ValueKind;
use minijinja::{Error, Value};

use super::args::{Call, bound, clamp, index, iterate, raise, type_of};
use super::format::repr;
use super::limits::check_items;
use super::mutable::{MutList, snapshot};

fn none() -> Result<Value, Error> {
    Ok(Value::from(()))
}

/// `Ok` when `me` can take `extra` more items.
fn room(me: &MutList, extra: usize) -> Result<(), Error> {
    check_items(me.read(Vec::len) as u128 + extra as u128)
}

/// Runs a method of `list` on `list`. `None` when `name` is no method of a list.
pub(super) fn call(me: &MutList, name: &str, args: &[Value]) -> Option<Result<Value, Error>> {
    let call = Call::of("list", args);
    Some(match name {
        "append" => call.one(name).and_then(|v| {
            room(me, 1)?;
            let item = snapshot(v);
            me.with(|items| items.push(item));
            none()
        }),
        "extend" => call.one(name).and_then(|v| {
            let more = iterate(v)?;
            room(me, more.len())?;
            let more: Vec<Value> = more.iter().map(snapshot).collect();
            me.with(|items| items.extend(more));
            none()
        }),
        "insert" => call.between(name, 2, 2).and_then(|a| {
            let at = index(&a[0])?;
            room(me, 1)?;
            let item = snapshot(&a[1]);
            me.with(|items| {
                let len = i64::try_from(items.len()).unwrap_or(i64::MAX);
                let at = if at < 0 {
                    (at + len).max(0)
                } else {
                    at.min(len)
                };
                items.insert(usize::try_from(at).unwrap_or(0), item);
            });
            none()
        }),
        "remove" => call.one(name).and_then(|v| {
            // The item is copied first: comparing it with a list that is itself locked waits for ever.
            let v = snapshot(v);
            match me.read(|items| items.iter().position(|x| *x == v)) {
                Some(at) => {
                    me.with(|items| items.remove(at));
                    none()
                }
                None => Err(raise("list.remove(x): x not in list")),
            }
        }),
        "pop" => call.between(name, 0, 1).and_then(|a| {
            let at = a.first().map_or(Ok(-1), index)?;
            me.with(|items| {
                if items.is_empty() {
                    return Err(raise("pop from empty list"));
                }
                let len = i64::try_from(items.len()).unwrap_or(i64::MAX);
                let at = if at < 0 { at + len } else { at };
                match usize::try_from(at).ok().filter(|&i| i < items.len()) {
                    Some(i) => Ok(items.remove(i)),
                    None => Err(raise("pop index out of range")),
                }
            })
        }),
        "sort" => sort(me, &call),
        "reverse" => call.none(name).map(|()| {
            me.with(|items| items.reverse());
            Value::from(())
        }),
        "clear" => call.none(name).map(|()| {
            me.with(Vec::clear);
            Value::from(())
        }),
        "copy" => call.none(name).map(|()| me.copy_of()),
        "index" => call.between(name, 1, 3).and_then(|a| {
            let (start, end) = (bound(a.get(1))?, bound(a.get(2))?);
            let item = snapshot(&a[0]);
            me.read(|items| {
                let (start, end) = clamp(items.len(), start, end);
                items
                    .iter()
                    .enumerate()
                    .take(end)
                    .skip(start)
                    .find(|(_, x)| **x == item)
                    .map(|(at, _)| Value::from(at))
                    .ok_or_else(|| raise(format!("{} is not in list", repr(&item))))
            })
        }),
        "count" => call.one(name).map(|v| {
            let item = snapshot(v);
            Value::from(me.read(|items| items.iter().filter(|x| **x == item).count()))
        }),
        _ => return None,
    })
}

/// `sort(*, key=None, reverse=False)`.
fn sort(me: &MutList, call: &Call<'_>) -> Result<Value, Error> {
    if !call.pos.is_empty() {
        return Err(raise("sort() takes no positional arguments"));
    }
    let mut reverse = false;
    for (key, value) in &call.kw {
        match key.as_str() {
            "reverse" => reverse = value.is_true(),
            "key" if value.is_none() => {}
            "key" => {
                return Err(raise(if value.kind() == ValueKind::Plain {
                    "a sort key function is not available yet".to_string()
                } else {
                    format!("'{}' object is not callable", type_of(value))
                }));
            }
            other => {
                return Err(raise(format!(
                    "'{other}' is an invalid keyword argument for sort()"
                )));
            }
        }
    }
    let items = crate::template::order::sort(me.items().to_vec(), &|v| v, reverse)?;
    me.with(|current| *current = items);
    none()
}
