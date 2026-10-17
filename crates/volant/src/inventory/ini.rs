// SPDX-License-Identifier: GPL-3.0-or-later
//! The `ini` inventory plugin.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::{Inventory, InventoryError, InventoryPlugin, ParseCx, PluginError, Source, ranges};

static SECTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[([^:\]\s]+)(?::(\w+))?\]\s*(?:#.*)?$").expect("a valid pattern")
});
static GROUP_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([^:\]\s]+)\s*(?:#.*)?$").expect("a valid pattern"));

/// The `ini` plugin: any readable file but a `.toml` one.
pub(super) struct Ini;

impl InventoryPlugin for Ini {
    fn name(&self) -> &'static str {
        "ini"
    }

    fn verify(&self, source: &Source) -> bool {
        source.readable_file() && source.path().extension().is_none_or(|e| e != "toml")
    }

    fn parse(
        &self,
        source: &Source,
        into: &mut Inventory,
        _cx: &ParseCx,
    ) -> Result<(), PluginError> {
        let failed = |message: String| PluginError {
            message: format!("Failed to parse inventory: {message}"),
            origin: Some(source.text().to_string()),
            unsupported: false,
        };
        let bytes = std::fs::read(source.path()).map_err(|e| failed(e.to_string()))?;
        let text = String::from_utf8(bytes).map_err(|e| failed(e.to_string()))?;
        parse_into(&text, into, Some(source.path())).map_err(|e| failed(e.message))
    }
}

impl Inventory {
    /// An inventory read from INI text alone.
    pub fn parse_ini(text: &str) -> Result<Self, InventoryError> {
        let mut inv = Inventory::default();
        parse_into(text, &mut inv, None)?;
        Ok(inv)
    }
}

/// Reads INI text into `inv`, the way ansible-core 2.19.12's `ini` plugin reads a file: what was
/// added before an error stays, as the reference's does.
fn parse_into(text: &str, inv: &mut Inventory, file: Option<&Path>) -> Result<(), InventoryError> {
    let mut group = "ungrouped".to_string();
    let mut state = "hosts".to_string();
    // The reference's `pending_declarations`: a group named by a `:vars` header or a
    // `:children` line before it is declared, in the order it was first named.
    let mut pending: Vec<Pending> = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let err = |message: String| InventoryError {
            line: idx + 1,
            message,
        };
        let line = raw.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(m) = SECTION.captures(line) {
            group = m[1].to_string();
            state = m.get(2).map_or("hosts", |k| k.as_str()).to_string();
            if !matches!(state.as_str(), "hosts" | "children" | "vars") {
                return Err(err(format!(
                    "Section [{group}:{state}] has unknown type: {state}"
                )));
            }
            if !declared(inv, &group) {
                if state == "vars" && !pending.iter().any(|p| p.name == group) {
                    pending.push(Pending {
                        name: group.clone(),
                        line: idx + 1,
                        vars: true,
                        parents: Vec::new(),
                    });
                }
                inv.group_mut(&group);
            }
            if state != "vars" {
                declare(inv, &group, &mut pending);
            }
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            return Err(err(format!(
                "Invalid section entry: '{line}'. Please make sure that there are no spaces in \
                 the section entry, and that there are no other invalid characters"
            )));
        }
        match state.as_str() {
            "hosts" => {
                let words = shlex_words(line).map_err(err)?;
                let Some((pattern, pairs)) = words.split_first() else {
                    continue;
                };
                let (names, port) = expand_hostpattern(pattern).map_err(err)?;
                let vars = key_values(pairs).map_err(err)?;
                for name in names {
                    if name.is_empty() {
                        return Err(err("Invalid empty host name provided: ".to_string()));
                    }
                    inv.add_source_host(&name, port, vars.clone(), file);
                    inv.add_member(&name, &group);
                }
            }
            "vars" => {
                let (key, value) = line
                    .split_once('=')
                    .ok_or_else(|| err(format!("Expected key=value, got: {line}")))?;
                let (key, value) = (key.trim(), literal(value.trim()));
                if key == "ansible_group_priority" {
                    if let Some(priority) = priority(&value).map_err(err)? {
                        inv.group_mut(&group).priority = Some(priority);
                    }
                } else {
                    inv.group_mut(&group).vars.insert(key.to_string(), value);
                }
            }
            _ => {
                let child = GROUP_NAME
                    .captures(line)
                    .ok_or_else(|| err(format!("Expected group name, got: {line}")))?;
                let child = &child[1];
                if declared(inv, child) {
                    inv.add_child(&group, child);
                } else if let Some(p) = pending.iter_mut().find(|p| p.name == child) {
                    p.parents.push(group.clone());
                } else {
                    pending.push(Pending {
                        name: child.to_string(),
                        line: idx + 1,
                        vars: false,
                        parents: vec![group.clone()],
                    });
                }
            }
        }
    }
    match pending.first() {
        None => Ok(()),
        Some(p) => Err(InventoryError {
            line: p.line,
            message: if p.vars {
                format!(
                    "Section [{0}:vars] not valid for undefined group '{0}'.",
                    p.name
                )
            } else {
                format!(
                    "Section [{}:children] includes undefined group '{}'.",
                    p.parents.last().map_or("", String::as_str),
                    p.name
                )
            },
        }),
    }
}

/// Whether the inventory has `group`: `all` and `ungrouped` are there from the start.
fn declared(inv: &Inventory, group: &str) -> bool {
    matches!(group, "all" | "ungrouped") || inv.group(group).is_some()
}

