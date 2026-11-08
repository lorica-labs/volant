// SPDX-License-Identifier: GPL-3.0-or-later
//! The values of a script's answer: where they come from, their types and their markers.

use serde_json::{Map, Value};

use super::py_repr;

/// Where the reference says an inventory script's values come from.
pub(super) fn script_origin(path: &str) -> crate::tags::OriginId {
    crate::tags::origin_described(&format!("<inventory script output from {}>", py_repr(path)))
}

/// A host's variables from a script's output, trusted as the reference trusts that output.
pub(super) fn script_tagged(path: &str, vars: Map<String, Value>) -> crate::tags::Tagged {
    let origin = script_origin(path);
    let mut out = crate::tags::Tagged::default();
    for (key, value) in vars {
        let (value, tree) = crate::yaml::unsafe_markers(&value, origin);
        out.insert(key, value, tree);
    }
    out
}

pub(super) fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// The type of the first `{"__ansible_vault": value}` in `map` whose value is not text, which
/// the reference's decoder refuses.
pub(super) fn vault_not_text(map: &Map<String, Value>) -> Option<&'static str> {
    match map.get("__ansible_vault") {
        Some(value) if map.len() == 1 => Some(type_name(value)).filter(|kind| *kind != "str"),
        _ => map.values().find_map(|value| match value {
            Value::Object(map) => vault_not_text(map),
            Value::Array(items) => items.iter().find_map(|item| match item {
                Value::Object(map) => vault_not_text(map),
                _ => None,
            }),
            _ => None,
        }),
    }
}
