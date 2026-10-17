// SPDX-License-Identifier: GPL-3.0-or-later
//! The `yaml` inventory plugin, and the group reader the `toml` plugin shares with it: both walk
//! `{group: {vars, children, hosts}}` the way ansible-core 2.19.12's plugins do.

use std::collections::BTreeMap;
use std::path::Path;

use saphyr::{Scalar, Yaml};
use serde_json::{Map, Value};

use super::ini::{expand_hostpattern, priority};
use super::{Inventory, InventoryPlugin, ParseCx, PluginError, Source};
use crate::yaml;

pub(super) struct YamlInventory;

impl InventoryPlugin for YamlInventory {
    fn name(&self) -> &'static str {
        "yaml"
    }

    fn verify(&self, source: &Source) -> bool {
        super::sources::file_with_extension(
            source,
            &[None, Some("yaml"), Some("yml"), Some("json")],
        )
    }

    fn parse(
        &self,
        source: &Source,
        into: &mut Inventory,
        cx: &ParseCx,
    ) -> Result<(), PluginError> {
        let failed = |message: String| PluginError {
            message,
            origin: None,
            cause: None,
            unsupported: false,
        };
        let bytes = std::fs::read(source.path()).map_err(|e| failed(e.to_string()))?;
        let text = String::from_utf8(bytes).map_err(|e| failed(e.to_string()))?;
        let docs = yaml::load(&text, source.text()).map_err(|e| failed(format!("{e:#}")))?;
        let null = Yaml::Value(Scalar::Null);
        let root = match docs.as_slice() {
            [] => &null,
            [one] => one,
            _ => {
                return Err(failed(
                    "YAML parsing failed: Expected a single document in the stream but found \
                     another document."
                        .to_string(),
                ));
            }
        };
        let groups = lower(root).map_err(failed)?;
        read_groups(&groups, Dialect::Yaml, into, Some(source.path()), cx).map_err(failed)
    }
}

/// What a data file is checked against and how the reference words it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Dialect {
    Yaml,
    Toml,
}

impl Dialect {
    /// Python's name for the type of `value` as the reference prints it.
    fn type_name(self, value: &Value) -> String {
        let (plain, tagged) = match value {
            Value::Null => return "<class 'NoneType'>".to_string(),
            Value::Bool(_) => return "<class 'bool'>".to_string(),
            Value::Number(n) if n.is_f64() => ("float", "Float"),
            Value::Number(_) => ("int", "Int"),
            Value::String(_) => ("str", "Str"),
            Value::Array(_) => ("list", "List"),
            Value::Object(_) => ("dict", "Dict"),
        };
        match self {
            Self::Toml => format!("<class '{plain}'>"),
            Self::Yaml => {
                format!("<class 'ansible.module_utils._internal._datatag._AnsibleTagged{tagged}'>")
            }
        }
    }
}

fn scalar_text(node: &Yaml) -> String {
    match node {
        Yaml::Value(Scalar::Integer(i)) => i.to_string(),
        Yaml::Value(Scalar::FloatingPoint(f)) => f.to_string(),
        Yaml::Value(Scalar::Boolean(b)) => if *b { "True" } else { "False" }.to_string(),
        Yaml::Value(Scalar::Null) => "None".to_string(),
        other => other.as_str().unwrap_or_default().to_string(),
    }
}

/// The top-level mapping as JSON, in file order. The structural levels are read here so a host
/// pattern that is no string (`456:`) is refused the way the reference refuses it.
fn lower(root: &Yaml) -> Result<Value, String> {
    let empty = match root {
        Yaml::Value(Scalar::Null) => true,
        Yaml::Value(Scalar::Boolean(b)) => !b,
        Yaml::Value(Scalar::Integer(i)) => *i == 0,
        Yaml::Value(Scalar::String(s)) => s.is_empty(),
        Yaml::Mapping(m) => m.is_empty(),
        Yaml::Sequence(s) => s.is_empty(),
        _ => false,
    };
    if empty {
        return Err("Parsed empty YAML file".to_string());
    }
    let Yaml::Mapping(_) = root else {
        let value = yaml::to_json(root).map_err(|e| e.to_string())?;
        return Err(format!(
            "YAML inventory has invalid structure, it should be a dictionary, got: {}",
            Dialect::Yaml.type_name(&value)
        ));
    };
    if let Some(plugin) = yaml::field(root, "plugin") {
        let value = yaml::to_json(plugin).map_err(|e| e.to_string())?;
        if !falsy(&value) {
            return Err("Plugin configuration YAML file, not YAML inventory".to_string());
        }
    }
    groups_value(root)
}

