// SPDX-License-Identifier: GPL-3.0-or-later
//! The `constructed` inventory plugin of ansible-core 2.19.12: variables, groups and keyed groups
//! built, for each host an earlier source added, from Jinja expressions over its variables.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::auto::{failed, load_config};
use super::yaml::{Dialect, add_child_checked, falsy};
use super::{Inventory, InventoryPlugin, ParseCx, PluginError, Source};
use crate::playbook::python_repr;
use crate::template::Templar;

pub(super) struct Constructed;

impl InventoryPlugin for Constructed {
    fn name(&self) -> &'static str {
        "constructed"
    }

    fn verify(&self, source: &Source) -> bool {
        verify_config(source)
    }

    fn parse(
        &self,
        source: &Source,
        into: &mut Inventory,
        cx: &ParseCx,
    ) -> Result<(), PluginError> {
        let config = read_config(source, self.name())?;
        let options = Options::read(&config).map_err(failed)?;
        let path = source.text();
        let base = source
            .path()
            .parent()
            .map_or_else(PathBuf::new, Path::to_path_buf);
        Run {
            options,
            templar: cx.templar_for(base, &Value::Object(config)),
        }
        .hosts(into)
        .map_err(|f| f.into_error(path))
    }
}

/// `verify_file` of both plugins that read a config file: no extension, `.config` or YAML's.
pub(super) fn verify_config(source: &Source) -> bool {
    super::sources::file_with_extension(
        source,
        &[
            None,
            Some("config"),
            Some("yaml"),
            Some("yml"),
            Some("json"),
        ],
    )
}

/// `_read_config_data`: the file as a mapping that names this plugin.
pub(super) fn read_config(
    source: &Source,
    plugin: &str,
) -> Result<Map<String, Value>, PluginError> {
    let config = load_config(source)?;
    if falsy(&config) {
        return Err(failed(format!("{} is empty", source.text())));
    }
    let Value::Object(config) = config else {
        return Err(failed(format!(
            "inventory source has invalid structure, it should be a dictionary, got: {}",
            Dialect::Toml.type_name(&config)
        )));
    };
    let named = config.get("plugin").map(py_str);
    if named.as_deref() != Some(plugin)
        && named.as_deref() != Some(&format!("ansible.builtin.{plugin}"))
    {
        return Err(failed(format!(
            "Incorrect plugin name in file: {}",
            named.as_deref().unwrap_or("none found")
        )));
    }
    Ok(config)
}

/// Why a plugin stopped, worded the way the reference's exception type words it.
pub(super) enum Failure {
    /// An `AnsibleParserError` raised inside it: folded into the `Failed to parse` message.
    Parser(String),
    /// An `AnsibleError` raised inside it: the `Failed to parse` message, then this as its cause.
    Chained(String),
}

impl Failure {
    pub(super) fn into_error(self, path: &str) -> PluginError {
        match self {
            Self::Parser(message) => failed(format!("Failed to parse '{path}': {message}")),
            Self::Chained(cause) => PluginError {
                cause: Some((cause, None)),
                ..failed(format!("Failed to parse '{path}'."))
            },
        }
    }
}

struct Options {
    strict: bool,
    leading_separator: bool,
    compose: Map<String, Value>,
    groups: Map<String, Value>,
    keyed_groups: Vec<Value>,
}

impl Options {
    fn read(config: &Map<String, Value>) -> Result<Self, String> {
        let flag = |name: &str, default: bool| match config.get(name) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Bool(b)) => Ok(*b),
            Some(Value::String(s)) => match s.to_lowercase().as_str() {
                "y" | "yes" | "on" | "1" | "true" | "t" => Ok(true),
                "n" | "no" | "off" | "0" | "false" | "f" => Ok(false),
                _ => Err(format!(
                    "Invalid type for configuration option {name}: expected a boolean, got {}",
                    python_repr(&Value::String(s.clone()))
                )),
            },
            Some(other) => Err(format!(
                "Invalid type for configuration option {name}: expected a boolean, got {}",
                Dialect::Toml.type_name(other)
            )),
        };
        let map = |name: &str| match config.get(name) {
            None | Some(Value::Null) => Ok(Map::new()),
            Some(Value::Object(m)) => Ok(m.clone()),
            Some(other) => Err(format!(
                "Invalid type for configuration option {name}: expected a dict, got {}",
                Dialect::Toml.type_name(other)
            )),
        };
        let keyed_groups = match config.get("keyed_groups") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(list)) => list.clone(),
            Some(other) => {
                return Err(format!(
                    "Invalid type for configuration option keyed_groups: expected a list, got {}",
                    Dialect::Toml.type_name(other)
                ));
            }
        };
        if flag("use_vars_plugins", false)? {
            return Err("use_vars_plugins is not supported yet".to_string());
        }
        if flag("use_extra_vars", false)? {
            return Err("use_extra_vars is not supported yet".to_string());
        }
        Ok(Self {
            strict: flag("strict", false)?,
            leading_separator: flag("leading_separator", true)?,
            compose: map("compose")?,
            groups: map("groups")?,
            keyed_groups,
        })
    }
}

