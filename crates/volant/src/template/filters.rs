// SPDX-License-Identifier: GPL-3.0-or-later
//! Ansible's filters, tests and lookups on top of MiniJinja's Jinja2 builtins. Each one keeps
//! Ansible's argument names and its edge cases, checked by tests/golden.

use std::path::{Path, PathBuf};

use minijinja::value::{Kwargs, Rest};
use minijinja::{Environment, Error, ErrorKind, State, Value};

use super::truthy;

pub fn register(env: &mut Environment<'static>, base_dir: PathBuf) {
    env.add_filter("default", default);
    env.add_filter("d", default);
    env.add_filter("bool", to_bool);
    env.add_filter("int", to_int);
    env.add_filter("float", to_float);
    env.add_filter("mandatory", mandatory);
    env.add_filter("ternary", ternary);
    env.add_filter("combine", combine);
    env.add_filter("dict2items", dict2items);
    env.add_filter("items2dict", items2dict);
    env.add_filter("to_json", to_json);
    env.add_filter("to_nice_json", to_nice_json);
    env.add_filter("from_json", from_json);
    env.add_filter("basename", basename);
    env.add_filter("dirname", dirname);
    env.add_filter("split", split);
    env.add_filter("regex_replace", regex_replace);
    env.add_filter("regex_search", regex_search);
    env.add_filter("regex_findall", regex_findall);

    env.add_test("truthy", |v: Value| truthy(&json(&v)));
    env.add_test("falsy", |v: Value| !truthy(&json(&v)));
    env.add_test("match", |v: Value, pattern: String, kwargs: Kwargs| {
        regex_test(&v, &pattern, true, kwargs)
    });
    env.add_test("search", |v: Value, pattern: String, kwargs: Kwargs| {
        regex_test(&v, &pattern, false, kwargs)
    });
    env.add_test("regex", |v: Value, pattern: String, kwargs: Kwargs| {
        regex_test(&v, &pattern, false, kwargs)
    });
    env.add_test("contains", |seq: Value, item: Value| {
        seq.try_iter()
            .map(|mut it| it.any(|x| x == item))
            .unwrap_or(false)
    });

    env.add_function(
        "lookup",
        move |state: &State, name: String, terms: Rest<Value>, kwargs: Kwargs| {
            lookup(state, &name, &terms, kwargs, &base_dir)
        },
    );

    // Anything Ansible has that is not implemented yet must fail by name, not silently.
    env.set_unknown_method_callback(|_state, _value, method, _args| {
        Err(Error::new(
            ErrorKind::UnknownMethod,
            format!("method {method} is not available yet"),
        ))
    });
}

fn json(v: &Value) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
}

fn from_json_value(v: serde_json::Value) -> Value {
    Value::from_serialize(&v)
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidOperation, msg.into())
}

/// `default(value, other='', boolean=False)`: `other` when `value` is undefined, or when
/// `boolean` is set and `value` is false-y.
fn default(value: Value, other: Option<Value>, boolean: Option<bool>) -> Value {
    let other = other.unwrap_or_else(|| Value::from(""));
    if value.is_undefined() || (boolean.unwrap_or(false) && !truthy(&json(&value))) {
        other
    } else {
        value
    }
}

/// Ansible's `boolean()` with strict=False: recognised spellings, everything else is false.
fn to_bool(value: Value) -> bool {
    match json(&value) {
        serde_json::Value::Bool(b) => b,
        serde_json::Value::Number(n) => n.as_f64() == Some(1.0),
        serde_json::Value::String(s) => matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "y" | "yes" | "on" | "1" | "true" | "t"
        ),
        _ => false,
    }
}

fn to_int(value: Value, default_value: Option<i64>) -> Result<Value, Error> {
    let fallback = default_value.unwrap_or(0);
    Ok(Value::from(match json(&value) {
        serde_json::Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f.trunc() as i64))
            .unwrap_or(fallback),
        serde_json::Value::Bool(b) => i64::from(b),
        serde_json::Value::String(s) => s
            .trim()
            .parse::<i64>()
            .or_else(|_| s.trim().parse::<f64>().map(|f| f.trunc() as i64))
            .unwrap_or(fallback),
        _ => fallback,
    }))
}

