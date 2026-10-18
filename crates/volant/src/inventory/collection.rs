// SPDX-License-Identifier: GPL-3.0-or-later
//! An inventory plugin of a collection, named by an `auto` file: ansible-core runs it in the
//! Python helper, which starts only then, and its answer is read here.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{Inventory, ParseCx, PluginError, Source, ini};
use crate::yaml;

fn failure(message: String) -> PluginError {
    PluginError {
        message,
        origin: None,
        cause: None,
    }
}

fn bad() -> PluginError {
    failure("the Python helper answered the inventory in a shape it should not".into())
}

fn list(value: Option<&Value>) -> Result<&Vec<Value>, PluginError> {
    value.and_then(Value::as_array).ok_or_else(bad)
}

/// A value as the plugin made it: a string it did not trust as a template arrives as
/// `{"__ansible_unsafe": text}` and stays data.
fn untrusted(value: &Value) -> Value {
    match value {
        Value::Object(map) if map.len() == 1 => match map.get("__ansible_unsafe") {
            Some(Value::String(text)) => Value::String(yaml::unsafe_string(text)),
            _ => Value::Object(map.iter().map(|(k, v)| (k.clone(), untrusted(v))).collect()),
        },
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), untrusted(v))).collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(untrusted).collect()),
        other => other.clone(),
    }
}

/// The helper's answer into the inventory: the warnings the plugin said, then, unless it
/// failed, its groups and hosts. Every value is data from a plugin and is never templated.
pub(super) fn read(
    answer: &Value,
    source: &Source,
    into: &mut Inventory,
    cx: &ParseCx,
) -> Result<(), PluginError> {
    for line in list(answer.get("verbose"))? {
        cx.verbose(1, line.as_str().ok_or_else(bad)?.to_string());
    }
    for warning in list(answer.get("warnings"))? {
        cx.warn(warning.as_str().ok_or_else(bad)?.to_string());
    }
    if let Some(error) = answer.get("error") {
        return Err(failure(error.as_str().ok_or_else(bad)?.to_string()));
    }
    let origin = Some(source.path());
    let vars_of = |value: &Value| -> Result<BTreeMap<String, Value>, PluginError> {
        let vars = value
            .get("vars")
            .and_then(Value::as_object)
            .ok_or_else(bad)?;
        Ok(vars
            .iter()
            .map(|(k, v)| (k.clone(), untrusted(v)))
            .collect())
    };
    let named = |value: &Value| {
        value
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    for host in list(answer.get("hosts"))? {
        let host_name = named(host).ok_or_else(bad)?;
        into.add_source_host(&host_name, None, vars_of(host)?, origin);
    }
    for group in list(answer.get("groups"))? {
        let group_name = named(group).ok_or_else(bad)?;
        let members = |key| -> Result<Vec<String>, PluginError> {
            Ok(list(group.get(key))?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect())
        };
        into.group_mut(&group_name);
        for (key, value) in vars_of(group)? {
            if key == "ansible_group_priority" {
                if let Some(p) = ini::priority(&value).map_err(failure)? {
                    into.group_mut(&group_name).priority = Some(p);
                }
            } else {
                into.group_mut(&group_name).vars.insert(key, value);
            }
        }
        if group_name == "all" || group_name == "ungrouped" {
            continue;
        }
        for host in members("hosts")? {
            into.add_member(&host, &group_name);
        }
        for child in members("children")? {
            into.add_child(&group_name, &child);
        }
    }
    Ok(())
}
