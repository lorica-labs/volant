// SPDX-License-Identifier: GPL-3.0-or-later
//! The deprecations a module returns: kept in its result as ansible-core registers them, and
//! shown as it shows them.

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;

/// Rewrites each deprecation a module sent in ansible-core 2.19's wire form (`DeprecationSummary`,
/// its message under `event`) into the mapping a registered result holds:
/// `_event_utils.deprecation_as_dict` and the deprecator beside it. Measured on ansible-core
/// 2.19.12: `{collection_name, deprecator: {resolved_name, type}, msg, version}`, `date` in place
/// of `version` when one is set. Anything else is left as it came.
pub(super) fn register_deprecations(result: &mut TaskResult) {
    let Some(Value::Array(list)) = result.0.get_mut("deprecations") else {
        return;
    };
    let any = !list.is_empty();
    for item in list.iter_mut() {
        if item.get("__ansible_type").and_then(Value::as_str) != Some("DeprecationSummary") {
            continue;
        }
        let resolved = item
            .pointer("/deprecator/resolved_name")
            .cloned()
            .unwrap_or(Value::Null);
        let kind = item
            .pointer("/deprecator/type")
            .cloned()
            .unwrap_or(Value::Null);
        let collection = resolved
            .as_str()
            .map(|name| name.split('.').take(2).collect::<Vec<_>>().join("."));
        let msg = super::register::warning_text(&json!({ "event": item.get("event") }));
        let mut shown = Map::new();
        shown.insert("msg".into(), json!(msg));
        shown.insert("collection_name".into(), json!(collection));
        match item.get("date").filter(|d| !d.is_null()) {
            Some(date) => shown.insert("date".into(), date.clone()),
            None => shown.insert(
                "version".into(),
                item.get("version").cloned().unwrap_or(Value::Null),
            ),
        };
        shown.insert(
            "deprecator".into(),
            json!({ "resolved_name": resolved, "type": kind }),
        );
        *item = Value::Object(shown);
    }
    if any {
        result.0.insert(SHOW_KEY.into(), Value::Bool(true));
    }
}

/// Marks a result whose deprecations a module sent through the agent, which nothing has shown
/// yet: the helper shows a plugin's itself.
const SHOW_KEY: &str = "_volant_show_deprecations";

/// [`deprecation_warnings`] for a result a module sent through the agent, taking the mark off.
pub(super) fn module_deprecation_warnings(result: &mut TaskResult) -> Vec<String> {
    if result.0.remove(SHOW_KEY).is_some() {
        deprecation_warnings(result)
    } else {
        Vec::new()
    }
}

/// What the coordinator prints for the registered deprecations of `result`, nothing when
/// deprecations are off: the line that says how to silence them the first time, then one
/// `[DEPRECATION WARNING]` line each, written as they come.
pub(in crate::executor) fn deprecation_warnings(result: &TaskResult) -> Vec<String> {
    let mut out = Vec::new();
    for line in deprecation_lines(result) {
        let Some(header) = crate::playbook::deprecation_header() else {
            break;
        };
        if let Some(header) = header {
            out.push(format!("{}{header}\n", crate::render::RAW_WARNING));
        }
        out.push(format!(
            "{}[DEPRECATION WARNING]: {line}\n",
            crate::render::RAW_WARNING
        ));
    }
    out
}

/// The line each registered deprecation of `result` is shown with, after `[DEPRECATION
/// WARNING]: `: ansible-core's `get_deprecation_message_with_plugin_info`.
fn deprecation_lines(result: &TaskResult) -> Vec<String> {
    let Some(Value::Array(list)) = result.0.get("deprecations") else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|item| {
            let msg = item.get("msg")?.as_str()?;
            let text = |key: &str| item.get(key).and_then(Value::as_str);
            let resolved = item
                .pointer("/deprecator/resolved_name")
                .and_then(Value::as_str);
            let kind = item.pointer("/deprecator/type").and_then(Value::as_str);
            Some(join_sentences(
                msg,
                &removal(resolved, kind, text("version"), text("date")),
            ))
        })
        .collect()
}

/// `This feature will be removed from ...`, as the reference builds it from the deprecator.
fn removal(
    resolved: Option<&str>,
    kind: Option<&str>,
    version: Option<&str>,
    date: Option<&str>,
) -> String {
    let typed = |kind: &str| {
        if kind == "module" {
            kind.to_string()
        } else {
            format!("{kind} plugin")
        }
    };
    let (collection, mut plugin) = match (resolved, kind) {
        (resolved, None) => (resolved.map(str::to_string), String::new()),
        (Some("ansible.builtin"), Some(kind)) => (
            Some("ansible.builtin".into()),
            format!("the {} API", typed(kind)),
        ),
        (resolved, Some(kind)) => {
            let parts: Vec<&str> = resolved.unwrap_or_default().split('.').collect();
            let name = parts.last().copied().unwrap_or_default();
            let collection = (parts.len() > 2).then(|| parts[..2].join("."));
            (collection, format!("{} '{name}'", typed(kind)))
        }
    };
    if collection.is_some() && !plugin.is_empty() {
        plugin.push_str(" in");
    }
    let collection_part = match collection.as_deref() {
        Some("ansible.builtin") => "ansible-core".to_string(),
        Some(name) => format!("collection '{name}'"),
        None => String::new(),
    };
    let when = match (collection.as_deref(), date, version) {
        (None, _, _) => "in the future".to_string(),
        (Some(_), Some(date), _) => format!("in a release after {date}"),
        (Some(_), None, Some(version)) => format!("version {version}"),
        (Some(_), None, None) => "in a future release".to_string(),
    };
    let from = if plugin.is_empty() && collection_part.is_empty() {
        ""
    } else {
        "from"
    };
    let words: Vec<&str> = [
        "This feature will be removed",
        from,
        &plugin,
        &collection_part,
        &when,
    ]
    .into_iter()
    .filter(|w| !w.is_empty())
    .collect();
    format!("{}.", words.join(" "))
}

/// `join_sentences`: each sentence ends with a stop, and the two are joined by a space.
fn join_sentences(first: &str, second: &str) -> String {
    let end = |text: &str| {
        let text = text.trim();
        if text.is_empty() || text.ends_with(['!', '?', '.']) {
            text.to_string()
        } else {
            format!("{text}.")
        }
    };
    let (first, second) = (end(first), end(second));
    match (first.is_empty(), second.is_empty()) {
        (false, true) => first,
        (true, _) => second,
        (false, false) => format!("{first} {second}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{join_sentences, removal};

    #[test]
    fn the_removal_sentence_names_who_removes_it_and_when() {
        assert_eq!(
            removal(Some("ansible.builtin"), None, Some("9.99"), None),
            "This feature will be removed from ansible-core version 9.99."
        );
        assert_eq!(
            removal(Some("ns.coll.mod"), Some("module"), Some("2.0.0"), None),
            "This feature will be removed from module 'mod' in collection 'ns.coll' version 2.0.0."
        );
        assert_eq!(
            removal(None, None, None, None),
            "This feature will be removed in the future."
        );
        assert_eq!(
            join_sentences("Alias 'old' is deprecated", "This feature will be removed."),
            "Alias 'old' is deprecated. This feature will be removed."
        );
    }
}