fn to_float(value: Value, default_value: Option<f64>) -> Value {
    let fallback = default_value.unwrap_or(0.0);
    Value::from(match json(&value) {
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(fallback),
        serde_json::Value::Bool(b) => {
            if b {
                1.0
            } else {
                0.0
            }
        }
        serde_json::Value::String(s) => s.trim().parse::<f64>().unwrap_or(fallback),
        _ => fallback,
    })
}

fn mandatory(value: Value, msg: Option<String>) -> Result<Value, Error> {
    if value.is_undefined() {
        return Err(invalid(
            msg.unwrap_or_else(|| "Mandatory variable not defined.".to_string()),
        ));
    }
    Ok(value)
}

fn ternary(value: Value, true_val: Value, false_val: Value, none_val: Option<Value>) -> Value {
    if value.is_none()
        && let Some(none_val) = none_val
    {
        return none_val;
    }
    if truthy(&json(&value)) {
        true_val
    } else {
        false_val
    }
}

fn combine(first: Value, rest: Rest<Value>, kwargs: Kwargs) -> Result<Value, Error> {
    let recursive: bool = kwargs.get::<Option<bool>>("recursive")?.unwrap_or(false);
    let list_merge: String = kwargs
        .get::<Option<String>>("list_merge")?
        .unwrap_or_else(|| "replace".to_string());
    kwargs.assert_all_used()?;
    let mut out = match json(&first) {
        serde_json::Value::Object(m) => m,
        _ => return Err(invalid("|combine expects dictionaries")),
    };
    for other in rest.iter() {
        let serde_json::Value::Object(other) = json(other) else {
            return Err(invalid("|combine expects dictionaries"));
        };
        merge_into(&mut out, other, recursive, &list_merge);
    }
    Ok(from_json_value(serde_json::Value::Object(out)))
}

fn merge_into(
    target: &mut serde_json::Map<String, serde_json::Value>,
    source: serde_json::Map<String, serde_json::Value>,
    recursive: bool,
    list_merge: &str,
) {
    for (k, v) in source {
        match (target.get_mut(&k), v) {
            (Some(serde_json::Value::Object(t)), serde_json::Value::Object(s)) if recursive => {
                merge_into(t, s, recursive, list_merge)
            }
            (Some(serde_json::Value::Array(t)), serde_json::Value::Array(s))
                if list_merge == "append" =>
            {
                t.extend(s)
            }
            (Some(serde_json::Value::Array(t)), serde_json::Value::Array(mut s))
                if list_merge == "prepend" =>
            {
                s.append(t);
                *t = s;
            }
            (_, v) => {
                target.insert(k, v);
            }
        }
    }
}

fn dict2items(value: Value, kwargs: Kwargs) -> Result<Value, Error> {
    let key_name: String = kwargs
        .get::<Option<String>>("key_name")?
        .unwrap_or_else(|| "key".to_string());
    let value_name: String = kwargs
        .get::<Option<String>>("value_name")?
        .unwrap_or_else(|| "value".to_string());
    kwargs.assert_all_used()?;
    let serde_json::Value::Object(map) = json(&value) else {
        return Err(invalid("dict2items requires a dictionary"));
    };
    let items: Vec<serde_json::Value> = map
        .into_iter()
        .map(|(k, v)| serde_json::json!({ key_name.clone(): k, value_name.clone(): v }))
        .collect();
    Ok(from_json_value(serde_json::Value::Array(items)))
}