struct Run {
    options: Options,
    templar: Templar,
}

impl Run {
    fn hosts(&self, inv: &mut Inventory) -> Result<(), Failure> {
        let hosts: Vec<String> = inv.hosts.iter().map(|h| h.name.clone()).collect();
        for host in &hosts {
            let vars = host_vars(inv, host);
            for (name, expr) in &self.options.compose {
                match self.evaluate(expr, &vars) {
                    Ok(value) => set_var(inv, host, name, value, &vars),
                    Err(e) if self.options.strict => {
                        return Err(Failure::Chained(format!(
                            "Could not set {name} for host {host}: {e}"
                        )));
                    }
                    Err(_) => {}
                }
            }
            // Composed variables are visible to what follows; the groups they add are not.
            let vars = host_vars(inv, host);
            self.composed_groups(inv, host, &vars)?;
            for entry in &self.options.keyed_groups {
                self.keyed_group(inv, host, &vars, entry)?;
            }
        }
        Ok(())
    }

    fn evaluate(&self, expr: &Value, vars: &Map<String, Value>) -> Result<Value, String> {
        let Value::String(text) = expr else {
            return Err("expression must be a string".to_string());
        };
        self.templar
            .evaluate(text, vars)
            .map_err(|e| reason(&e.0, text, vars))
    }

    fn composed_groups(
        &self,
        inv: &mut Inventory,
        host: &str,
        vars: &Map<String, Value>,
    ) -> Result<(), Failure> {
        for (name, condition) in &self.options.groups {
            let group = safe_group_name(name);
            let hit = match condition {
                Value::String(text) => self
                    .templar
                    .condition(text, vars)
                    .map_err(|e| reason(&e.0, text, vars)),
                Value::Bool(b) => Ok(*b),
                _ => Err("conditional must be a string".to_string()),
            };
            match hit {
                Ok(true) => {
                    add_group(inv, &group)?;
                    inv.add_member(host, &group);
                }
                Err(e) if self.options.strict => {
                    return Err(Failure::Parser(format!(
                        "Could not add host {host} to group {group}: {e}"
                    )));
                }
                Ok(false) | Err(_) => {}
            }
        }
        Ok(())
    }

