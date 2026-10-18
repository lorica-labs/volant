// SPDX-License-Identifier: GPL-3.0-or-later
//! Filters over lists and dicts: `combine`, `dict2items`, `items2dict`, the set filters, `flatten` and
//! `extract`.

use minijinja::value::{Kwargs, Rest, ValueKind};
use minijinja::{Error, ErrorKind, Value};

use super::{data, from_json_value, invalid};

pub(super) fn combine(first: Value, rest: Rest<Value>, kwargs: Kwargs) -> Result<Value, Error> {
    let recursive: bool = kwargs.get::<Option<bool>>("recursive")?.unwrap_or(false);
    let list_merge: String = kwargs
        .get::<Option<String>>("list_merge")?
        .unwrap_or_else(|| "replace".to_string());
    kwargs.assert_all_used()?;
    let serde_json::Value::Object(mut out) = data(&first)? else {
        return Err(invalid("|combine expects dictionaries"));
    };
    for other in rest.iter() {
        let serde_json::Value::Object(other) = data(other)? else {
            return Err(invalid("|combine expects dictionaries"));
        };
        merge_into(&mut out, other, recursive, &list_merge);
    }
    Ok(from_json_value(serde_json::Value::Object(out)))
}

pub(super) fn merge_into(
    target: &mut serde_json::Map<String, serde_json::Value>,
    source: serde_json::Map<String, serde_json::Value>,
    recursive: bool,
    list_merge: &str,
) {
    for (k, v) in source {
        match (target.get_mut(&k), v) {
            (Some(serde_json::Value::Object(t)), serde_json::Value::Object(s)) if recursive => {
                merge_into(t, s, recursive, list_merge);
            }
            (Some(serde_json::Value::Array(t)), serde_json::Value::Array(s))
                if list_merge == "append" =>
            {
                t.extend(s);
            }
            (Some(serde_json::Value::Array(t)), serde_json::Value::Array(mut s))
                if list_merge == "prepend" =>
            {
                s.append(t);
                *t = s;
            }
            (_, v) => {
                target.insert(k, v);
            }
        }
    }
}

pub(super) fn dict2items(value: Value, kwargs: Kwargs) -> Result<Value, Error> {
    let key_name: String = kwargs
        .get::<Option<String>>("key_name")?
        .unwrap_or_else(|| "key".to_string());
    let value_name: String = kwargs
        .get::<Option<String>>("value_name")?
        .unwrap_or_else(|| "value".to_string());
    kwargs.assert_all_used()?;
    let serde_json::Value::Object(map) = data(&value)? else {
        return Err(invalid("dict2items requires a dictionary"));
    };
    let items: Vec<serde_json::Value> = map
        .into_iter()
        .map(|(k, v)| serde_json::json!({ key_name.clone(): k, value_name.clone(): v }))
        .collect();
    Ok(from_json_value(serde_json::Value::Array(items)))
}

pub(super) fn items2dict(value: Value, kwargs: Kwargs) -> Result<Value, Error> {
    let key_name: String = kwargs
        .get::<Option<String>>("key_name")?
        .unwrap_or_else(|| "key".to_string());
    let value_name: String = kwargs
        .get::<Option<String>>("value_name")?
        .unwrap_or_else(|| "value".to_string());
    kwargs.assert_all_used()?;
    let serde_json::Value::Array(items) = data(&value)? else {
        return Err(invalid("items2dict requires a list"));
    };
    let mut out = serde_json::Map::new();
    for item in items {
        let key = item
            .get(&key_name)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                invalid(format!(
                    "items2dict requires each item to have a '{key_name}' string"
                ))
            })?
            .to_string();
        out.insert(
            key,
            item.get(&value_name)
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        );
    }
    Ok(from_json_value(serde_json::Value::Object(out)))
}

#[derive(Clone, Copy)]
pub(super) enum SetFilter {
    Difference,
    Intersect,
    Union,
}