struct Pending {
    name: String,
    line: usize,
    /// Named by a `:vars` header, rather than by a `:children` line.
    vars: bool,
    parents: Vec<String>,
}

/// A group's declaration settles what naming it before asked for: the parents a `:children`
/// line gave it get it now, and so do theirs in turn (`_add_pending_children`).
fn declare(inv: &mut Inventory, group: &str, pending: &mut Vec<Pending>) {
    let Some(pos) = pending.iter().position(|p| p.name == group) else {
        return;
    };
    if pending[pos].vars {
        pending.remove(pos);
        return;
    }
    let parents = pending.remove(pos).parents;
    for parent in parents {
        inv.add_child(&parent, group);
        if pending.iter().any(|p| p.name == parent && !p.vars) {
            declare(inv, &parent, pending);
        }
    }
}

/// `Group.set_priority(int(value))`: a value `int()` cannot take as a type is ignored, one it
/// cannot read as a number fails the file.
fn priority(value: &Value) -> Result<Option<i64>, String> {
    #[expect(clippy::cast_possible_truncation, reason = "Python's int() truncates")]
    Ok(match value {
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => Some(ranges::py_int(s)?),
        _ => None,
    })
}

/// A host line's first word: the names it stands for and the port they share.
fn expand_hostpattern(pattern: &str) -> Result<(Vec<String>, Option<i64>), String> {
    let (host, port) =
        ranges::parse_address(pattern, true).unwrap_or_else(|_| (pattern.to_string(), None));
    let names = ranges::expand(&host)?;
    if pattern.trim().ends_with(':') && port.is_none() {
        return Err(format!(
            "Invalid host pattern '{pattern}' supplied, ending in ':' is not allowed, this \
             character is reserved to provide a port."
        ));
    }
    if names.iter().any(|n| n.trim() == "---") {
        return Err(format!(
            "Invalid host pattern '{pattern}' supplied, '---' is normally a sign this is a YAML \
             file."
        ));
    }
    Ok((names, port))
}

/// The words of a host line, split as `shlex.split(line, comments=True)` splits them: a word
/// that starts with `#` ends the line.
fn shlex_words(line: &str) -> Result<Vec<String>, String> {
    shlex::split(uncommented(line))
        .ok_or_else(|| format!("Error parsing host definition '{line}': No closing quotation"))
}

/// The line up to its first `#` outside quotes, mid-word included, as POSIX `shlex` reads it.
fn uncommented(line: &str) -> &str {
    let (mut quote, mut escaped) = (None, false);
    for (i, c) in line.char_indices() {
        match (quote, c) {
            _ if escaped => escaped = false,
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('"') | None, '\\') => escaped = true,
            (None, '\'' | '"') => quote = Some(c),
            (None, '#') => return &line[..i],
            _ => {}
        }
    }
    line
}

fn key_values(words: &[String]) -> Result<BTreeMap<String, Value>, String> {
    words
        .iter()
        .map(|w| {
            w.split_once('=')
                .map(|(k, v)| (k.to_string(), literal(v)))
                .ok_or_else(|| format!("Expected key=value host variable assignment, got: {w}"))
        })
        .collect()
}

/// An integer the way Python's `ast.literal_eval` accepts one: optional sign, `_` allowed only
/// between digits (never leading, trailing, or doubled), and no leading zero unless the whole
/// literal is `0`. Measured against the reference: `leading=010` in an INI stays the string
/// `"010"` (Python's grammar rejects a leading zero as a syntax error, so `literal_eval` raises
/// and the ini plugin falls back to the raw text), while `grouped=1_000` becomes the integer
/// `1000` (`_` is a valid digit separator in Python's integer literals since 3.6).
///
/// Deliberate ceiling: Python's integers are unbounded, ours are not. A literal outside
/// `i64::MIN..=i64::MAX` (`clean.parse` failing) falls back to a string here, same as any other
/// value `ast.literal_eval` would refuse to call an int for — no inventory host var plausibly
/// needs more than 63 bits, so this stays a documented ceiling rather than a `serde_json`
/// arbitrary-precision dependency for a case that never comes up in practice.
fn python_int(t: &str) -> Option<i64> {
    let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.chars().all(|c| c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    let clean: String = digits.chars().filter(|c| *c != '_').collect();
    if clean.len() > 1 && clean.starts_with('0') {
        return None;
    }
    let value: i64 = clean.parse().ok()?;
    Some(if t.starts_with('-') { -value } else { value })
}

/// What Ansible's INI plugin does with a value: `ast.literal_eval`, falling back to the text.
/// Integers, floats, `True`/`False`/`None`, quoted strings, and JSON-shaped lists and dicts
/// are recognised; everything else, `yes` included, stays a string.
fn literal(text: &str) -> Value {
    let t = text.trim();
    if let Some(i) = python_int(t) {
        return Value::from(i);
    }
    if t.contains('.')
        && let Ok(f) = t.parse::<f64>()
        && let Some(n) = serde_json::Number::from_f64(f)
    {
        return Value::Number(n);
    }
    match t {
        "True" => return Value::Bool(true),
        "False" => return Value::Bool(false),
        "None" => return Value::Null,
        _ => {}
    }
    if t.len() >= 2
        && ((t.starts_with('\'') && t.ends_with('\'')) || (t.starts_with('"') && t.ends_with('"')))
    {
        return Value::String(t[1..t.len() - 1].to_string());
    }
    if ((t.starts_with('[') && t.ends_with(']')) || (t.starts_with('{') && t.ends_with('}')))
        && let Ok(v) = serde_json::from_str::<Value>(t)
    {
        return v;
    }
    Value::String(text.to_string())
}
