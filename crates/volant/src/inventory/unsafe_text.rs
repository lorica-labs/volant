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

/// A value as `ansible-inventory` prints it: a `!vault` value as `{"__ansible_vault": text}`,
/// and text that is data, never rendered, as `{"__ansible_unsafe": text}` under `--list` and as
/// the text itself under `--host`.
pub(super) fn show_unsafe(value: &Value, listing: bool) -> Value {
    match value {
        Value::String(text) => {
            if let Some(cipher) = crate::yaml::vault_text(text) {
                serde_json::json!({ "__ansible_vault": cipher })
            } else if let Some(plain) = crate::yaml::unsafe_text(text) {
                if listing {
                    serde_json::json!({ "__ansible_unsafe": plain })
                } else {
                    Value::String(plain)
                }
            } else {
                value.clone()
            }
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| show_unsafe(v, listing)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), show_unsafe(v, listing)))
                .collect(),
        ),
        other => other.clone(),
    }
}