fn groups_value(node: &Yaml) -> Result<Value, String> {
    let mut out = Map::new();
    if let Yaml::Mapping(map) = node {
        for (name, data) in map {
            out.insert(group_name(name)?, group_value(data)?);
        }
    }
    Ok(Value::Object(out))
}

fn group_name(key: &Yaml) -> Result<String, String> {
    let class = match key {
        Yaml::Value(Scalar::Integer(_)) => "int",
        Yaml::Value(Scalar::FloatingPoint(_)) => "float",
        Yaml::Value(Scalar::Boolean(_)) => "bool",
        Yaml::Value(Scalar::Null) => "NoneType",
        _ => return Ok(scalar_text(key)),
    };
    let text = scalar_text(key);
    Err(bad_group(&text, &format!("<class '{class}'>")))
}

fn group_value(node: &Yaml) -> Result<Value, String> {
    let Yaml::Mapping(map) = node else {
        return yaml::to_json(node).map_err(|e| e.to_string());
    };
    let mut out = Map::new();
    for (key, data) in map {
        let key = scalar_text(key);
        let value = match (key.as_str(), data) {
            ("children", Yaml::Mapping(_)) => groups_value(data)?,
            ("hosts", Yaml::Mapping(hosts)) => {
                let mut patterns = Map::new();
                for (pattern, vars) in hosts {
                    if pattern.as_str().is_none() {
                        return Err(format!(
                            "Host pattern {} must be a string. Enclose integers/floats in \
                             quotation marks.",
                            scalar_text(pattern)
                        ));
                    }
                    patterns.insert(
                        scalar_text(pattern),
                        yaml::to_json(vars).map_err(|e| e.to_string())?,
                    );
                }
                Value::Object(patterns)
            }
            _ => yaml::to_json(data).map_err(|e| e.to_string())?,
        };
        out.insert(key, value);
    }
    Ok(Value::Object(out))
}

/// What `add_group` says of a group name that is no string.
fn bad_group(text: &str, class: &str) -> String {
    format!(
        "Unable to add group {text}: Invalid group name supplied, expected a string but got {class} for {text}"
    )
}

/// Python truthiness of a loaded value.
pub(super) fn falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