/// `difference`, `intersect` and `union` from `mathstuff.py`, each element once. The reference
/// builds a Python `set`, whose order is its own: small integers happen to come out sorted,
/// strings in an order that changes with `PYTHONHASHSEED`. This sorts when every element is an
/// integer, which is what the reference printed on every integer case measured, and keeps the
/// order of first appearance otherwise, which is the reference's own fallback for elements it
/// cannot hash.
pub(super) fn set_filter(a: &Value, b: &Value, which: SetFilter) -> Result<Value, Error> {
    let a: Vec<Value> = a.try_iter()?.collect();
    let b: Vec<Value> = b.try_iter()?.collect();
    let candidates: Vec<Value> = match which {
        SetFilter::Difference => a.into_iter().filter(|x| !b.contains(x)).collect(),
        SetFilter::Intersect => a.into_iter().filter(|x| b.contains(x)).collect(),
        SetFilter::Union => a.into_iter().chain(b).collect(),
    };
    let mut out: Vec<Value> = Vec::new();
    for x in candidates {
        if !out.contains(&x) {
            out.push(x);
        }
    }
    if out.iter().all(Value::is_integer) {
        out.sort();
    }
    Ok(Value::from(out))
}

/// `flatten(levels=None, skip_nulls=True)`: nested lists spliced in, `levels` deep at most,
/// and `None`, `'None'` and `'null'` dropped while `skip_nulls` holds.
pub(super) fn flatten(value: Value, levels: Option<i64>, kwargs: Kwargs) -> Result<Value, Error> {
    let levels = kwargs.get::<Option<i64>>("levels")?.or(levels);
    let skip_nulls = kwargs.get::<Option<bool>>("skip_nulls")?.unwrap_or(true);
    kwargs.assert_all_used()?;
    let serde_json::Value::Array(items) = data(&value)? else {
        return Err(invalid("flatten expects a list"));
    };
    Ok(from_json_value(serde_json::Value::Array(flatten_items(
        items, levels, skip_nulls,
    ))))
}

pub(super) fn flatten_items(
    items: Vec<serde_json::Value>,
    levels: Option<i64>,
    skip_nulls: bool,
) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for element in items {
        if skip_nulls && (element.is_null() || element == "None" || element == "null") {
            continue;
        }
        match (element, levels) {
            (serde_json::Value::Array(inner), None) => {
                out.extend(flatten_items(inner, None, skip_nulls));
            }
            (serde_json::Value::Array(inner), Some(n)) if n >= 1 => {
                out.extend(flatten_items(inner, Some(n - 1), skip_nulls));
            }
            (other, _) => out.push(other),
        }
    }
    out
}

/// `extract(item, container, morekeys=None)`, `core.py`'s: `container[item]`, then each of
/// `morekeys` (a list, or one key) in turn. A key that is not there gives an undefined value,
/// and the keys after it are not read, as the reference hands its undefined marker on: `default`
/// and `is defined` see it, and anything else fails as an undefined variable. A `hostvars`
/// container is read through its own object, so a host's untrusted values taint the render
/// here exactly as a bare `hostvars[h]` does.
///
/// `morekeys` is taken as it arrives rather than as an `Option`, which minijinja fills with
/// `None` for an undefined argument as well as for an omitted one: an undefined `morekeys` is a
/// key that cannot be read, so the result is undefined, as the reference's is.
pub(super) fn extract(
    item: Value,
    container: Value,
    Rest(morekeys): Rest<Value>,
) -> Result<Value, Error> {
    if morekeys.len() > 1 {
        return Err(Error::from(ErrorKind::TooManyArguments));
    }
    let mut keys = vec![item];
    match morekeys.into_iter().next() {
        Some(more) if more.kind() == ValueKind::Seq => keys.extend(more.try_iter()?),
        Some(more) if !more.is_none() => keys.push(more),
        _ => {}
    }
    let mut value = container;
    for key in &keys {
        if value.is_undefined() {
            break;
        }
        value = value.get_item(key)?;
    }
    Ok(value)
}
