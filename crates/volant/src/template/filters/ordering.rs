// SPDX-License-Identifier: GPL-3.0-or-later
//! Jinja2's `sort`, `min`, `max` and `unique`, which compare as Python does through
//! [`order`](crate::template::order): exactly for every integer, and refusing values Python
//! cannot order (`['a', 1] | sort`).

use std::cmp::Ordering;
use std::collections::BTreeSet;

use minijinja::value::{Kwargs, Rest};
use minijinja::{Error, ErrorKind, Value};

use super::regex::bind;
use crate::template::order::{order, sort as sort_by, unique_key};
use crate::template::pymethods::type_of;

/// `sort(value, reverse=False, case_sensitive=False, attribute=None)`. The attribute may name
/// several, separated by commas, which compare in turn.
pub(super) fn sort(value: Value, rest: Rest<Value>, kwargs: Kwargs) -> Result<Value, Error> {
    let a = bind(
        "sort",
        &["reverse", "case_sensitive", "attribute"],
        &rest,
        &kwargs,
        true,
    )?;
    let reverse = flag(a[0].as_ref());
    let case_sensitive = flag(a[1].as_ref());
    let keyed = value
        .try_iter()?
        .map(|item| {
            let key = match &a[2] {
                Some(attr) if attr.as_str().is_some_and(|s| s.contains(',')) => {
                    let names = attr.as_str().unwrap_or_default().split(',');
                    Value::from(
                        names
                            .map(|name| {
                                folded(attribute(&item, &Value::from(name)), case_sensitive)
                            })
                            .collect::<Vec<_>>(),
                    )
                }
                Some(attr) => folded(attribute(&item, attr), case_sensitive),
                None => folded(item.clone(), case_sensitive),
            };
            (key, item)
        })
        .collect();
    let sorted = sort_by(keyed, &|(key, _)| key, reverse)?;
    Ok(Value::from(
        sorted.into_iter().map(|(_, item)| item).collect::<Vec<_>>(),
    ))
}

/// `min(value, case_sensitive=False, attribute=None)`: the first smallest item, undefined when
/// there is none.
pub(super) fn min(value: Value, rest: Rest<Value>, kwargs: Kwargs) -> Result<Value, Error> {
    extreme("min", value, &rest, &kwargs, "<", Ordering::Less)
}

/// `max(value, case_sensitive=False, attribute=None)`: the first largest item.
pub(super) fn max(value: Value, rest: Rest<Value>, kwargs: Kwargs) -> Result<Value, Error> {
    extreme("max", value, &rest, &kwargs, ">", Ordering::Greater)
}

/// Python's `min` or `max`: an item replaces the one kept only when it compares `op` to it, so
/// the first of equal items stays.
fn extreme(
    name: &str,
    value: Value,
    rest: &[Value],
    kwargs: &Kwargs,
    op: &str,
    wins: Ordering,
) -> Result<Value, Error> {
    let a = bind(name, &["case_sensitive", "attribute"], rest, kwargs, true)?;
    let case_sensitive = flag(a[0].as_ref());
    let mut kept: Option<(Value, Value)> = None;
    for item in value.try_iter()? {
        let key = match &a[1] {
            Some(attr) => folded(attribute(&item, attr), case_sensitive),
            None => folded(item.clone(), case_sensitive),
        };
        let replaces = match &kept {
            None => true,
            Some((best, _)) => order(&key, best, op)? == Some(wins),
        };
        if replaces {
            kept = Some((key, item));
        }
    }
    Ok(kept.map_or(Value::UNDEFINED, |(_, item)| item))
}

/// `unique(value, case_sensitive=False, attribute=None)`: each item whose key no earlier item
/// had, the keys told apart as Python's set does.
pub(super) fn unique(value: Value, rest: Rest<Value>, kwargs: Kwargs) -> Result<Value, Error> {
    let a = bind(
        "unique",
        &["case_sensitive", "attribute"],
        &rest,
        &kwargs,
        true,
    )?;
    let case_sensitive = flag(a[0].as_ref());
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for item in value.try_iter()? {
        // Measured on ansible-core 2.19.12: an item without the attribute fails the filter.
        let key = match &a[1] {
            Some(attr) => walk(&item, attr).map_err(|said| {
                Error::new(
                    ErrorKind::InvalidOperation,
                    format!(
                        "Jinja2's unique filter failed and we cannot fall back to Ansible's                          version as it does not support the parameters supplied: {said}"
                    ),
                )
            })?,
            None => item.clone(),
        };
        let fresh = unique_key(&folded(key, case_sensitive)).is_none_or(|key| seen.insert(key));
        if fresh {
            out.push(item);
        }
    }
    Ok(Value::from(out))
}

/// Whether an argument is given and true.
fn flag(slot: Option<&Value>) -> bool {
    slot.is_some_and(Value::is_true)
}

/// A text in lower case unless the comparison is case-sensitive; any other value as it is.
fn folded(value: Value, case_sensitive: bool) -> Value {
    match value.as_str() {
        Some(text) if !case_sensitive => Value::from(text.to_lowercase()),
        _ => value,
    }
}

/// What Jinja2's attribute getter reads of `item`: a dotted path, a part made of digits being an
/// index, or an integer index. Undefined when a part is missing.
fn attribute(item: &Value, attr: &Value) -> Value {
    walk(item, attr).unwrap_or(Value::UNDEFINED)
}

/// [`attribute`], or Python's words for the part that is missing.
fn walk(item: &Value, attr: &Value) -> Result<Value, String> {
    let missing = |at: &Value, part: &str| {
        format!("object of type '{}' has no attribute '{part}'", type_of(at))
    };
    let Some(path) = attr.as_str() else {
        return match item.get_item(attr) {
            Ok(found) if !found.is_undefined() => Ok(found),
            _ => Err(missing(item, &attr.to_string())),
        };
    };
    let mut at = item.clone();
    for part in path.split('.') {
        let key = match part.parse::<u64>() {
            Ok(index) if part.bytes().all(|b| b.is_ascii_digit()) => Value::from(index),
            _ => Value::from(part),
        };
        at = match at.get_item(&key) {
            Ok(next) if !next.is_undefined() => next,
            _ => return Err(missing(&at, part)),
        };
    }
    Ok(at)
}
