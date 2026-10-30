// SPDX-License-Identifier: GPL-3.0-or-later
//! `d[key]` for a number, a boolean or null, on a dict whose YAML keys were of those types.
//!
//! The variables hold such a key as its text (`crate::yaml` loads `1:` as `"1"`, `true:` as
//! `"true"`, `~:` as `"None"`), while the reference keeps the Python value and finds it by equality.
//! Measured on ansible-core 2.19.12: `d[1]`, `d[2.5]`, `d[true]`, `d[false]` and `d[none]` read the
//! entries written `1:`, `2.5:`, `true:`, `false:` and `~:`, and `True == 1` lets `d[true]` find
//! `1:` too. `d["1"]` finds nothing there, which this text form cannot tell apart.

use minijinja::value::ValueKind;
use minijinja::{Error, Value};

/// `parent[key]`, a key that is not a string also tried as the texts a YAML key of its value
/// was loaded under.
pub(super) fn item(parent: &Value, key: &Value) -> Result<Value, Error> {
    let read = parent.get_item(key)?;
    if !read.is_undefined() || parent.kind() != ValueKind::Map {
        return Ok(read);
    }
    // A typed key equal by value: `True == 1 == 1.0`, as Python's dict finds them.
    for other in equals(key) {
        let found = parent.get_item(&other)?;
        if !found.is_undefined() {
            return Ok(found);
        }
    }
    for text in texts(key) {
        let found = parent.get_item(&Value::from(text))?;
        if !found.is_undefined() {
            return Ok(found);
        }
    }
    Ok(read)
}

/// The other typed keys equal to `key`.
fn equals(key: &Value) -> Vec<Value> {
    match key.kind() {
        ValueKind::Bool => vec![Value::from(i64::from(key.is_true()))],
        ValueKind::Number => match key.as_i64() {
            Some(n @ (0 | 1)) => vec![Value::from(n == 1), Value::from(n as f64)],
            Some(n) => vec![Value::from(n as f64)],
            None => f64::try_from(key.clone())
                .ok()
                .filter(|f| f.fract() == 0.0 && f.abs() < 9e15)
                .map(|f| vec![Value::from(f as i64)])
                .unwrap_or_default(),
        },
        _ => Vec::new(),
    }
}

/// The texts a YAML key equal to `key` is held under, in the order the reference would match.
fn texts(key: &Value) -> Vec<String> {
    let truth = |one: bool| {
        if one {
            vec!["true".to_string(), "1".to_string()]
        } else {
            vec!["false".to_string(), "0".to_string()]
        }
    };
    match key.kind() {
        ValueKind::Bool => truth(key.is_true()),
        ValueKind::None => vec!["None".to_string()],
        ValueKind::Number => match key.as_i64() {
            Some(n @ (0 | 1)) => {
                let mut out = vec![key.to_string()];
                out.extend(truth(n == 1));
                out
            }
            _ => vec![key.to_string()],
        },
        _ => Vec::new(),
    }
}
