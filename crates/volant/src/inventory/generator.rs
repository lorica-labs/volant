// SPDX-License-Identifier: GPL-3.0-or-later
//! The `generator` inventory plugin of ansible-core 2.19.12: one host for each combination of
//! its layers, named by a template, under the chain of parent groups the config describes.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::auto::failed;
use super::constructed::{has_invalid_chars, py_str, read_config, verify_config};
use super::yaml::{Dialect, add_child_checked};
use super::{Inventory, InventoryPlugin, ParseCx, PluginError, Source};
use crate::template::Templar;

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
        };
        let mut lists = Vec::new();
        for values in layers.values() {
            lists.push(items(values).map_err(failed)?);
        }
        for combination in product(&lists) {
            let vars: Map<String, Value> = layers.keys().cloned().zip(combination).collect();
            let host = py_str(&run.template(&name, &vars).map_err(failed)?);
            into.add_source_host(&host, None, BTreeMap::new(), Some(run.file));
            run.parents(into, &host, &parents, &vars).map_err(failed)?;
        }
        Ok(())
    }
}

struct Run<'a, 'c> {
    templar: Templar,
    cx: &'a ParseCx<'c>,
    file: &'a std::path::Path,
}

impl Run<'_, '_> {
    /// `template`: a string is rendered, anything else is passed through for later.
    fn template(&self, pattern: &Value, vars: &Map<String, Value>) -> Result<Value, String> {
        match pattern {
            Value::String(text) => self.templar.render_one_pass(text, vars).map_err(|e| {
                e.0.split_once("The error was: ")
                    .map_or(e.0.clone(), |(_, r)| r.to_string())
            }),
            other => Ok(other.clone()),
        }
    }

    fn parents(
        &self,
        inv: &mut Inventory,
        child: &str,
        parents: &Value,
        vars: &Map<String, Value>,
    ) -> Result<(), String> {
        for parent in items(parents)? {
            let Value::Object(parent) = parent else {
                return Err("'str' object has no attribute 'get'".to_string());
            };
            let name = self.template(parent.get("name").unwrap_or(&Value::Null), vars)?;
            if super::yaml::falsy(&name) {
                return Err(format!("Element {child} has a parent with no name."));
            }
            let group = py_str(&name);
            if inv.group(&group).is_none() {
                if has_invalid_chars(&group) {
                    self.cx.warn_once(
                        "Invalid characters were found in group names but not replaced, use \
                         -vvvv to see details"
                            .to_string(),
                    );
                }
                inv.group_mut(&group);
            }
            if let Some(Value::Object(set)) = parent.get("vars") {
                for (var, value) in set {
                    let value = self.template(value, vars)?;
                    if var == "ansible_group_priority" {
                        if let Some(p) = super::ini::priority(&value)? {
                            inv.group_mut(&group).priority = Some(p);
                        }
                    } else {
                        inv.group_mut(&group).vars.insert(var.clone(), value);
                    }
                }
            }
            if inv.group(child).is_some() {
                add_child_checked(inv, &group, child)?;
            } else {
                inv.add_member(child, &group);
            }
            self.parents(
                inv,
                &group,
                parent.get("parents").unwrap_or(&Value::Array(Vec::new())),
                vars,
            )?;
        }
        Ok(())
    }
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
