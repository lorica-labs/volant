// SPDX-License-Identifier: GPL-3.0-or-later

//! The checks between options of ansible-core 2.19.12's `module_utils/common/validation.py`
//! that a sub-option spec can ask for: `mutually_exclusive` and the `required_*` family. Each
//! raises on the first group it finds broken, so each adds at most one error.

use serde_json::{Map, Value};

use crate::executor::prepare::display;
use crate::template::truthy;

use super::types::py_eq;
use super::validate::found_in;

/// The names a term holds, as `count_terms` reads it: a string is one name.
fn names(term: &Value) -> Vec<&str> {
    match term {
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
        Value::String(name) => vec![name.as_str()],
        _ => Vec::new(),
    }
}

/// `count_terms`: how many distinct names of the term are set.
fn count_terms(term: &Value, params: &Map<String, Value>) -> usize {
    let mut names = names(term);
    names.sort_unstable();
    names.dedup();
    names.iter().filter(|n| params.contains_key(**n)).count()
}

/// The items `for x in term` walks: a list's items, a string's characters.
fn items(term: &Value) -> Vec<Value> {
    match term {
        Value::Array(items) => items.clone(),
        Value::String(s) => s.chars().map(|c| Value::from(c.to_string())).collect(),
        _ => Vec::new(),
    }
}

/// `sep.join(term)`.
fn joined(term: &Value, sep: &str) -> String {
    items(term)
        .iter()
        .map(display)
        .collect::<Vec<_>>()
        .join(sep)
}

/// `check_mutually_exclusive`: every group with more than one of its names set.
pub(super) fn mutually_exclusive(
    terms: Option<&Value>,
    params: &Map<String, Value>,
    context: &[String],
) -> Result<(), String> {
    let Some(Value::Array(groups)) = terms else {
        return Ok(());
    };
    let clashing: Vec<String> = groups
        .iter()
        .filter(|group| count_terms(group, params) > 1)
        .map(|group| joined(group, "|"))
        .collect();
    if clashing.is_empty() {
        return Ok(());
    }
    Err(found_in(
        format!("parameters are mutually exclusive: {}", clashing.join(", ")),
        context,
    ))
}

/// `required_together`, `required_one_of`, `required_if` and `required_by` of an option, in
/// that order.
pub(super) fn required(
    option: &Value,
    params: &Map<String, Value>,
    context: &[String],
) -> Vec<String> {
    let groups = |key| match option.get(key) {
        Some(Value::Array(groups)) => groups.clone(),
        _ => Vec::new(),
    };
    let together = groups("required_together").into_iter().find(|term| {
        let counts: Vec<usize> = items(term).iter().map(|f| count_terms(f, params)).collect();
        counts.iter().any(|&c| c > 0) && counts.contains(&0)
    });
    let one_of = groups("required_one_of")
        .into_iter()
        .find(|term| count_terms(term, params) == 0);
    let found = [
        together.map(|t| format!("parameters are required together: {}", joined(&t, ", "))),
        one_of.map(|t| format!("one of the following is required: {}", joined(&t, ", "))),
        groups("required_if")
            .iter()
            .find_map(|req| required_if(req, params)),
        option
            .get("required_by")
            .and_then(|by| required_by(by, params)),
    ];
    found
        .into_iter()
        .flatten()
        .map(|msg| found_in(msg, context))
        .collect()
}

/// One `[key, value, names]` or `[key, value, names, any]` of `required_if`.
fn required_if(req: &Value, params: &Map<String, Value>) -> Option<String> {
    let (key, value, wanted, any) = match req.as_array()?.as_slice() {
        [key, value, wanted] => (key.as_str()?, value, wanted, false),
        [key, value, wanted, any] => (key.as_str()?, value, wanted, truthy(any)),
        _ => return None,
    };
    if !params.get(key).is_some_and(|v| py_eq(v, value)) {
        return None;
    }
    let wanted = items(wanted);
    let missing: Vec<String> = wanted
        .iter()
        .filter(|name| count_terms(name, params) == 0)
        .map(display)
        .collect();
    let enough = if any { wanted.len() } else { 0 };
    (!missing.is_empty() && missing.len() >= enough).then(|| {
        format!(
            "{key} is {} but {} of the following are missing: {}",
            display(value),
            if any { "any" } else { "all" },
            missing.join(", ")
        )
    })
}

/// `required_by`: the first set name whose companions are not all set.
fn required_by(by: &Value, params: &Map<String, Value>) -> Option<String> {
    let set = |name: &str| params.get(name).is_some_and(|v| !v.is_null());
    by.as_object()?.iter().find_map(|(key, companions)| {
        if !set(key) {
            return None;
        }
        let missing: Vec<&str> = names(companions).into_iter().filter(|n| !set(n)).collect();
        (!missing.is_empty()).then(|| {
            format!(
                "missing parameter(s) required by '{key}': {}",
                missing.join(", ")
            )
        })
    })
}