fn items2dict(value: Value, kwargs: Kwargs) -> Result<Value, Error> {
    let key_name: String = kwargs
        .get::<Option<String>>("key_name")?
        .unwrap_or_else(|| "key".to_string());
    let value_name: String = kwargs
        .get::<Option<String>>("value_name")?
        .unwrap_or_else(|| "value".to_string());
    kwargs.assert_all_used()?;
    let serde_json::Value::Array(items) = json(&value) else {
        return Err(invalid("items2dict requires a list"));
    };
    let mut out = serde_json::Map::new();
    for item in items {
        let key = item
            .get(&key_name)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                invalid(format!(
                    "items2dict requires each item to have a '{key_name}' string"
                ))
            })?
            .to_string();
        out.insert(
            key,
            item.get(&value_name)
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        );
    }
    Ok(from_json_value(serde_json::Value::Object(out)))
}

/// Python's `json.dumps` default separators: `", "` and `": "`, keys in insertion order.
fn to_json(value: Value) -> Result<Value, Error> {
    Ok(Value::from(python_json(&json(&value), None, 0)))
}

fn to_nice_json(value: Value, kwargs: Kwargs) -> Result<Value, Error> {
    let indent: usize = kwargs.get::<Option<usize>>("indent")?.unwrap_or(4);
    kwargs.assert_all_used()?;
    Ok(Value::from(python_json(&json(&value), Some(indent), 0)))
}

