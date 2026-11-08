// SPDX-License-Identifier: GPL-3.0-or-later
//! The deprecations a module returns: kept in its result as ansible-core registers them, and
//! shown as it shows them.

use std::sync::Arc;

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;

use crate::tags::{Children, Deprecated, TagSet, TagSetId, TagTree};

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
        let msg = super::register::warning_text(&json!({ "event": item.get("event") }));
        let date = item.get("date").filter(|d| !d.is_null()).cloned();
        let version = item.get("version").cloned();
        *item = registered(msg, resolved, kind, date, version);
    }
    if any {
        result.0.insert(SHOW_KEY.into(), Value::Bool(true));
    }
}

/// A deprecation as a registered result holds it.
fn registered(
    msg: String,
    resolved: Value,
    kind: Value,
    date: Option<Value>,
    version: Option<Value>,
) -> Value {
    let collection = resolved
        .as_str()
        .map(|name| name.split('.').take(2).collect::<Vec<_>>().join("."));
    let mut shown = Map::new();
    shown.insert("msg".into(), json!(msg));
    shown.insert("collection_name".into(), json!(collection));
    match date {
        Some(date) => shown.insert("date".into(), date),
        None => shown.insert("version".into(), version.unwrap_or(Value::Null)),
    };
    shown.insert(
        "deprecator".into(),
        json!({ "resolved_name": resolved, "type": kind }),
    );
    Value::Object(shown)
}

/// The key a result holds the tags of its values under, as JSON: a `_volant_` key, which
/// nothing shows.
const VALUE_TAGS_KEY: &str = "_volant_value_tags";

/// Takes the tags ansible-core 2.19's module wire form writes on a value off the values of
/// `result`, leaving the plain value, and keeps the `Deprecated` ones for what the task
/// registers. Measured on ansible-core 2.19.12, a module returning `deprecate_value(...)` sends
/// `{"value": ..., "tags": [{"msg": ..., "version": ..., "deprecator": {...}, "__ansible_type":
/// "Deprecated"}], "__ansible_type": "_AnsibleTaggedStr"}`.
pub(super) fn keep_value_tags(result: &mut TaskResult) {
    if !result.0.values().any(holds_tagged) {
        return;
    }
    let kids = result
        .0
        .iter_mut()
        .filter_map(|(key, value)| Some((key.clone(), untag(value)?)))
        .collect();
    if let Some(tree) = TagTree::node(TagSetId::NONE, Children::Map(kids)) {
        result.0.insert(VALUE_TAGS_KEY.into(), json!(tree));
    }
}

/// The tags [`keep_value_tags`] kept, by the key of the result they are under.
pub(super) fn value_tags(result: &TaskResult) -> Vec<(String, TagTree)> {
    let tree = result
        .0
        .get(VALUE_TAGS_KEY)
        .and_then(|kept| serde_json::from_value::<TagTree>(kept.clone()).ok())
        .filter(TagTree::is_known);
    match tree {
        Some(TagTree::Node(_, Children::Map(kids))) => kids,
        _ => Vec::new(),
    }
}