    #[expect(clippy::too_many_lines)]
    fn keyed_group(
        &self,
        inv: &mut Inventory,
        host: &str,
        vars: &Map<String, Value>,
        entry: &Value,
    ) -> Result<(), Failure> {
        let Value::Object(keyed) = entry else {
            return Err(invalid_entry(entry));
        };
        if keyed.is_empty() {
            return Err(invalid_entry(entry));
        }
        let raw_key = keyed.get("key").unwrap_or(&Value::Null);
        let key = match self.evaluate(raw_key, vars) {
            Ok(key) => key,
            Err(e) if self.options.strict => {
                return Err(Failure::Parser(format!(
                    "Could not generate group for host {host} from {} entry: {e}",
                    py_str(raw_key)
                )));
            }
            Err(_) => return Ok(()),
        };
        let default = keyed.get("default_value").filter(|v| !v.is_null());
        let trailing = keyed.get("trailing_separator").filter(|v| !v.is_null());
        if default.is_some() && trailing.is_some() {
            return Err(Failure::Parser(
                "parameters are mutually exclusive for keyed groups: default_value|trailing_separator"
                    .to_string(),
            ));
        }
        let use_default = is_blank(&key) && default.is_some();
        if falsy(&key) && !use_default {
            // An empty list or mapping is a valid construction that makes no group.
            let empty = matches!(&key, Value::Array(a) if a.is_empty())
                || matches!(&key, Value::Object(o) if o.is_empty());
            if self.options.strict && !empty {
                return Err(Failure::Parser(format!(
                    "No key or key resulted empty for {} in host {host}, invalid entry",
                    py_str(raw_key)
                )));
            }
            return Ok(());
        }
        let prefix = keyed.get("prefix").map_or_else(String::new, py_str);
        let mut sep = keyed
            .get("separator")
            .map_or_else(|| "_".to_string(), py_str);
        let parent = match keyed.get("parent_group") {
            Some(Value::String(text)) => match self.templar.render_one_pass(text, vars) {
                Ok(parent) => parent,
                Err(e) if self.options.strict => {
                    return Err(Failure::Parser(format!(
                        "Could not generate parent group {} for group {}: {}",
                        python_repr(&Value::String(text.clone())),
                        python_repr(&key),
                        reason(&e.0, text, vars)
                    )));
                }
                Err(_) => return Ok(()),
            },
            other => other.cloned().unwrap_or(Value::Null),
        };
        let names: Vec<String> = if use_default {
            vec![py_str(default.unwrap_or(&Value::Null))]
        } else {
            match &key {
                Value::String(text) => vec![text.clone()],
                Value::Array(items) => items
                    .iter()
                    .map(|item| match default {
                        Some(default) if is_blank(item) => py_str(default),
                        _ => py_str(item),
                    })
                    .collect(),
                Value::Object(map) => map
                    .iter()
                    .map(|(name, value)| {
                        if is_blank(value) {
                            if let Some(default) = default {
                                return format!("{name}{sep}{}", py_str(default));
                            }
                            if trailing == Some(&Value::Bool(false)) {
                                return name.clone();
                            }
                        }
                        format!("{name}{sep}{}", py_str(value))
                    })
                    .collect(),
                other => {
                    // A value read from the inventory carries the loader's tag; a computed one
                    // is a plain Python type.
                    let dialect = if is_original(other, vars) {
                        Dialect::Yaml
                    } else {
                        Dialect::Toml
                    };
                    return Err(Failure::Parser(format!(
                        "Invalid group name format, expected a string or a list of them or \
                         dictionary, got: {}",
                        dialect.type_name(other)
                    )));
                }
            }
        };
        for bare in names {
            if prefix.is_empty() && !self.options.leading_separator {
                sep.clear();
            }
            let group = safe_group_name(&format!("{prefix}{sep}{bare}"));
            add_group(inv, &group)?;
            inv.add_member(host, &group);
            if !falsy(&parent) {
                let parent = safe_group_name(&py_str(&parent));
                add_group(inv, &parent)?;
                add_child_checked(inv, &parent, &group).map_err(Failure::Chained)?;
            }
        }
        Ok(())
    }
}

fn invalid_entry(entry: &Value) -> Failure {
    Failure::Parser(format!(
        "Invalid keyed group entry, it must be a dictionary: {} ",
        python_repr(entry)
    ))
}

/// The variables a host's expressions see: its groups' and its own, and the magic ones.
fn host_vars(inv: &Inventory, host: &str) -> Map<String, Value> {
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
fn set_var(inv: &mut Inventory, host: &str, name: &str, value: Value, vars: &Map<String, Value>) {
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
fn is_original(value: &Value, vars: &Map<String, Value>) -> bool {
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
pub(super) fn has_invalid_chars(name: &str) -> bool {
    INVALID_CHARS.is_match(name)
}

/// `to_safe_group_name(name, force=True, silent=True)`.
fn safe_group_name(name: &str) -> String {
    INVALID_CHARS.replace_all(name, "_").into_owned()
}

/// Python's `str()` of a value.
pub(super) fn py_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => python_repr(other),
    }
}

fn is_blank(value: &Value) -> bool {
    value.is_null() || value.as_str() == Some("")
}

/// A template failure as the reference words it in a plugin's message: the reason alone. An
/// undefined read names the first name of the expression the host has no value for.
// The engine does not say which name was undefined; the first one not in `vars` is
// right for a plain expression, and a nested attribute keeps the engine's wording.
fn reason(error: &str, expr: &str, vars: &Map<String, Value>) -> String {
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

#[cfg(test)]
mod tests;