fn python_json(v: &serde_json::Value, indent: Option<usize>, depth: usize) -> String {
    let pad = |d: usize| {
        indent
            .map(|i| format!("\n{}", " ".repeat(i * d)))
            .unwrap_or_default()
    };
    match v {
        serde_json::Value::Object(map) if map.is_empty() => "{}".to_string(),
        serde_json::Value::Array(items) if items.is_empty() => "[]".to_string(),
        serde_json::Value::Object(map) => {
            let sep = if indent.is_some() { "," } else { ", " };
            let fields: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}{}: {}",
                        pad(depth + 1),
                        serde_json::to_string(k).unwrap_or_default(),
                        python_json(v, indent, depth + 1)
                    )
                })
                .collect();
            format!("{{{}{}}}", fields.join(sep), pad(depth))
        }
        serde_json::Value::Array(items) => {
            let sep = if indent.is_some() { "," } else { ", " };
            let fields: Vec<String> = items
                .iter()
                .map(|v| format!("{}{}", pad(depth + 1), python_json(v, indent, depth + 1)))
                .collect();
            format!("[{}{}]", fields.join(sep), pad(depth))
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn from_json(value: Value) -> Result<Value, Error> {
    let text = value
        .as_str()
        .ok_or_else(|| invalid("from_json requires a string"))?;
    let parsed: serde_json::Value =
        serde_json::from_str(text).map_err(|e| invalid(format!("from_json: {e}")))?;
    Ok(from_json_value(parsed))
}

fn basename(value: Value) -> Result<Value, Error> {
    let text = value
        .as_str()
        .ok_or_else(|| invalid("basename requires a string"))?;
    Ok(Value::from(text.rsplit('/').next().unwrap_or("")))
}

fn dirname(value: Value) -> Result<Value, Error> {
    let text = value
        .as_str()
        .ok_or_else(|| invalid("dirname requires a string"))?;
    Ok(Value::from(match text.rfind('/') {
        Some(0) => "/",
        Some(i) => &text[..i],
        None => "",
    }))
}

/// Python's `str.split`: no separator splits on runs of whitespace and drops empty fields.
fn split(value: Value, sep: Option<String>, maxsplit: Option<i64>) -> Result<Value, Error> {
    let text = value
        .as_str()
        .ok_or_else(|| invalid("split requires a string"))?;
    let limit = maxsplit.filter(|n| *n >= 0).map(|n| n as usize + 1);
    let parts: Vec<&str> = match (sep, limit) {
        (None, None) => text.split_whitespace().collect(),
        (None, Some(n)) => text
            .trim_start()
            .splitn(n, char::is_whitespace)
            .filter(|s| !s.is_empty())
            .collect(),
        (Some(sep), None) => text.split(sep.as_str()).collect(),
        (Some(sep), Some(n)) => text.splitn(n, sep.as_str()).collect(),
    };
    Ok(Value::from(
        parts.iter().map(|s| Value::from(*s)).collect::<Vec<_>>(),
    ))
}

fn compile(pattern: &str, kwargs: &Kwargs) -> Result<regex::Regex, Error> {
    let ignorecase: bool = kwargs.get::<Option<bool>>("ignorecase")?.unwrap_or(false);
    let multiline: bool = kwargs.get::<Option<bool>>("multiline")?.unwrap_or(false);
    regex::RegexBuilder::new(pattern)
        .case_insensitive(ignorecase)
        .multi_line(multiline)
        .build()
        .map_err(|e| invalid(format!("invalid regular expression '{pattern}': {e}")))
}

/// MiniJinja's string literals fold `\1`..`\7` into the raw byte `chr(1)`..`chr(7)` (a JSON-style
/// octal escape); checked directly against ansible-core 2.19, its Jinja dialect does not do this
/// at all (`\n`, `\t`, `\1`, `\x41`, even `\'` all stay perfectly literal in a string literal, see
/// docs/superpowers/architecture.md). Undo the fold for the arguments that carry Python-style
/// backreferences, so `'\1'` keeps meaning "backreference one" rather than a control byte.
/// ponytail: only single-digit octal folds (`\0`-`\7`) are reversed, matching every golden case;
/// MiniJinja's multi-digit octal folds (`\12` -> one byte) lose which digits were consumed, so a
/// two-digit backreference (`\10` and above) would need a real fix, not this inverse map.
fn undo_octal_fold(s: &str) -> String {
    s.chars()
        .map(|c| match c as u32 {
            n @ 0..=7 => format!("\\{n}"),
            _ => c.to_string(),
        })
        .collect()
}

/// Python back-references (`\1`, `\g<name>`) to the `regex` crate's `${1}` and `${name}`.
fn python_replacement(text: &str) -> String {
    let with_named = regex::Regex::new(r"\\g<([A-Za-z0-9_]+)>")
        .expect("static pattern")
        .replace_all(text, "$${$1}");
    regex::Regex::new(r"\\(\d+)")
        .expect("static pattern")
        .replace_all(&with_named, "$${$1}")
        .into_owned()
}

fn regex_replace(
    value: Value,
    pattern: String,
    replacement: String,
    kwargs: Kwargs,
) -> Result<Value, Error> {
    let text = value
        .as_str()
        .ok_or_else(|| invalid("regex_replace requires a string"))?;
    let re = compile(&pattern, &kwargs)?;
    kwargs.assert_all_used()?;
    let replacement = undo_octal_fold(&replacement);
    Ok(Value::from(
        re.replace_all(text, python_replacement(&replacement).as_str())
            .into_owned(),
    ))
}

/// `regex_search(value, pattern, *groups)`: the whole match, or the listed groups (`\1`, `\g<name>`)
/// as a list; `None` when nothing matches.
fn regex_search(
    value: Value,
    pattern: String,
    groups: Rest<String>,
    kwargs: Kwargs,
) -> Result<Value, Error> {
    let text = value
        .as_str()
        .ok_or_else(|| invalid("regex_search requires a string"))?;
    let re = compile(&pattern, &kwargs)?;
    kwargs.assert_all_used()?;
    let Some(caps) = re.captures(text) else {
        return Ok(Value::from(()));
    };
    if groups.is_empty() {
        return Ok(Value::from(caps.get(0).map(|m| m.as_str()).unwrap_or("")));
    }
    let mut out = Vec::new();
    for g in groups.iter() {
        let g = undo_octal_fold(g);
        let m = if let Some(n) = g.strip_prefix('\\').and_then(|n| n.parse::<usize>().ok()) {
            caps.get(n)
        } else if let Some(name) = g.strip_prefix("\\g<").and_then(|n| n.strip_suffix('>')) {
            caps.name(name)
        } else {
            return Err(invalid(format!("Unknown match group '{g}'")));
        };
        out.push(Value::from(m.map(|m| m.as_str()).unwrap_or("")));
    }
    Ok(Value::from(out))
}

fn regex_findall(value: Value, pattern: String, kwargs: Kwargs) -> Result<Value, Error> {
    let text = value
        .as_str()
        .ok_or_else(|| invalid("regex_findall requires a string"))?;
    let re = compile(&pattern, &kwargs)?;
    kwargs.assert_all_used()?;
    let matches: Vec<Value> = re
        .find_iter(text)
        .map(|m| Value::from(m.as_str()))
        .collect();
    Ok(Value::from(matches))
}

fn regex_test(value: &Value, pattern: &str, anchored: bool, kwargs: Kwargs) -> bool {
    let Some(text) = value.as_str() else {
        return false;
    };
    let Ok(re) = compile(pattern, &kwargs) else {
        return false;
    };
    match re.find(text) {
        Some(m) if anchored => m.start() == 0,
        Some(_) => true,
        None => false,
    }
}

/// `lookup('env'|'file'|'vars'|'pipe', term...)`. One term gives a scalar, several give a list.
fn lookup(
    state: &State,
    name: &str,
    terms: &[Value],
    kwargs: Kwargs,
    base_dir: &Path,
) -> Result<Value, Error> {
    let default_value: Option<Value> = kwargs.get::<Option<Value>>("default")?;
    kwargs.assert_all_used()?;
    let mut results = Vec::new();
    for term in terms {
        let term_text = term
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| term.to_string());
        let found = match name {
            "env" | "ansible.builtin.env" => {
                Value::from(std::env::var(&term_text).unwrap_or_default())
            }
            "file" | "ansible.builtin.file" => {
                let path = if term_text.starts_with('/') {
                    PathBuf::from(&term_text)
                } else {
                    base_dir.join(&term_text)
                };
                let text = std::fs::read_to_string(&path).map_err(|e| {
                    invalid(format!(
                        "could not locate file in lookup: {}: {e}",
                        path.display()
                    ))
                })?;
                Value::from(text.trim_end_matches(['\r', '\n']))
            }
            "vars" | "ansible.builtin.vars" => match state.lookup(&term_text) {
                Some(v) if !v.is_undefined() => v,
                _ => match &default_value {
                    Some(d) => d.clone(),
                    None => {
                        return Err(invalid(format!(
                            "No variable found with this name: {term_text}"
                        )));
                    }
                },
            },
            "pipe" | "ansible.builtin.pipe" => {
                let out = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&term_text)
                    .output()
                    .map_err(|e| invalid(format!("lookup pipe: {e}")))?;
                if !out.status.success() {
                    return Err(invalid(format!(
                        "lookup_plugin.pipe({term_text}) returned {}",
                        out.status.code().unwrap_or(-1)
                    )));
                }
                Value::from(
                    String::from_utf8_lossy(&out.stdout)
                        .trim_end_matches(['\r', '\n'])
                        .to_string(),
                )
            }
            other => {
                return Err(invalid(format!(
                    "lookup plugin ({other}) is not available yet"
                )));
            }
        };
        results.push(found);
    }
    Ok(match results.len() {
        0 => Value::from(""),
        1 => results.remove(0),
        _ => Value::from(results),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_back_references_become_regex_crate_references() {
        assert_eq!(python_replacement(r"A\1-\g<name>"), "A${1}-${name}");
        assert_eq!(python_replacement("plain"), "plain");
    }

    #[test]
    fn python_json_uses_pythons_separators_and_indent() {
        let v = serde_json::json!({"b": 1, "a": [1, 2]});
        assert_eq!(python_json(&v, None, 0), r#"{"a": [1, 2], "b": 1}"#);
        assert_eq!(
            python_json(&v, Some(4), 0),
            "{\n    \"a\": [\n        1,\n        2\n    ],\n    \"b\": 1\n}"
        );
    }

    #[test]
    fn an_unknown_filter_names_itself() {
        let t = super::super::Templar::new(std::env::temp_dir());
        let err = t
            .render("{{ 1 | some_unknown_filter }}", &serde_json::Map::new())
            .unwrap_err();
        assert!(err.0.contains("some_unknown_filter"), "{err}");
    }
}
