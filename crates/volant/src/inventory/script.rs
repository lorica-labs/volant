// SPDX-License-Identifier: GPL-3.0-or-later
//! The `script` inventory plugin: an executable run with `--list`, and with `--host` for each
//! host when it gives no `_meta.hostvars`.

use std::collections::BTreeMap;
use std::process::Command;

use serde_json::{Map, Value};

use super::{Inventory, InventoryPlugin, ParseCx, PluginError, Source, ini, sources};

pub(super) struct Script;

impl InventoryPlugin for Script {
    fn name(&self) -> &'static str {
        "script"
    }

    fn verify(&self, source: &Source) -> bool {
        sources::executable(source)
    }

    /// Groups in name order, as the reference walks them, then each host's variables: from
    /// `_meta.hostvars`, or from one `--host` run per host when there is none. What was added
    /// before an error stays.
    fn parse(
        &self,
        source: &Source,
        into: &mut Inventory,
        cx: &ParseCx,
    ) -> Result<(), PluginError> {
        let path = source.text();
        let mut groups = run(path, &["--list"])?;
        let mut hostvars = None;
        if let Some(meta) = groups.shift_remove("_meta") {
            hostvars = Some(meta_hostvars(meta)?);
        }
        groups.sort_keys();
        let mut hosts: Vec<String> = Vec::new();
        for (name, data) in groups {
            group(&name, data, source, into, cx, &mut hosts)?;
        }
        let origin = Some(source.path());
        if let Some(mut all) = hostvars {
            for host in &hosts {
                let vars = match all.shift_remove(host) {
                    None => Map::new(),
                    Some(Value::Object(vars)) => vars,
                    Some(other) => {
                        return Err(failure(format!(
                            "Improperly formatted host information for {host}: '{}' object \
                                 has no attribute 'items'",
                            type_name(&other)
                        )));
                    }
                };
                into.add_source_host(host, None, vars.into_iter().collect(), origin);
            }
        } else {
            cx.deprecate(format!(
                "Inventory scripts should always provide 'meta.hostvars'. Host variables \
                     will be collected by running the inventory script with the '--host' \
                     option for each host. This feature will be removed from inventory plugin \
                     'script' in ansible-core version 2.23.\nOrigin: {}\n",
                shown_origin(path)
            ));
            for host in &hosts {
                let vars = run(path, &["--host", host])?;
                into.add_source_host(host, None, vars.into_iter().collect(), origin);
            }
        }
        Ok(())
    }
}

fn shown_origin(path: &str) -> String {
    format!("<inventory script output from '{path}'>")
}

fn failure(message: String) -> PluginError {
    PluginError {
        message,
        origin: None,
        unsupported: false,
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `_meta`'s `hostvars`, which has to be a mapping.
fn meta_hostvars(meta: Value) -> Result<Map<String, Value>, PluginError> {
    let decoder = |what: &str, value: &Value| {
        failure(format!(
            "Value contains '{what}' which is '{}' instead of 'dict'.",
            type_name(value)
        ))
    };
    let Value::Object(mut meta) = meta else {
        return Err(decoder("_meta", &meta));
    };
    match meta.shift_remove("hostvars") {
        Some(Value::Object(hostvars)) => Ok(hostvars),
        other => Err(decoder("_meta.hostvars", &other.unwrap_or(Value::Null))),
    }
}

/// The script's answer to `args`: a JSON mapping, or the reference's words for why not.
fn run(path: &str, args: &[&str]) -> Result<Map<String, Value>, PluginError> {
    let output = Command::new(path)
        .args(args)
        .output()
        .map_err(|e| failure(format!("problem running {path} {} ({e})", args.join(" "))))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim_end_matches('\n');
        return Err(PluginError {
            message: format!(
                "Inventory script returned non-zero exit code {}.",
                exit_code(output.status)
            ),
            origin: (!stderr.is_empty()).then(|| {
                format!(
                    "<inventory plugin 'script' with source '{path}'>\n\nStandard error from \
                     inventory script:\n{stderr}"
                )
            }),
            unsupported: false,
        });
    }
    let decoder = |reason: String| {
        failure(format!(
            "Unable to get JSON decoder for inventory script result: {reason}"
        ))
    };
    match serde_json::from_slice::<Value>(&output.stdout) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(other) => Err(decoder(format!(
            "Value is '{}' instead of 'dict'.",
            type_name(&other)
        ))),
        Err(e) => Err(decoder(format!(
            "Value could not be parsed as JSON: {}",
            python_json_error(&output.stdout, &e)
        ))),
    }
}

/// Python's `subprocess` return code: negative for a signal.
fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(0))
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(-1)
    }
}

