// SPDX-License-Identifier: GPL-3.0-or-later
//! The `ini` inventory plugin.

use std::path::Path;
use std::sync::LazyLock;

use crate::tags::{TagTree, Tagged};
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
        cx: &ParseCx,
    ) -> Result<(), PluginError> {
        let failed = |message: String| PluginError {
            message: format!("Failed to parse inventory: {message}"),
            origin: Some(source.text().to_string()),
            cause: None,
        };
        let bytes = std::fs::read(source.path()).map_err(|e| failed(e.to_string()))?;
        let text = String::from_utf8(bytes).map_err(|e| failed(e.to_string()))?;
        parse_into(&text, into, Some(source.path()), Some(cx)).map_err(|e| {
            // Measured: the reference raises this one inside `except ValueError`, so it chains
            // the message as a second block under `Failed to parse inventory.`; every other
            // error of the file prints the merged one-block shape.
            if e.message.starts_with("Error parsing host definition ") {
                PluginError {
                    message: "Failed to parse inventory.".to_string(),
                    origin: Some(source.text().to_string()),
                    cause: Some((e.message, None)),
                }
            } else {
                failed(e.message)
            }
        })
    }
}

#[cfg(test)]
impl Inventory {
    /// An inventory read from INI text alone.
    pub fn parse_ini(text: &str) -> Result<Self, InventoryError> {
        let mut inv = Inventory::default();
        parse_into(text, &mut inv, None, None)?;
        Ok(inv)
    }
}

/// Reads INI text into `inv`, the way ansible-core 2.19.12's `ini` plugin reads a file: what was
/// added before an error stays, as the reference's does.
#[expect(clippy::too_many_lines)]
fn parse_into(
    text: &str,
    inv: &mut Inventory,
    file: Option<&Path>,
    cx: Option<&ParseCx>,
) -> Result<(), InventoryError> {
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
            group = safe_group_name(&m[1], cx);
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
                if let (Some(cx), Some(file)) = (cx, file) {
                    super::names::note_line(cx, file, idx + 1, vars.keys());
                }
                for name in names {
                    if name.is_empty() {
                        return Err(err("Invalid empty host name provided: ".to_string()));
                    }
                    inv.add_source_host(&name, port, line_tagged(&vars, file, idx + 1), file);
                    inv.add_member(&name, &group);
                }
            }
            "vars" => {
                let (key, value) = line
                    .split_once('=')
                    .ok_or_else(|| err(format!("Expected key=value, got: {line}")))?;
                let (key, value) = (key.trim(), literal(value.trim()));
                if let (Some(cx), Some(file)) = (cx, file) {
                    super::names::note_line(cx, file, idx + 1, [&key.to_string()]);
                }
                if key == "ansible_group_priority" {
                    if let Some(priority) = priority(&value).map_err(err)? {
                        inv.group_mut(&group).priority = Some(priority);
                    }
                } else {
                    let tree = line_tree(&value, file, idx + 1);
                    inv.group_mut(&group)
                        .vars
                        .combine_one(key, &value, tree.as_ref());
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
pub(super) fn priority(value: &Value) -> Result<Option<i64>, String> {
    #[expect(clippy::cast_possible_truncation, reason = "Python's int() truncates")]
    Ok(match value {
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => Some(ranges::py_int(s)?),
        _ => None,
    })
}

/// A host line's first word: the names it stands for and the port they share.
pub(super) fn expand_hostpattern(pattern: &str) -> Result<(Vec<String>, Option<i64>), String> {
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

fn key_values(words: &[String]) -> Result<serde_json::Map<String, Value>, String> {
    words
        .iter()
        .map(|w| {
            w.split_once('=')
                .map(|(k, v)| (k.to_string(), literal(v)))
                .ok_or_else(|| format!("Expected key=value host variable assignment, got: {w}"))
        })
        .collect()
}

/// What Ansible's INI plugin does with a value: `ast.literal_eval`, falling back to the text,
/// with the escapes of its template literals kept as written (see `yaml::escape_strings`).
/// See `pyliteral`; a text it refuses, `yes` or `010` included, stays a string.
/// The tags the reference gives a value of an INI line: its file and line, and trust for its
/// strings.
fn line_tree(value: &Value, file: Option<&Path>, line: usize) -> Option<TagTree> {
    let path = std::path::absolute(file?).ok()?;
    let line = u32::try_from(line).unwrap_or(0);
    let origin = crate::tags::origin_at(&path.display().to_string(), line, 0);
    crate::tags::uniform(value, origin, true)
}

fn line_tagged(vars: &serde_json::Map<String, Value>, file: Option<&Path>, line: usize) -> Tagged {
    let mut out = Tagged::default();
    for (key, value) in vars {
        out.insert(key.clone(), value.clone(), line_tree(value, file, line));
    }
    out
}

fn literal(text: &str) -> Value {
    let mut value = python_literal(text);
    crate::yaml::escape_strings(&mut value);
    value
}

fn python_literal(text: &str) -> Value {
    super::pyliteral::literal_eval(text.trim()).unwrap_or_else(|| Value::String(text.to_string()))
}

/// A group name as `to_safe_group_name` makes it under `TRANSFORM_INVALID_GROUP_CHARS`: with
/// `always` or `silently` each character outside `[A-Za-z0-9_]` becomes `_`, and with `never`, the
/// default, the name stays. `always` and `never` warn once, `ignore` and `silently` do not.
fn safe_group_name(name: &str, cx: Option<&ParseCx>) -> String {
    let invalid = |c: char| !(c.is_ascii_alphanumeric() || c == '_');
    if !name.chars().any(invalid) {
        return name.to_string();
    }
    let mode = std::env::var("ANSIBLE_TRANSFORM_INVALID_GROUP_CHARS")
        .unwrap_or_else(|_| "never".to_string());
    let warn = |text: &str| {
        if let Some(cx) = cx {
            cx.warn_once(format!("{text}, use -vvvv to see details"));
        }
    };
    match mode.as_str() {
        "always" | "silently" => {
            if mode == "always" {
                warn("Invalid characters were found in group names and automatically replaced");
            }
            name.chars()
                .map(|c| if invalid(c) { '_' } else { c })
                .collect()
        }
        "never" => {
            warn("Invalid characters were found in group names but not replaced");
            name.to_string()
        }
        _ => name.to_string(),
    }
}
