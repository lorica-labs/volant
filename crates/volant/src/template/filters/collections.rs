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
    if kwargs
        .args()
        .any(|k| !["recursive", "list_merge"].contains(&k))
    {
        return Err(invalid(
            "'recursive' and 'list_merge' are the only valid keyword arguments",
        ));
    }
    if ![
        "replace",
        "keep",
        "append",
        "prepend",
        "append_rp",
        "prepend_rp",
    ]
    .contains(&list_merge.as_str())
    {
        return Err(invalid(
            "merge_hash: 'list_merge' argument can only be equal to 'replace', 'keep', 'append', \
             'prepend', 'append_rp' or 'prepend_rp'",
        ));
    }
    // The reference's `flatten(terms, levels=1)`: a list among the terms gives its items, and a
    // null, `'None'` or `'null'` drops out.
    let null = |v: &serde_json::Value| v.is_null() || v == "None" || v == "null";
    let mut dicts = Vec::new();
    for term in std::iter::once(&first).chain(rest.iter()) {
        match data(term)? {
            serde_json::Value::Array(items) => dicts.extend(items.into_iter().filter(|v| !null(v))),
            v if !null(&v) => dicts.push(v),
            _ => {}
        }
    }
    let n = dicts.len();
    if n < 2 {
        let one = dicts.pop();
        return Ok(from_json_value(
            one.unwrap_or_else(|| serde_json::json!({})),
        ));
    }
    // The reference merges from the last term down, so the first pair it refuses is the last one
    // with a term that is no dict.
    if let Some(i) = (0..n - 1)
        .rev()
        .find(|&i| !dicts[i].is_object() || (i == n - 2 && !dicts[n - 1].is_object()))
    {
        let higher = if i == n - 2 {
            crate::template::env::type_name(&dicts[n - 1])
        } else {
            "dict"
        };
        return Err(invalid(format!(
            "failed to combine variables, expected dicts but got a '{}' and a '{higher}'.",
            crate::template::env::type_name(&dicts[i])
        )));
    }
    let mut dicts = dicts.into_iter().filter_map(|v| match v {
        serde_json::Value::Object(m) => Some(m),
        _ => None,
    });
    let mut out = dicts.next().unwrap_or_default();
    for other in dicts {
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
            // `_rp`, "remove present": what the lower list holds that the higher one also holds
            // goes, and the higher one is put after it, or before it.
            (Some(serde_json::Value::Array(t)), serde_json::Value::Array(mut s))
                if list_merge == "append_rp" || list_merge == "prepend_rp" =>
            {
                t.retain(|z| !s.contains(z));
                if list_merge == "append_rp" {
                    t.append(&mut s);
                } else {
                    s.append(t);
                    *t = s;
                }
            }
            (Some(serde_json::Value::Array(_)), serde_json::Value::Array(_))
                if list_merge == "keep" => {}
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
    // A key YAML gave a type keeps it: see `template::typed_keys`.
    let keys = crate::template::typed_keys::keys(&map)
        .unwrap_or_else(|| map.keys().map(|k| Value::from(k.as_str())).collect());
    let items: Vec<Value> = keys
        .into_iter()
        .zip(map)
        .map(|(k, (_, v))| {
            Value::from_pairs([
                (Value::from(key_name.as_str()), k),
                (Value::from(value_name.as_str()), from_json_value(v)),
            ])
        })
        .collect();
    Ok(Value::from(items))
}

pub(super) fn items2dict(value: Value, kwargs: Kwargs) -> Result<Value, Error> {
    let key_name: String = kwargs
        .get::<Option<String>>("key_name")?
        .unwrap_or_else(|| "key".to_string());
    let value_name: String = kwargs
        .get::<Option<String>>("value_name")?
        .unwrap_or_else(|| "value".to_string());
    kwargs.assert_all_used()?;
    // Ported from `list_of_dict_key_value_elements_to_dict` in `ansible/plugins/filter/core.py`
    // (2.19.12), its three errors in its words.
    let given = data(&value)?;
    let serde_json::Value::Array(items) = &given else {
        return Err(invalid(format!(
            "items2dict requires a list, got <class '{}'> instead.",
            crate::template::type_name(&given)
        )));
    };
    let shown = crate::playbook::python_repr(&given);
    let mut out = serde_json::Map::new();
    for item in items {
        let serde_json::Value::Object(map) = item else {
            return Err(invalid(format!(
                "items2dict requires a list of dictionaries, got {shown} instead."
            )));
        };
        let (Some(key), Some(value)) = (map.get(&key_name), map.get(&value_name)) else {
            return Err(invalid(format!(
                "items2dict requires each dictionary in the list to contain the keys '{key_name}' and '{value_name}', got {shown} instead."
            )));
        };
        let key = key.as_str().map_or_else(|| key.to_string(), str::to_string);
        out.insert(key, value.clone());
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

/// `subelements(obj, subelements, skip_missing=False)`, ported from
/// `ansible/plugins/filter/core.py` (2.19.12): each element of `obj` paired with each value the
/// dotted path, or the list of keys, reaches in it, as a tuple, with the reference's errors.
pub(super) fn subelements(value: Value, path: Value, kwargs: Kwargs) -> Result<Value, Error> {
    use crate::playbook::python_repr;
    use serde_json::Value as Json;
    let skip_missing: bool = kwargs.get::<Option<bool>>("skip_missing")?.unwrap_or(false);
    kwargs.assert_all_used()?;
    let elements = match data(&value)? {
        Json::Object(map) => map.into_iter().map(|(_, v)| v).collect(),
        Json::Array(items) => items,
        _ => return Err(invalid("obj must be a list of dicts or a nested dict")),
    };
    let keys = match data(&path)? {
        Json::Array(keys) => keys,
        Json::String(text) => text.split('.').map(|k| Json::String(k.into())).collect(),
        _ => return Err(invalid("subelements must be a list or a string")),
    };
    let mut out = Vec::new();
    for element in elements {
        let mut values = element.clone();
        for key in &keys {
            let next = match (&values, key) {
                (Json::Object(map), Json::String(k)) => map.get(k).cloned().ok_or(true),
                (Json::Array(items), Json::Number(n)) => n
                    .as_u64()
                    .and_then(|i| items.get(usize::try_from(i).ok()?).cloned())
                    .ok_or(true),
                _ => Err(false),
            };
            values = match next {
                Ok(found) => found,
                Err(true) if skip_missing => {
                    values = Json::Array(Vec::new());
                    break;
                }
                Err(true) => {
                    return Err(invalid(format!(
                        "could not find {} key in iterated item {}",
                        python_repr(key),
                        python_repr(&values)
                    )));
                }
                Err(false) => {
                    let key = key.as_str().map_or_else(|| key.to_string(), str::to_string);
                    return Err(invalid(format!(
                        "the key {key} should point to a dictionary, got '{}'",
                        python_repr(&values)
                    )));
                }
            };
        }
        let Json::Array(found) = values else {
            return Err(invalid(format!(
                "the key {} should point to a list, got {}",
                python_repr(keys.last().unwrap_or(&Json::Null)),
                python_repr(&values)
            )));
        };
        for one in found {
            let pair = vec![from_json_value(element.clone()), from_json_value(one)];
            out.push(Value::from(minijinja::value::Tuple::from(pair)));
        }
    }
    Ok(Value::from(out))
}
