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

    /// Groups in the order the script gives them, then each host's variables: from
    /// `_meta.hostvars`, or from one `--host` run per host when there is none. What was added
    /// before an error stays.
    fn parse(
        &self,
        source: &Source,
        into: &mut Inventory,
        cx: &ParseCx,
    ) -> Result<(), PluginError> {
        let path = source.text();
        let groups = run(path, &["--list"])?;
        let mut hostvars = None;
        let mut hosts: Vec<String> = Vec::new();
        for (name, data) in groups {
            if name == "_meta" {
                hostvars = Some(meta_hostvars(&data)?);
            } else {
                group(&name, data, source, into, cx, &mut hosts)?;
            }
        }
        let origin = Some(source.path());
        if let Some(mut all) = hostvars {
            for host in &hosts {
                let vars = match all.shift_remove(host) {
                    None => Map::new(),
                    Some(Value::Object(vars)) => vars,
                    Some(other) => {
                        return Err(failure(format!(
                            "Improperly formatted host information for {host}: '{}' object has \
                             no attribute 'items'",
                            type_name(&other)
                        )));
                    }
                };
                into.add_source_host(host, None, vars.into_iter().collect(), origin);
            }
        } else {
            cx.deprecate(deprecation(
                path,
                "Inventory scripts should always provide 'meta.hostvars'. Host variables will \
                 be collected by running the inventory script with the '--host' option for each \
                 host.",
            ));
            for host in &hosts {
                let vars = host_variables(path, host)?;
                into.add_source_host(host, None, vars.into_iter().collect(), origin);
            }
        }
        Ok(())
    }
}

/// A deprecation of the script plugin, worded and ended as the reference's `display.deprecated`.
fn deprecation(path: &str, what: &str) -> String {
    format!(
        "{what} This feature will be removed from inventory plugin 'script' in ansible-core \
         version 2.23.\nOrigin: <inventory script output from {}>\n",
        py_repr(path)
    )
}

/// Python's `repr` of a string.
fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `shlex.quote`.
fn shell_quote(word: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c);
    if !word.is_empty() && word.chars().all(safe) {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\"'\"'"))
    }
}

