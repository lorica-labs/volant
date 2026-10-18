// SPDX-License-Identifier: GPL-3.0-or-later
//! JSON and YAML filters: `to_json`, `to_nice_json`, `from_json`, `from_yaml`, and the
//! Python-compatible serializer under them.

use std::fmt::Write;

use minijinja::value::Kwargs;
use minijinja::{Error, Value};

use super::{data, from_json_value, invalid};

/// Python's `json.dumps` default separators: `", "` and `": "`, keys in insertion order.
/// Measured against ansible-core 2.19.12: `{b: 1, a: 2, c: 3} | to_json` gives
/// `{"b": 1, "a": 2, "c": 3}`, so `to_json` keeps the order the mapping was written in.
///
/// Both filters keep `json.dumps`'s `ensure_ascii=True` unless told otherwise: measured,
/// `{'é': 1} | to_json` gives `{"é": 1}`.
pub(super) fn to_json(value: Value, kwargs: Kwargs) -> Result<Value, Error> {
    let ensure_ascii = ensure_ascii(&kwargs)?;
    kwargs.assert_all_used()?;
    Ok(Value::from(python_json(
        &data(&value)?,
        None,
        false,
        ensure_ascii,
        0,
    )))
}

/// `to_nice_json` is the one that sorts: the reference calls `json.dumps` with
/// `sort_keys=True`, and the same mapping comes back as `{"a": 2, "b": 1, "c": 3}`.
pub(super) fn to_nice_json(value: Value, kwargs: Kwargs) -> Result<Value, Error> {
    let indent: usize = kwargs.get::<Option<usize>>("indent")?.unwrap_or(4);
    let ensure_ascii = ensure_ascii(&kwargs)?;
    kwargs.assert_all_used()?;
    Ok(Value::from(python_json(
        &data(&value)?,
        Some(indent),
        true,
        ensure_ascii,
        0,
    )))
}

pub(super) fn ensure_ascii(kwargs: &Kwargs) -> Result<bool, Error> {
    Ok(kwargs.get::<Option<bool>>("ensure_ascii")?.unwrap_or(true))
}

/// Python's `json.dumps`, the one imitation of it in this crate: `", "` between items without an
/// indent and `","` with one, `": "` after a key, keys in their order unless `sort_keys`. With
/// `ensure_ascii`, which is `json.dumps`' own default, every character past ASCII is escaped as
/// `\uXXXX`, a UTF-16 surrogate pair for one outside the basic plane.
pub(crate) fn python_json(
    v: &serde_json::Value,
    indent: Option<usize>,
    sort_keys: bool,
    ensure_ascii: bool,
    depth: usize,
) -> String {
    let pad = |d: usize| {
        indent
            .map(|i| format!("\n{}", " ".repeat(i * d)))
            .unwrap_or_default()
    };
    let string = |s: &str| {
        let quoted = serde_json::to_string(s).unwrap_or_default();
        if !ensure_ascii {
            return quoted;
        }
        let mut out = String::new();
        for c in quoted.chars() {
            // Python escapes everything outside space to `~`, so DEL too; serde_json has
            // already escaped the control characters below space.
            if c.is_ascii() && c != '\u{7f}' {
                out.push(c);
            } else {
                for unit in c.encode_utf16(&mut [0; 2]) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
        out
    };
    match v {
        serde_json::Value::Object(map) if map.is_empty() => "{}".to_string(),
        serde_json::Value::Array(items) if items.is_empty() => "[]".to_string(),
        serde_json::Value::Object(map) => {
            let sep = if indent.is_some() { "," } else { ", " };
            let mut keys: Vec<&String> = map.keys().collect();
            if sort_keys {
                keys.sort();
            }
            let fields: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    format!(
                        "{}{}: {}",
                        pad(depth + 1),
                        string(k),
                        python_json(&map[k], indent, sort_keys, ensure_ascii, depth + 1)
                    )
                })
                .collect();
            format!("{{{}{}}}", fields.join(sep), pad(depth))
        }
        serde_json::Value::Array(items) => {
            let sep = if indent.is_some() { "," } else { ", " };
            let fields: Vec<String> = items
                .iter()
                .map(|v| {
                    format!(
                        "{}{}",
                        pad(depth + 1),
                        python_json(v, indent, sort_keys, ensure_ascii, depth + 1)
                    )
                })
                .collect();
            format!("[{}{}]", fields.join(sep), pad(depth))
        }
        serde_json::Value::String(s) => string(s),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

pub(super) fn from_json(value: Value) -> Result<Value, Error> {
    let text = value
        .as_str()
        .ok_or_else(|| invalid("from_json requires a string"))?;
    let parsed: serde_json::Value =
        serde_json::from_str(text).map_err(|e| invalid(format!("from_json: {e}")))?;
    Ok(from_json_value(parsed))
}

/// One YAML document read by the engine's own reader; anything that is not a string, `None`
/// included, comes back as it is.
pub(super) fn from_yaml(value: Value) -> Result<Value, Error> {
    let Some(text) = value.as_str() else {
        return Ok(value);
    };
    let docs = crate::yaml::load(text, "from_yaml").map_err(|e| invalid(format!("{e:#}")))?;
    match docs.as_slice() {
        [] => Ok(Value::from(())),
        [doc] => crate::yaml::to_json(doc)
            .map(from_json_value)
            .map_err(|e| invalid(format!("from_yaml: {e:#}"))),
        _ => Err(invalid(
            "from_yaml: expected a single document in the stream",
        )),
    }
}
