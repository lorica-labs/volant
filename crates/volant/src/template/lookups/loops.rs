// SPDX-License-Identifier: GPL-3.0-or-later
//! The lookups `with_<name>` loops are made of: `dict`, `together`, `nested` and `subelements`,
//! read from ansible-core 2.19.12's plugins and `LookupBase._flatten`/`_combine`.

use std::collections::BTreeMap;

use minijinja::value::ValueKind;
use minijinja::{Error, Value};

use super::basic::python_type;
use super::invalid;
use crate::template::pymethods::format::to_str;

/// `dict`: each term's items as `{"key": k, "value": v}`, in the term's order.
pub(super) fn dict(terms: &[Value]) -> Result<Vec<Value>, Error> {
    let mut out = Vec::new();
    for term in terms {
        if term.kind() != ValueKind::Map {
            return Err(invalid(format!(
                "the 'dict' lookup plugin expects a dictionary, got '{}' of type {})",
                to_str(term),
                python_type(term)
            )));
        }
        for key in term.try_iter()? {
            let value = term.get_item(&key)?;
            out.push(Value::from(BTreeMap::from([
                ("key", key),
                ("value", value),
            ])));
        }
    }
    Ok(out)
}

/// `together`: the terms side by side, the shorter ones padded with `None`, each row flattened.
pub(super) fn together(terms: &[Value]) -> Result<Vec<Value>, Error> {
    if terms.is_empty() {
        return Err(invalid(
            "with_together requires at least one element in each list",
        ));
    }
    let columns: Vec<Vec<Value>> = terms.iter().map(items).collect::<Result<_, _>>()?;
    let rows = columns.iter().map(Vec::len).max().unwrap_or(0);
    (0..rows)
        .map(|row| {
            let cells: Vec<Value> = columns
                .iter()
                .map(|column| column.get(row).cloned().unwrap_or(Value::from(())))
                .collect();
            Ok(Value::from(flatten(&cells)))
        })
        .collect()
}

/// `nested`: every combination of the terms' items, first term outermost, each flattened.
pub(super) fn nested(terms: &[Value]) -> Result<Vec<Value>, Error> {
    let Some((first, rest)) = terms.split_first() else {
        return Err(invalid(
            "with_nested requires at least one element in the nested list",
        ));
    };
    let mut result = items(first)?;
    for term in rest {
        let right = items(term)?;
        result = result
            .iter()
            .flat_map(|x| {
                right
                    .iter()
                    .map(move |y| Value::from(flatten(&[x.clone(), y.clone()])))
            })
            .collect();
    }
    result
        .iter()
        .map(|x| Ok(Value::from(flatten(&items(x)?))))
        .collect()
}

