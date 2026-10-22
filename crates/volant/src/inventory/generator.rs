// SPDX-License-Identifier: GPL-3.0-or-later
//! The `generator` inventory plugin of ansible-core 2.19.12: one host for each combination of
//! its layers, named by a template, under the chain of parent groups the config describes.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::auto::failed;
use super::constructed::{invalid_chars_report, py_str, read_config, reason, verify_config};
use super::yaml::{Dialect, add_child_checked};
use super::{Inventory, InventoryPlugin, ParseCx, PluginError, Source};
use crate::playbook::Origin;
use crate::template::Templar;
use crate::yaml;
use saphyr::Yaml;

pub(super) struct Generator;

impl InventoryPlugin for Generator {
    fn name(&self) -> &'static str {
        "generator"
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
        let key = |map: &Map<String, Value>, name: &str| {
            map.get(name)
                .cloned()
                .ok_or_else(|| failed(format!("'{name}'")))
        };
        let layers = match key(&config, "layers")? {
            Value::Object(layers) => layers,
            other => {
                return Err(failed(format!(
                    "'{}' object has no attribute 'values'",
                    Dialect::Toml
                        .type_name(&other)
                        .trim_start_matches("<class '")
                        .trim_end_matches("'>")
                )));
            }
        };
        let Value::Object(hosts) = key(&config, "hosts")? else {
            return Err(failed("'name'".to_string()));
        };
        let name = key(&hosts, "name")?;
        let parents = hosts
            .get("parents")
            .cloned()
            .unwrap_or(Value::Array(Vec::new()));
        let run = Run {
            templar: cx.templar_for(
                source.path().parent().map(Into::into).unwrap_or_default(),
                &Value::Object(config.clone()),
            ),
            cx,
            file: source.path(),
            source,
        };
        let mut lists = Vec::new();
        for values in layers.values() {
            lists.push(items(values).map_err(failed)?);
        }
        for combination in product(&lists) {
            let vars: Map<String, Value> = layers.keys().cloned().zip(combination).collect();
            let at = ["hosts".to_string(), "name".to_string()];
            let host = py_str(&run.template(&name, &vars, &at)?);
            into.add_source_host(&host, None, BTreeMap::new(), Some(run.file));
            run.parents(
                into,
                &host,
                &parents,
                &vars,
                &["hosts".to_string(), "parents".to_string()],
            )?;
        }
        Ok(())
    }
}

struct Run<'a, 'c> {
    templar: Templar,
    cx: &'a ParseCx<'c>,
    file: &'a std::path::Path,
    source: &'a Source,
}

impl Run<'_, '_> {
    /// `template`: a string is rendered, anything else is passed through for later. A failure
    /// points at `at`, the place of the pattern in the config file.
    fn template(
        &self,
        pattern: &Value,
        vars: &Map<String, Value>,
        at: &[String],
    ) -> Result<Value, PluginError> {
        match pattern {
            Value::String(text) => {
                self.templar
                    .render_one_pass(text, vars)
                    .map_err(|e| PluginError {
                        origin: origin_of(self.source, at),
                        ..failed(reason(&e.0, text, vars))
                    })
            }
            other => Ok(other.clone()),
        }
    }

    /// `parents` is the list found at `at` in the config file.
    fn parents(
        &self,
        inv: &mut Inventory,
        child: &str,
        parents: &Value,
        vars: &Map<String, Value>,
        at: &[String],
    ) -> Result<(), PluginError> {
        let step = |index: usize, key: &str| {
            let mut next = at.to_vec();
            next.extend([index.to_string(), key.to_string()]);
            next
        };
        for (index, parent) in items(parents).map_err(failed)?.into_iter().enumerate() {
            let Value::Object(parent) = parent else {
                return Err(failed("'str' object has no attribute 'get'".to_string()));
            };
            let name = self.template(
                parent.get("name").unwrap_or(&Value::Null),
                vars,
                &step(index, "name"),
            )?;
            if super::yaml::falsy(&name) {
                return Err(failed(format!(
                    "Element {child} has a parent with no name."
                )));
            }
            let group = py_str(&name);
            if inv.group(&group).is_none() {
                if let Some(chars) = invalid_chars_report(&group) {
                    self.cx.verbose(
                        4,
                        format!(
                            "Not replacing invalid character(s) \"{chars}\" in group name ({group})"
                        ),
                    );
                    self.cx.warn_once(
                        "Invalid characters were found in group names but not replaced, use                          -vvvv to see details"
                            .to_string(),
                    );
                }
                inv.group_mut(&group);
            }
            if let Some(Value::Object(set)) = parent.get("vars") {
                for (var, value) in set {
                    let mut place = step(index, "vars");
                    place.push(var.clone());
                    let value = self.template(value, vars, &place)?;
                    if var == "ansible_group_priority" {
                        if let Some(p) = super::ini::priority(&value).map_err(failed)? {
                            inv.group_mut(&group).priority = Some(p);
                        }
                    } else {
                        inv.group_mut(&group).vars.insert(var.clone(), value);
                    }
                }
            }
            if inv.group(child).is_some() {
                add_child_checked(inv, &group, child).map_err(failed)?;
            } else {
                inv.add_member(child, &group);
            }
            self.parents(
                inv,
                &group,
                parent.get("parents").unwrap_or(&Value::Array(Vec::new())),
                vars,
                &step(index, "parents"),
            )?;
        }
        Ok(())
    }
}

/// Where the value found at `path` of a config file starts, as the reference words it after
/// `Origin:`: the file, line and column, then the lines around it.
fn origin_of(source: &Source, path: &[String]) -> Option<String> {
    let text = std::fs::read_to_string(source.path()).ok()?;
    let (docs, origins) = yaml::load_with_origins(&text, source.text()).ok()?;
    let mut node = docs.first()?;
    for step in path {
        node = match node {
            Yaml::Sequence(items) => items.get(step.parse::<usize>().ok()?)?,
            _ => yaml::field(node, step)?,
        };
    }
    let (line, col) = origins.of_value(node)?;
    let shown = std::path::absolute(source.text())
        .map_or_else(|_| source.text().to_string(), |p| p.display().to_string());
    let origin = Origin {
        path: shown.into(),
        line,
        col,
    };
    let context = crate::render::source_context(&origin);
    Some(context.strip_prefix("Origin: ")?.to_string())
}

/// What iterating a value yields, as Python iterates it.
fn items(value: &Value) -> Result<Vec<Value>, String> {
    match value {
        Value::Array(list) => Ok(list.clone()),
        Value::String(text) => Ok(text.chars().map(|c| Value::String(c.to_string())).collect()),
        Value::Object(map) => Ok(map.keys().cloned().map(Value::String).collect()),
        other => Err(format!(
            "'{}' object is not iterable",
            Dialect::Toml
                .type_name(other)
                .trim_start_matches("<class '")
                .trim_end_matches("'>")
        )),
    }
}

/// `itertools.product`: the last list varies fastest.
fn product(lists: &[Vec<Value>]) -> Vec<Vec<Value>> {
    lists.iter().fold(vec![Vec::new()], |acc, list| {
        acc.iter()
            .flat_map(|prefix| {
                list.iter().map(move |item| {
                    let mut next = prefix.clone();
                    next.push(item.clone());
                    next
                })
            })
            .collect()
    })
}