fn failure(message: String) -> PluginError {
    PluginError {
        message,
        origin: None,
        cause: None,
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
fn meta_hostvars(meta: &Value) -> Result<Map<String, Value>, PluginError> {
    match meta.get("hostvars") {
        Some(Value::Object(hostvars)) => Ok(hostvars.clone()),
        other => Err(failure(format!(
            "Value contains '_meta.hostvars' which is '{}' instead of 'dict'.",
            type_name(other.unwrap_or(&Value::Null))
        ))),
    }
}

/// An error that carries the script's standard error, the way the reference's `help_text`
/// does: a block after the `Origin:` line (the chain adds the line's own newline).
fn with_stderr(path: &str, message: String, stderr: &str) -> PluginError {
    let help = (!stderr.trim().is_empty()).then(|| {
        format!(
            "<inventory plugin 'script' with source '{path}'>\n\nStandard error from inventory \
             script:\n{}",
            stderr.strip_suffix('\n').unwrap_or(stderr)
        )
    });
    PluginError {
        message,
        origin: help,
        cause: None,
        unsupported: false,
    }
}

/// `run_command`: the script's standard output as text, or the reference's words for why not.
fn run_command(path: &str, args: &[&str]) -> Result<(String, String), PluginError> {
    let output = Command::new(path).args(args).output().map_err(|_| {
        let words: Vec<String> = std::iter::once(path)
            .chain(args.iter().copied())
            .map(shell_quote)
            .collect();
        failure(format!(
            "Failed to execute inventory script command {}.",
            py_repr(&words.join(" "))
        ))
    })?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        return Err(with_stderr(
            path,
            format!(
                "Inventory script returned non-zero exit code {}.",
                exit_code(output.status)
            ),
            &stderr,
        ));
    }
    let text = String::from_utf8(output.stdout).map_err(|_| {
        with_stderr(
            path,
            "Inventory script result contained characters that cannot be interpreted as UTF-8."
                .to_string(),
            &stderr,
        )
    })?;
    Ok((text, stderr))
}

/// `--list`: a JSON mapping, or the reference's words for why not.
fn run(path: &str, args: &[&str]) -> Result<Map<String, Value>, PluginError> {
    let (text, stderr) = run_command(path, args)?;
    let decoder = |reason: String| {
        with_stderr(
            path,
            format!("Unable to get JSON decoder for inventory script result: {reason}"),
            &stderr,
        )
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => {
            match map.get("_meta") {
                Some(Value::Object(_)) | None => {}
                Some(other) => {
                    return Err(decoder(format!(
                        "Value contains '_meta' which is '{}' instead of 'dict'.",
                        type_name(other)
                    )));
                }
            }
            match map.get("_meta").and_then(|m| m.get("profile")) {
                None => {}
                Some(Value::String(p)) if p.starts_with("inventory_") => {}
                Some(Value::String(p)) => {
                    return Err(decoder(format!(
                        "Non-inventory profile {} is not allowed.",
                        py_repr(p)
                    )));
                }
                Some(other) => {
                    return Err(decoder(format!(
                        "Value contains '_meta.profile' which is '{}' instead of 'str'.",
                        type_name(other)
                    )));
                }
            }
            Ok(map)
        }
        Ok(other) => Err(decoder(format!(
            "Value is '{}' instead of 'dict'.",
            type_name(&other)
        ))),
        Err(e) => Err(decoder(format!(
            "Value could not be parsed as JSON: {}",
            python_json_error(text.as_bytes(), &e)
        ))),
    }
}

/// `--host`: the host's variables; nothing but white space is an empty mapping.
fn host_variables(path: &str, host: &str) -> Result<Map<String, Value>, PluginError> {
    let (text, stderr) = run_command(path, &["--host", host])?;
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(vars)) => Ok(vars),
        Err(e) => {
            let mut error = with_stderr(
                path,
                format!(
                    "Inventory script result for host {} could not be parsed as JSON.",
                    py_repr(host)
                ),
                &stderr,
            );
            error.cause = Some((
                format!(
                    "JSON parsing failed: {}",
                    python_json_error(text.as_bytes(), &e)
                ),
                Some(format!(
                    "<inventory script output from {}> for host {}:{}:{}",
                    py_repr(path),
                    py_repr(host),
                    e.line(),
                    e.column()
                )),
            ));
            Err(error)
        }
        Ok(_) => Err(with_stderr(
            path,
            format!(
                "Inventory script result for host {} could not be parsed as JSON.",
                py_repr(host)
            ),
            &stderr,
        )),
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
// Serde's and Python's parsers stop at the same character for the common mistakes
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
    let wrong = |key: &str, got: &Value, wanted: &str| {
        failure(format!(
            "Value contains '{name}.{key}' which is '{}' instead of '{wanted}'.",
            type_name(got)
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
                cx.deprecate(deprecation(
                    source.text(),
                    &format!(
                        "Treating malformed group {} as the sole host of that group. Variables \
                         provided in this manner cannot be templated.",
                        py_repr(name)
                    ),
                ));
                Map::from_iter([
                    ("hosts".to_string(), Value::from(vec![name])),
                    ("vars".to_string(), Value::Object(map)),
                ])
            }
        }
        other => {
            cx.deprecate(deprecation(
                source.text(),
                &format!(
                    "Group {} was converted to 'dict' from '{}'.",
                    py_repr(name),
                    type_name(&other)
                ),
            ));
            Map::from_iter([("hosts".to_string(), other)])
        }
    };
    if let Some(listed) = data.get("hosts") {
        let Value::Array(listed) = listed else {
            return Err(wrong("hosts", listed, "list"));
        };
        for host in listed {
            let Value::String(host) = host else {
                return Err(wrong("hosts", host, "str"));
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
            return Err(wrong("vars", vars, "dict"));
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
            return Err(wrong("children", children, "list"));
        };
        for child in children {
            let Value::String(child) = child else {
                return Err(wrong("children", child, "str"));
            };
            into.add_child(name, child);
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests;