fn tagged_wire(map: &Map<String, Value>) -> bool {
    map.contains_key("value")
        && map
            .get("__ansible_type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.starts_with("_AnsibleTagged"))
}

fn holds_tagged(value: &Value) -> bool {
    match value {
        Value::Object(map) => tagged_wire(map) || map.values().any(holds_tagged),
        Value::Array(items) => items.iter().any(holds_tagged),
        _ => false,
    }
}

/// `value` with the wire form of its tags taken off, and the tree of the `Deprecated` ones.
fn untag(value: &mut Value) -> Option<TagTree> {
    let unwrapped = match value {
        Value::Object(map) if tagged_wire(map) => {
            let found = map
                .get("tags")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find_map(deprecated);
            let own = found.map_or(TagSetId::NONE, |d| {
                crate::tags::intern(TagSet {
                    deprecated: Some(Arc::new(d)),
                    ..TagSet::default()
                })
            });
            Some((own, map.remove("value").unwrap_or(Value::Null)))
        }
        _ => None,
    };
    let mut own = TagSetId::NONE;
    if let Some((id, inner)) = unwrapped {
        own = id;
        *value = inner;
    }
    let children = match value {
        Value::Object(map) => Children::Map(
            map.iter_mut()
                .filter_map(|(key, v)| Some((key.clone(), untag(v)?)))
                .collect(),
        ),
        Value::Array(items) => Children::Seq(
            items
                .iter_mut()
                .enumerate()
                .filter_map(|(i, v)| Some((i, untag(v)?)))
                .collect(),
        ),
        _ => Children::Seq(Vec::new()),
    };
    TagTree::node(own, children)
}

/// A `Deprecated` tag of the wire form.
fn deprecated(tag: &Value) -> Option<Deprecated> {
    if tag.get("__ansible_type")?.as_str()? != "Deprecated" {
        return None;
    }
    let text = |key: &str| tag.get(key).and_then(Value::as_str).map(str::to_string);
    let deprecator = tag
        .pointer("/deprecator/resolved_name")
        .and_then(Value::as_str)
        .map(|name| {
            let kind = tag.pointer("/deprecator/type").and_then(Value::as_str);
            (name.to_string(), kind.unwrap_or_default().to_string())
        });
    Some(Deprecated {
        msg: text("msg")?,
        help_text: text("help_text"),
        date: text("date"),
        version: text("version"),
        deprecator,
    })
}

/// The result `run` gives, with the deprecations its templates raised, as ansible-core registers
/// them: those already printed, then the deprecated values they read, which are shown with the
/// result.
pub(super) fn noting_reads(run: impl FnOnce() -> TaskResult) -> TaskResult {
    crate::template::take_deprecated_reads();
    crate::playbook::take_shown_deprecations();
    let mut result = run();
    add_read_deprecations(&mut result);
    result
}

fn add_read_deprecations(result: &mut TaskResult) {
    let printed = crate::playbook::take_shown_deprecations();
    let read = crate::template::take_deprecated_reads();
    if read.is_empty() && printed.is_empty() {
        return;
    }
    let shown = read.iter().map(|d| {
        let (resolved, kind) = match &d.deprecator {
            Some((name, kind)) => (json!(name), (!kind.is_empty()).then(|| json!(kind))),
            None => (Value::Null, None),
        };
        registered(
            d.msg.clone(),
            resolved,
            kind.unwrap_or(Value::Null),
            d.date.clone().map(Value::String),
            d.version.clone().map(Value::String),
        )
    });
    let list = result
        .0
        .entry("deprecations")
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Value::Array(list) = list {
        list.extend(printed);
        list.extend(shown);
    }
    if !read.is_empty() {
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
    use serde_json::json;
    use volant_protocol::TaskResult;

    use super::{join_sentences, keep_value_tags, removal, value_tags};

    #[test]
    fn a_tagged_module_value_comes_out_plain_and_keeps_its_deprecation() {
        let wire = json!({
            "deprecated_result": {
                "value": "v",
                "tags": [{
                    "msg": "value is deprecated",
                    "version": "9999.9",
                    "deprecator": {"resolved_name": "foo.bar", "type": null, "__ansible_type": "PluginInfo"},
                    "__ansible_type": "Deprecated",
                }],
                "__ansible_type": "_AnsibleTaggedStr",
            },
            "plain": "x",
        });
        let mut r = TaskResult(serde_json::from_value(wire).unwrap());
        keep_value_tags(&mut r);
        assert_eq!(r.0["deprecated_result"], json!("v"));
        let kept = value_tags(&r);
        assert_eq!(kept.len(), 1);
        let found = crate::tags::get(kept[0].1.own()).deprecated.unwrap();
        assert_eq!(found.msg, "value is deprecated");
        assert_eq!(found.version.as_deref(), Some("9999.9"));
        assert_eq!(found.deprecator, Some(("foo.bar".into(), String::new())));
    }

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
