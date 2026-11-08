// SPDX-License-Identifier: GPL-3.0-or-later
//! Text from a host, marked the way `ansible-inventory --list` prints it.

use std::collections::HashSet;

use serde_json::Value;

use crate::tags::TagTree;

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

/// A value as `ansible-inventory` prints it: a `!vault` value as `{"__ansible_vault": text}`, and
/// under `--list` a string whose tags do not trust it as `{"__ansible_unsafe": text}`.
pub(super) fn show_unsafe(value: &Value, tree: Option<&TagTree>, listing: bool) -> Value {
    match value {
        Value::String(text) => {
            if let Some(cipher) = crate::yaml::vault_text(text) {
                serde_json::json!({ "__ansible_vault": cipher })
            } else if listing && tree.is_some_and(|t| !crate::tags::is_trusted(t.own())) {
                serde_json::json!({ "__ansible_unsafe": text })
            } else {
                value.clone()
            }
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .enumerate()
                .map(|(i, v)| show_unsafe(v, tree.and_then(|t| t.index(i)), listing))
                .collect(),
        ),
        Value::Object(map) if map.len() == 1 && map.contains_key("__ansible_unsafe") => {
            value.clone()
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        show_unsafe(v, tree.and_then(|t| t.at(k)), listing),
                    )
                })
                .collect(),
        ),
        other => other.clone(),
    }
}