/// `subelements`: `[item, sub]` for each `sub` of the list `item` holds under the dotted key,
/// the list taken out of `item` as the plugin's `pop` takes it.
pub(super) fn subelements(terms: &[Value]) -> Result<Vec<Value>, Error> {
    let terms_error = |more: &str| {
        invalid(format!(
            "subelements lookup expects a list of two or three items, {more}"
        ))
    };
    if !(2..=3).contains(&terms.len()) {
        return Err(terms_error(""));
    }
    let Some(key) = terms[1].as_str().filter(|_| {
        matches!(terms[0].kind(), ValueKind::Seq | ValueKind::Map) && !terms[0].is_tuple()
    }) else {
        return Err(terms_error(
            "first a dict or a list, second a string pointing to the subkey",
        ));
    };
    let subkeys: Vec<&str> = key.split('.').collect();
    let elements = if terms[0].kind() == ValueKind::Map {
        if skipped(&terms[0]) {
            return Ok(Vec::new());
        }
        terms[0]
            .try_iter()?
            .map(|k| terms[0].get_item(&k))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        items(&terms[0])?
    };
    let skip_missing = match terms.get(2) {
        None => false,
        Some(flags) if flags.kind() == ValueKind::Map => {
            lenient_bool(&flags.get_attr("skip_missing").unwrap_or_default())
        }
        Some(_) => {
            return Err(terms_error(
                "the optional third item must be a dict with flags skip_missing",
            ));
        }
    };
    let mut out = Vec::new();
    for item in elements {
        if item.kind() != ValueKind::Map {
            return Err(invalid(format!(
                "subelements lookup expects a dictionary, got '{}'",
                to_str(&item)
            )));
        }
        if skipped(&item) {
            continue;
        }
        let mut sublist = Vec::new();
        let mut path = Vec::new();
        let mut value = item.clone();
        for (i, subkey) in subkeys.iter().enumerate() {
            let last = i + 1 == subkeys.len();
            let Some(found) = get(&value, subkey) else {
                if skip_missing {
                    continue;
                }
                return Err(invalid(format!(
                    "could not find '{subkey}' key in iterated item '{}'",
                    to_str(&value)
                )));
            };
            if !last {
                if found.kind() != ValueKind::Map {
                    if skip_missing {
                        continue;
                    }
                    return Err(invalid(format!(
                        "the key {subkey} should point to a dictionary, got '{}'",
                        to_str(&found)
                    )));
                }
                path.push(*subkey);
                value = found;
            } else if found.kind() != ValueKind::Seq || found.is_tuple() {
                return Err(invalid(format!(
                    "the key {subkey} should point to a list, got '{}'",
                    to_str(&found)
                )));
            } else {
                sublist = items(&found)?;
                path.push(*subkey);
            }
        }
        let item = if sublist.is_empty() && path.len() < subkeys.len() {
            item
        } else {
            without(&item, &path)
        };
        out.extend(
            sublist
                .into_iter()
                .map(|sub| Value::from(vec![item.clone(), sub])),
        );
    }
    Ok(out)
}

/// `x.get('skipped', False) is not False`.
fn skipped(map: &Value) -> bool {
    get(map, "skipped").is_some_and(|v| !(v.kind() == ValueKind::Bool && !v.is_true()))
}

fn get(map: &Value, key: &str) -> Option<Value> {
    map.get_item(&Value::from(key))
        .ok()
        .filter(|value| !value.is_undefined())
}

/// `map` without the last key of `path`: `subvalue.pop(subkey)`, on a copy.
fn without(map: &Value, path: &[&str]) -> Value {
    let Ok(mut copy) = crate::template::bigint::to_json(map) else {
        return map.clone();
    };
    let mut at = &mut copy;
    for (i, key) in path.iter().enumerate() {
        let Some(object) = at.as_object_mut() else {
            return map.clone();
        };
        if i + 1 == path.len() {
            object.shift_remove(*key);
            break;
        }
        match object.get_mut(*key) {
            Some(next) => at = next,
            None => return map.clone(),
        }
    }
    crate::template::bigint::to_template(&copy)
}

/// The items of an iterable, as Python's `for` walks it: a string's characters, a map's keys.
fn items(value: &Value) -> Result<Vec<Value>, Error> {
    match value.try_iter() {
        Ok(iter) if value.kind() != ValueKind::Undefined => Ok(iter.collect()),
        _ => {
            let kind = python_type(value)
                .trim_start_matches("<class '")
                .trim_end_matches("'>");
            Err(invalid(format!("'{kind}' object is not iterable")))
        }
    }
}

/// `LookupBase._flatten`: lists spliced in one level, anything else kept.
fn flatten(terms: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for term in terms {
        if term.kind() == ValueKind::Seq {
            out.extend(term.try_iter().into_iter().flatten());
        } else {
            out.push(term.clone());
        }
    }
    out
}

/// `boolean(value, strict=False)`: true for its true spellings, false for anything else.
fn lenient_bool(value: &Value) -> bool {
    match value.kind() {
        ValueKind::Bool => value.is_true(),
        ValueKind::Number => f64::try_from(value.clone()).ok() == Some(1.0),
        ValueKind::String => matches!(
            value.as_str().unwrap_or_default().to_lowercase().trim(),
            "y" | "yes" | "on" | "1" | "true" | "t"
        ),
        _ => false,
    }
}