/// Python's `str()` of a value, near enough for the one message that prints it.
fn show(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Reads `{group: data}` into `inv`. What was added before an error stays.
pub(super) fn read_groups(
    groups: &Value,
    dialect: Dialect,
    inv: &mut Inventory,
    file: Option<&Path>,
    cx: &ParseCx,
) -> Result<(), String> {
    let mut reader = Reader {
        inv,
        dialect,
        file,
        cx,
    };
    if let Value::Object(map) = groups {
        for (name, data) in map {
            reader.group(name, data)?;
        }
    }
    Ok(())
}

struct Reader<'a, 'c> {
    inv: &'a mut Inventory,
    dialect: Dialect,
    file: Option<&'a Path>,
    cx: &'a ParseCx<'c>,
}

impl Reader<'_, '_> {
    /// `_parse_group`: whether the group was read, `false` when it is no group definition.
    fn group(&mut self, name: &str, data: &Value) -> Result<bool, String> {
        if !matches!(data, Value::Null | Value::Object(_)) {
            self.cx.warn(format!(
                "Skipping '{name}' as this is not a valid group definition"
            ));
            return Ok(false);
        }
        self.add_group(name);
        let Value::Object(map) = data else {
            return Ok(true);
        };
        let map = &self.sections(name, map)?;
        let (word, list) = match self.dialect {
            Dialect::Yaml => ("dictionary", "dictionary"),
            Dialect::Toml => ("dict", "list"),
        };
        for (key, data) in map {
            let invalid = |this: &Self, entry: &str, wanted: &str| {
                format!(
                    "Invalid \"{entry}\" entry for \"{name}\" group, requires a {wanted}, found \
                     \"{}\" instead.",
                    this.dialect.type_name(data)
                )
            };
            if self.dialect == Dialect::Yaml {
                if !matches!(data, Value::Null | Value::Object(_)) {
                    self.cx.warn(format!(
                        "Skipping key ({key}) in group ({name}) as it is not a mapping, it is a {}",
                        self.dialect.type_name(data)
                    ));
                    continue;
                }
                if data.is_null() {
                    self.cx
                        .verbose(3, format!("Skipping empty key ({key}) in group ({name})"));
                    continue;
                }
            }
            match (key.as_str(), data) {
                ("vars", Value::Object(vars)) => {
                    for (var, value) in vars {
                        self.group_var(name, var, value)?;
                    }
                }
                ("vars", _) => return Err(invalid(self, "vars", word)),
                ("children", Value::Object(children)) if self.dialect == Dialect::Yaml => {
                    for (child, data) in children {
                        if !self.group(child, data)? {
                            return Err(format!("{child} is not a known host nor group"));
                        }
                        self.child(name, child)?;
                    }
                }
                ("children", Value::Array(children)) if self.dialect == Dialect::Toml => {
                    for child in children {
                        let Value::String(child) = child else {
                            return Err(bad_group(&show(child), &self.dialect.type_name(child)));
                        };
                        self.group(child, &Value::Object(Map::new()))?;
                        self.child(name, child)?;
                    }
                }
                ("children", _) => return Err(invalid(self, "children", list)),
                ("hosts", Value::Object(hosts)) => {
                    for (pattern, vars) in hosts {
                        self.hosts(name, pattern, vars)?;
                    }
                }
                ("hosts", _) => return Err(invalid(self, "hosts", word)),
                _ if self.dialect == Dialect::Toml => self.cx.warn(format!(
                    "Skipping unexpected key \"{key}\" in group \"{name}\", only \"vars\", \
                     \"children\" and \"hosts\" are valid"
                )),
                _ => self.cx.warn(format!(
                    "Skipping unexpected key ({key}) in group ({name}), only \"vars\", \
                     \"children\" and \"hosts\" are valid"
                )),
            }
        }
        Ok(true)
    }

    /// For the yaml plugin, a string section is `{string: None}` and every section must be a
    /// mapping or empty, all checked before any is read.
    fn sections(&self, name: &str, map: &Map<String, Value>) -> Result<Map<String, Value>, String> {
        let mut map = map.clone();
        if self.dialect == Dialect::Toml {
            return Ok(map);
        }
        for section in ["vars", "children", "hosts"] {
            if let Some(Value::String(s)) = map.get(section) {
                let single = Map::from_iter([(s.clone(), Value::Null)]);
                map.insert(section.to_string(), Value::Object(single));
            }
            if let Some(v) = map.get(section)
                && !matches!(v, Value::Null | Value::Object(_))
            {
                return Err(format!(
                    "Invalid \"{section}\" entry for \"{name}\" group, requires a dictionary, found \"{}\" instead.",
                    self.dialect.type_name(v)
                ));
            }
        }
        Ok(map)
    }

    /// `add_group`, which warns once about a name with characters outside `[A-Za-z0-9_]`.
    fn add_group(&mut self, name: &str) {
        if name
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || c == '_'))
        {
            self.cx.warn_once(
                "Invalid characters were found in group names but not replaced, use -vvvv to \
                 see details"
                    .to_string(),
            );
        }
        self.inv.group_mut(name);
    }

    fn group_var(&mut self, group: &str, var: &str, value: &Value) -> Result<(), String> {
        if var == "ansible_group_priority" {
            if let Some(priority) = priority(value)? {
                self.inv.group_mut(group).priority = Some(priority);
            }
        } else {
            self.inv
                .group_mut(group)
                .vars
                .insert(var.to_string(), value.clone());
        }
        Ok(())
    }

    /// `add_child`, which refuses a group that is its own ancestor.
    fn child(&mut self, parent: &str, child: &str) -> Result<(), String> {
        if parent == child {
            return Err("can't add group to itself".to_string());
        }
        let mut seen = vec![parent.to_string()];
        let mut at = 0;
        while let Some(current) = seen.get(at).cloned() {
            at += 1;
            for g in &self.inv.groups {
                if g.children.contains(&current) && !seen.contains(&g.name) {
                    seen.push(g.name.clone());
                }
            }
        }
        if seen.iter().any(|g| g == child) {
            return Err(format!(
                "Adding group '{child}' as child to '{parent}' creates a recursive dependency \
                 loop."
            ));
        }
        self.inv.add_child(parent, child);
        Ok(())
    }

    /// `_populate_host_vars` for every host a pattern expands to.
    fn hosts(&mut self, group: &str, pattern: &str, vars: &Value) -> Result<(), String> {
        let vars = match vars {
            Value::Object(vars) => vars.clone(),
            v if self.dialect == Dialect::Yaml && falsy(v) => Map::new(),
            other => {
                return Err(format!(
                    "Invalid data from file, expected dictionary and got:\n\n{}",
                    show(other)
                ));
            }
        };
        let (names, port) = expand_hostpattern(pattern)?;
        for name in names {
            let mut all = BTreeMap::new();
            if let Some(port) = port.filter(|p| *p != 0) {
                all.insert("ansible_port".to_string(), Value::from(port));
            }
            all.extend(vars.clone());
            self.inv.add_source_host(&name, None, all, self.file);
            self.inv.add_member(&name, group);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::inventory::PluginConfig;

    fn read(text: &str, dialect: Dialect) -> (Inventory, Result<(), String>) {
        let config = PluginConfig::default();
        let cx = ParseCx::new(&config);
        let mut inv = Inventory::default();
        let result = match dialect {
            Dialect::Yaml => {
                let docs = yaml::load(text, "t.yml").unwrap();
                lower(&docs[0]).and_then(|g| read_groups(&g, dialect, &mut inv, None, &cx))
            }
            Dialect::Toml => super::super::toml::read_text(text, &mut inv, &cx, "t.toml"),
        };
        (inv, result)
    }

    #[rstest]
    #[case::cycle(
        "a: {children: {b: {children: {a: ~}}}}",
        "Adding group 'b' as child to 'a' creates a recursive dependency loop."
    )]
    #[case::own_child("a: {children: {a: ~}}", "can't add group to itself")]
    #[case::int_host(
        "g: {hosts: {456: ~}}",
        "Host pattern 456 must be a string. Enclose integers/floats in quotation marks."
    )]
    #[case::list_hosts(
        "g: {hosts: [a]}",
        "Invalid \"hosts\" entry for \"g\" group, requires a dictionary, found \"<class \
         'ansible.module_utils._internal._datatag._AnsibleTaggedList'>\" instead."
    )]
    #[case::skipped_child("a: {children: {m: 5}}", "m is not a known host nor group")]
    #[case::empty("{}", "Parsed empty YAML file")]
    #[case::plugin("plugin: x", "Plugin configuration YAML file, not YAML inventory")]
    fn yaml_refusals(#[case] text: &str, #[case] message: &str) {
        let (_, result) = read(text, Dialect::Yaml);
        assert_eq!(result.unwrap_err(), message);
    }

    /// A group read before the error stays, as the reference's partial state does.
    #[test]
    fn a_cycle_keeps_what_was_read() {
        let (inv, result) = read(
            "x: {hosts: {h: ~}}\na: {children: {b: {children: {a: ~}}}}",
            Dialect::Yaml,
        );
        assert!(result.is_err());
        assert!(inv.group("x").is_some_and(|g| g.hosts == ["h"]));
    }
}