/// `json.JSONDecodeError`'s text for what `serde_json` refused: its wording for the kinds of
/// mistake a script makes, with the position Python counts.
// ponytail: serde's and Python's parsers stop at the same character for the common mistakes
// only; an exotic one keeps serde's own wording until a measured case shows another.
fn python_json_error(text: &[u8], err: &serde_json::Error) -> String {
    let full = err.to_string();
    let reason = full
        .rsplit_once(" at line ")
        .map_or(full.as_str(), |(r, _)| r);
    let said = if reason.starts_with("key must be a string") {
        "Expecting property name enclosed in double quotes"
    } else if reason.starts_with("expected value") || reason.starts_with("EOF while parsing") {
        "Expecting value"
    } else if reason.starts_with("expected `,`") {
        "Expecting ',' delimiter"
    } else if reason.starts_with("expected `:`") {
        "Expecting ':' delimiter"
    } else if reason.starts_with("trailing characters") {
        "Extra data"
    } else {
        reason
    };
    let line = err.line().max(1);
    let column = err.column().max(1);
    let before: usize = text
        .split(|b| *b == b'\n')
        .take(line - 1)
        .map(|l| String::from_utf8_lossy(l).chars().count() + 1)
        .sum();
    format!(
        "{said}: line {line} column {column} (char {})",
        before + column - 1
    )
}

/// `_parse_group`: one group of the script's answer, its hosts, variables and children.
fn group(
    name: &str,
    data: Value,
    source: &Source,
    into: &mut Inventory,
    cx: &ParseCx,
    hosts: &mut Vec<String>,
) -> Result<(), PluginError> {
    let bad = |what: &str, data: &Value| {
        failure(format!(
            "You defined a group '{name}' with bad data for the {what}:\n {data}"
        ))
    };
    into.group_mut(name);
    let data = match data {
        Value::Object(map) => {
            if ["hosts", "vars", "children"]
                .iter()
                .any(|k| map.contains_key(*k))
            {
                map
            } else {
                // A mapping with none of the group keys is one host and its variables.
                Map::from_iter([
                    ("hosts".to_string(), Value::from(vec![name])),
                    ("vars".to_string(), Value::Object(map)),
                ])
            }
        }
        list => {
            cx.deprecate(format!(
                "Group '{name}' was converted to 'dict' from 'list'. This feature will be \
                 removed from inventory plugin 'script' in ansible-core version 2.23.\nOrigin: \
                 {}\n",
                shown_origin(source.text())
            ));
            Map::from_iter([("hosts".to_string(), list)])
        }
    };
    if let Some(listed) = data.get("hosts") {
        let Value::Array(listed) = listed else {
            return Err(bad("host list", &Value::Object(data.clone())));
        };
        for host in listed {
            let Value::String(host) = host else {
                return Err(bad("host list", &Value::Object(data.clone())));
            };
            if !hosts.contains(host) {
                hosts.push(host.clone());
            }
            into.add_source_host(host, None, BTreeMap::new(), Some(source.path()));
            into.add_member(host, name);
        }
    }
    if let Some(vars) = data.get("vars") {
        let Value::Object(vars) = vars else {
            return Err(bad("variables", &Value::Object(data.clone())));
        };
        for (key, value) in vars {
            if key == "ansible_group_priority" {
                if let Some(p) = ini::priority(value).map_err(failure)? {
                    into.group_mut(name).priority = Some(p);
                }
            } else {
                into.group_mut(name).vars.insert(key.clone(), value.clone());
            }
        }
    }
    if let Some(children) = data.get("children") {
        let Value::Array(children) = children else {
            return Err(bad("children list", &Value::Object(data.clone())));
        };
        for child in children {
            let Value::String(child) = child else {
                return Err(bad("children list", &Value::Object(data.clone())));
            };
            into.add_child(name, child);
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use rstest::rstest;

    use super::*;
    use crate::inventory::PluginConfig;

    fn load(body: &str) -> (Vec<String>, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls.log");
        let script = dir.path().join("inv.sh");
        let text = format!("#!/bin/sh\necho \"$@\" >> {}\n{body}\n", log.display());
        std::fs::write(&script, text).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let loaded = crate::inventory::load_sources(
            &[Source::new(&script.to_string_lossy())],
            &PluginConfig::default(),
        );
        let hosts = loaded
            .inventory
            .resolve("all")
            .hosts
            .into_iter()
            .map(|h| h.name)
            .collect();
        let calls = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect();
        (hosts, calls)
    }

    #[rstest]
    #[case::meta(
        r#"echo '{"_meta":{"hostvars":{"h1":{}}},"g":{"hosts":["h1"]}}'"#,
        &["--list"]
    )]
    #[case::no_meta(
        r#"[ "$1" = --list ] && echo '{"g":{"hosts":["h1","h2"]}}' || echo '{}'"#,
        &["--list", "--host h1", "--host h2"]
    )]
    fn host_calls_happen_only_without_meta(#[case] body: &str, #[case] calls: &[&str]) {
        let (hosts, got) = load(body);
        assert!(!hosts.is_empty());
        assert_eq!(got, calls);
    }
}
