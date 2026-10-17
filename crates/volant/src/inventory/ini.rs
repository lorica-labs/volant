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
            inv.group_mut(&group);
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
                    inv.add_source_host(&name, port, file);
                    inv.add_host(&name, vars.clone());
                    inv.add_member(&name, &group);
                }
            }
            "vars" => {
                let (key, value) = line
                    .split_once('=')
                    .ok_or_else(|| err(format!("Expected key=value, got: {line}")))?;
                inv.group_mut(&group)
                    .vars
                    .insert(key.trim().to_string(), literal(value.trim()));
            }
            _ => {
                let child = GROUP_NAME
                    .captures(line)
                    .ok_or_else(|| err(format!("Expected group name, got: {line}")))?;
                inv.add_child(&group, &child[1]);
            }
        }
    }
    Ok(())
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
    let mut words = shlex::split(line)
        .ok_or_else(|| format!("Error parsing host definition '{line}': No closing quotation"))?;
    if let Some(comment) = words.iter().position(|w| w.starts_with('#')) {
        words.truncate(comment);
    }
    Ok(words)
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
