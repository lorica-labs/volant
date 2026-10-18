// SPDX-License-Identifier: GPL-3.0-or-later
//! What the `constructed` plugin's expressions read and how it names what they make: the host's
//! variables, group names and the messages of a failed expression.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::Failure;
use crate::inventory::Inventory;
use crate::playbook::python_repr;

/// The variables a host's expressions see: its groups' and its own, and the magic ones.
pub(super) fn host_vars(inv: &Inventory, host: &str) -> Map<String, Value> {
    let mut vars: Map<String, Value> = inv.host_with_vars(host).vars.into_iter().collect();
    vars.insert("inventory_hostname".into(), host.into());
    vars.insert(
        "inventory_hostname_short".into(),
        host.split('.').next().unwrap_or(host).into(),
    );
    vars.insert("group_names".into(), inv.group_names_of(host).into());
    vars
}

/// Sets a composed variable. The strings of it that no other variable of the host holds were
/// computed, and are marked: a run treats the variable as data, `--list` wraps those strings.
// A computed string equal to an original one (`os | lower` on a lowercase value) stays
// plain here and is wrapped in the reference; tracking the tags would need a tagged value type.
pub(super) fn set_var(
    inv: &mut Inventory,
    host: &str,
    name: &str,
    value: Value,
    vars: &Map<String, Value>,
) {
    let mut originals = HashSet::new();
    for (key, v) in vars.iter().filter(|(k, _)| !MAGIC.contains(&k.as_str())) {
        if key != name {
            leaves(v, &mut originals);
        }
    }
    // An empty string is too often a literal to count as a copy, unless the whole value is one.
    originals.remove("");
    let mut all = HashSet::new();
    if !is_original(&value, vars) {
        leaves(&value, &mut all);
    }
    let computed: HashSet<String> = all.difference(&originals).cloned().collect();
    let key = (host.to_string(), name.to_string());
    if computed.is_empty() {
        inv.unsafe_vars.remove(&key);
    } else {
        inv.unsafe_vars.insert(key, computed);
    }
    if let Some(h) = inv.hosts.iter_mut().find(|h| h.name == host) {
        h.vars.insert(name.to_string(), value);
    }
}

const MAGIC: [&str; 3] = [
    "inventory_hostname",
    "inventory_hostname_short",
    "group_names",
];

/// Every string in a value, mapping keys left out.
fn leaves(value: &Value, out: &mut HashSet<String>) {
    match value {
        Value::String(s) => {
            out.insert(s.clone());
        }
        Value::Array(items) => items.iter().for_each(|v| leaves(v, out)),
        Value::Object(map) => map.values().for_each(|v| leaves(v, out)),
        _ => {}
    }
}

/// Whether a non-string value is a variable passed through, which the reference keeps tagged.
pub(super) fn is_original(value: &Value, vars: &Map<String, Value>) -> bool {
    vars.iter()
        .filter(|(k, _)| !MAGIC.contains(&k.as_str()))
        .any(|(_, v)| v == value)
}

/// `add_group`: the group exists afterwards, and a name with nothing in it is refused.
pub(super) fn add_group(inv: &mut Inventory, name: &str) -> Result<(), Failure> {
    if name.is_empty() {
        return Err(Failure::Chained(
            "Invalid empty/false group name provided: ".to_string(),
        ));
    }
    inv.group_mut(name);
    Ok(())
}

static INVALID_CHARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\d\W]|[^\w]").expect("a fixed pattern"));

/// Whether the reference would call a character of this group name invalid.
pub(in crate::inventory) fn has_invalid_chars(name: &str) -> bool {
    INVALID_CHARS.is_match(name)
}

/// `to_safe_group_name(name, force=True, silent=True)`.
pub(super) fn safe_group_name(name: &str) -> String {
    INVALID_CHARS.replace_all(name, "_").into_owned()
}

/// Python's `str()` of a value.
pub(in crate::inventory) fn py_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => python_repr(other),
    }
}

pub(super) fn is_blank(value: &Value) -> bool {
    value.is_null() || value.as_str() == Some("")
}

/// A template failure as the reference words it in a plugin's message: the reason alone. An
/// undefined read names the first name of the expression the host has no value for.
// The engine does not say which name was undefined; the first one not in `vars` is
// right for a plain expression, and a nested attribute keeps the engine's wording.
pub(super) fn reason(error: &str, expr: &str, vars: &Map<String, Value>) -> String {
    let reason = error
        .split_once("The error was: ")
        .map_or(error, |(_, rest)| rest);
    if !reason.starts_with("undefined value") {
        return reason.to_string();
    }
    static NAME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"'[^']*'|"[^"]*"|([.|]\s*|\bis\s+(?:not\s+)?)?\b([A-Za-z_]\w*)"#)
            .expect("a fixed pattern")
    });
    const WORDS: [&str; 10] = [
        "and", "or", "not", "in", "is", "if", "else", "true", "false", "none",
    ];
    NAME.captures_iter(expr)
        .filter(|c| c.get(1).is_none())
        .filter_map(|c| c.get(2).map(|m| m.as_str()))
        .find(|n| !vars.contains_key(*n) && !WORDS.contains(&n.to_lowercase().as_str()))
        .map_or_else(
            || reason.to_string(),
            |name| format!("'{name}' is undefined"),
        )
}
