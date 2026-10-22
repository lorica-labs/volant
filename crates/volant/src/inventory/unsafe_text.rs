// SPDX-License-Identifier: GPL-3.0-or-later
//! Text from a host, marked the way `ansible-inventory --list` prints it.

use std::collections::HashSet;

use serde_json::Value;

/// A computed value as `--list` prints it: every string in it wrapped, mapping keys left alone.
pub(super) fn mark_unsafe(value: &mut Value, computed: &HashSet<String>) {
    match value {
        Value::String(text) if computed.contains(text.as_str()) => {
            *value = serde_json::json!({ "__ansible_unsafe": text });
        }
        Value::Array(items) => items.iter_mut().for_each(|v| mark_unsafe(v, computed)),
        Value::Object(map) => map.values_mut().for_each(|v| mark_unsafe(v, computed)),
        _ => {}
    }
}

/// A value as `ansible-inventory --list` prints it: text that is data, never rendered, as
/// `{"__ansible_unsafe": text}`.
pub(super) fn show_unsafe(value: &Value) -> Value {
    match value {
        Value::String(text) => crate::yaml::unsafe_text(text).map_or_else(
            || value.clone(),
            |text| serde_json::json!({ "__ansible_unsafe": text }),
        ),
        Value::Array(items) => Value::Array(items.iter().map(show_unsafe).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), show_unsafe(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}
