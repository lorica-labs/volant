// SPDX-License-Identifier: GPL-3.0-or-later
//! Ansible's filters, tests and lookups on top of MiniJinja's Jinja2 builtins. Each one keeps
//! Ansible's argument names and its edge cases, checked by tests/golden.

use std::fmt::Write;
use std::path::PathBuf;

use minijinja::value::{Kwargs, Rest};
use minijinja::{Environment, Error, ErrorKind, Value};

use volant_protocol::encoding::{b64_decode, b64_encode, sha1_hex};

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
    env.add_filter("b64decode", b64decode);
    env.add_filter("b64encode", b64encode);
    env.add_filter("comment", comment);
    env.add_filter("difference", |a: Value, b: Value| {
        set_filter(&a, &b, SetFilter::Difference)
    });
    env.add_filter("intersect", |a: Value, b: Value| {
        set_filter(&a, &b, SetFilter::Intersect)
    });
    env.add_filter("union", |a: Value, b: Value| {
        set_filter(&a, &b, SetFilter::Union)
    });
    env.add_filter("flatten", flatten);
    env.add_filter("from_yaml", from_yaml);
    env.add_filter("to_uuid", to_uuid);
    env.add_filter("type_debug", |v: Value| super::type_name(&json(&v)));
    env.add_filter("quote", quote);
    env.add_filter("regex_escape", regex_escape);
    super::yaml_dump::register(env);

    super::tests::register(env);
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
        seq.try_iter().is_ok_and(|mut it| it.any(|x| x == item))
    });

    super::lookups::register(env, base_dir);

    // Python's `str` and `dict` methods (`.split()`, `.startswith()`, `.keys()`...), which real
    // roles call on values. Anything neither `pycompat` nor this engine has must still fail by
    // name, not silently: `pycompat` answers a method it does not know with a bare
    // `UnknownMethod`, and an error of its own (a bad argument) keeps its detail.
    env.set_unknown_method_callback(|state, value, method, args| {
        minijinja_contrib::pycompat::unknown_method_callback(state, value, method, args).map_err(
            |e| {
                if e.kind() == ErrorKind::UnknownMethod && e.detail().is_none() {
                    Error::new(
                        ErrorKind::UnknownMethod,
                        format!("method {method} is not available yet"),
                    )
                } else {
                    e
                }
            },
        )
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

/// The `bool` filter's own set of spellings, which is **not** PyYAML's (see
/// [`crate::yaml::PYYAML_TRUE`]): it lower-cases the string and compares it against
/// `yes`, `on`, `1` and `true` only. Measured against ansible-core 2.19.12: `'yEs' | bool` is
/// true while `'y' | bool` and `'t' | bool` are false, and surrounding space is not stripped,
/// so a padded spelling is false too. Numbers are true only at exactly one (`1`, `1.00`), and
/// everything else, a non-empty list included, is false. `none | bool` is `false` too: measured
/// against ansible-core 2.19.12, `to_bool` coerces `None` to `False` (with a deprecation warning
/// that a future release will stop doing this), not the unchanged argument a stricter reading of
/// `to_bool`'s source would predict.
fn to_bool(value: Value) -> bool {
    match json(&value) {
        serde_json::Value::Bool(b) => b,
        serde_json::Value::Number(n) => n.as_f64() == Some(1.0),
        serde_json::Value::String(s) => {
            matches!(s.to_lowercase().as_str(), "yes" | "on" | "1" | "true")
        }
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
    let serde_json::Value::Object(mut out) = json(&first) else {
        return Err(invalid("|combine expects dictionaries"));
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
                merge_into(t, s, recursive, list_merge);
            }
            (Some(serde_json::Value::Array(t)), serde_json::Value::Array(s))
                if list_merge == "append" =>
            {
                t.extend(s);
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
/// Measured against ansible-core 2.19.12: `{b: 1, a: 2, c: 3} | to_json` gives
/// `{"b": 1, "a": 2, "c": 3}`, so `to_json` keeps the order the mapping was written in.
fn to_json(value: Value) -> Result<Value, Error> {
    Ok(Value::from(python_json(
        &json(&value),
        None,
        false,
        false,
        0,
    )))
}

/// `to_nice_json` is the one that sorts: the reference calls `json.dumps` with
/// `sort_keys=True`, and the same mapping comes back as `{"a": 2, "b": 1, "c": 3}`.
fn to_nice_json(value: Value, kwargs: Kwargs) -> Result<Value, Error> {
    let indent: usize = kwargs.get::<Option<usize>>("indent")?.unwrap_or(4);
    kwargs.assert_all_used()?;
    Ok(Value::from(python_json(
        &json(&value),
        Some(indent),
        true,
        false,
        0,
    )))
}

/// Python's `json.dumps`, the one imitation of it in this crate: `", "` between items without an
/// indent and `","` with one, `": "` after a key, keys in their order unless `sort_keys`. With
/// `ensure_ascii`, which is `json.dumps`' own default, every character past ASCII is escaped as
/// `\uXXXX`, a UTF-16 surrogate pair for one outside the basic plane.
pub(crate) fn python_json(
    v: &serde_json::Value,
    indent: Option<usize>,
    sort_keys: bool,
    ensure_ascii: bool,
    depth: usize,
) -> String {
    let pad = |d: usize| {
        indent
            .map(|i| format!("\n{}", " ".repeat(i * d)))
            .unwrap_or_default()
    };
    let string = |s: &str| {
        let quoted = serde_json::to_string(s).unwrap_or_default();
        if !ensure_ascii {
            return quoted;
        }
        let mut out = String::new();
        for c in quoted.chars() {
            // Python escapes everything outside space to `~`, so DEL too; serde_json has
            // already escaped the control characters below space.
            if c.is_ascii() && c != '\u{7f}' {
                out.push(c);
            } else {
                for unit in c.encode_utf16(&mut [0; 2]) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
        out
    };
    match v {
        serde_json::Value::Object(map) if map.is_empty() => "{}".to_string(),
        serde_json::Value::Array(items) if items.is_empty() => "[]".to_string(),
        serde_json::Value::Object(map) => {
            let sep = if indent.is_some() { "," } else { ", " };
            let mut keys: Vec<&String> = map.keys().collect();
            if sort_keys {
                keys.sort();
            }
            let fields: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    format!(
                        "{}{}: {}",
                        pad(depth + 1),
                        string(k),
                        python_json(&map[k], indent, sort_keys, ensure_ascii, depth + 1)
                    )
                })
                .collect();
            format!("{{{}{}}}", fields.join(sep), pad(depth))
        }
        serde_json::Value::Array(items) => {
            let sep = if indent.is_some() { "," } else { ", " };
            let fields: Vec<String> = items
                .iter()
                .map(|v| {
                    format!(
                        "{}{}",
                        pad(depth + 1),
                        python_json(v, indent, sort_keys, ensure_ascii, depth + 1)
                    )
                })
                .collect();
            format!("[{}{}]", fields.join(sep), pad(depth))
        }
        serde_json::Value::String(s) => string(s),
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
/// at all: `\n`, `\t`, `\1`, `\x41` and even `\'` all stay literal in a string literal.
/// Undo the fold for the arguments that carry Python-style
/// backreferences, so `'\1'` keeps meaning "backreference one" rather than a control byte.
/// Only single-digit octal folds (`\0`-`\7`) are reversed, matching every golden case;
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
        return Ok(Value::from(caps.get(0).map_or("", |m| m.as_str())));
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
        out.push(Value::from(m.map_or("", |m| m.as_str())));
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

/// A value as Python's `to_text` would hand it to a filter: a string as it is, anything else
/// printed.
fn text_of(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_string)
}

/// `encoding` and `urlsafe`, the two options `b64decode` and `b64encode` share. Only UTF-8 is
/// an encoding here: any other is refused by name rather than read as UTF-8.
fn b64_options(filter: &str, encoding: Option<String>, kwargs: &Kwargs) -> Result<bool, Error> {
    let encoding = kwargs
        .get::<Option<String>>("encoding")?
        .or(encoding)
        .unwrap_or_else(|| "utf-8".to_string());
    let urlsafe = kwargs.get::<Option<bool>>("urlsafe")?.unwrap_or(false);
    kwargs.assert_all_used()?;
    if !encoding.eq_ignore_ascii_case("utf-8") && !encoding.eq_ignore_ascii_case("utf8") {
        return Err(invalid(format!(
            "{filter}: the encoding '{encoding}' is not supported, only utf-8 is"
        )));
    }
    Ok(urlsafe)
}

/// `base64.b64decode`, then the bytes read as UTF-8 text; `urlsafe` reads `-` and `_` for `+`
/// and `/`. Called without `validate`, Python's decoder skips whitespace, so this does too.
fn b64decode(value: Value, encoding: Option<String>, kwargs: Kwargs) -> Result<String, Error> {
    let urlsafe = b64_options("b64decode", encoding, &kwargs)?;
    let mut text = text_of(&value);
    text.retain(|c| !c.is_ascii_whitespace());
    if urlsafe {
        text = text.replace('-', "+").replace('_', "/");
    }
    let bytes = b64_decode(&text).map_err(|e| invalid(format!("b64decode: {e}")))?;
    String::from_utf8(bytes)
        .map_err(|e| invalid(format!("b64decode: the decoded bytes are not UTF-8: {e}")))
}

fn b64encode(value: Value, encoding: Option<String>, kwargs: Kwargs) -> Result<String, Error> {
    let urlsafe = b64_options("b64encode", encoding, &kwargs)?;
    let out = b64_encode(text_of(&value).as_bytes());
    Ok(if urlsafe {
        out.replace('+', "-").replace('/', "_")
    } else {
        out
    })
}

/// The port of `comment` in ansible-core's `plugins/filter/core.py`, parameter for parameter:
/// each style gives a decoration and, for the block styles, a beginning and an end; any of
/// them, and the prefix and postfix lines built from the decoration, can be set by keyword.
fn comment(text: String, style: Option<String>, kwargs: Kwargs) -> Result<String, Error> {
    let style = kwargs
        .get::<Option<String>>("style")?
        .or(style)
        .unwrap_or_else(|| "plain".to_string());
    let (beginning, decoration, end) = match style.as_str() {
        "plain" => ("", "# ", ""),
        "erlang" => ("", "% ", ""),
        "c" => ("", "// ", ""),
        "cblock" => ("/*", " * ", " */"),
        "xml" => ("<!--", " - ", "-->"),
        _ => return Err(invalid(format!("Invalid style '{style}'."))),
    };
    let get = |key: &str, default: &str| -> Result<String, Error> {
        Ok(kwargs
            .get::<Option<String>>(key)?
            .unwrap_or_else(|| default.to_string()))
    };
    let decoration = get("decoration", decoration)?;
    let prepostfix = decoration.trim_end().to_string();
    let newline = get("newline", "\n")?;
    let beginning = get("beginning", beginning)?;
    let end = get("end", end)?;
    let prefix = get("prefix", &prepostfix)?;
    let postfix = get("postfix", &prepostfix)?;
    let count = |key: &str| -> Result<usize, Error> {
        Ok(kwargs.get::<Option<i64>>(key)?.unwrap_or(1).max(0) as usize)
    };
    let prefix_count = count("prefix_count")?;
    let postfix_count = count("postfix_count")?;
    kwargs.assert_all_used()?;

    let mut out = String::new();
    if !beginning.is_empty() {
        out.push_str(&beginning);
        out.push_str(&newline);
    }
    if !prefix.is_empty() {
        let line = if prefix == newline {
            newline.clone()
        } else {
            format!("{prefix}{newline}")
        };
        out.push_str(&line.repeat(prefix_count));
    }
    // Each line gets the decoration, and a line that is nothing but the decoration loses the
    // decoration's trailing space.
    let body = format!(
        "{decoration}{}",
        text.replace(&newline, &format!("{newline}{decoration}"))
    );
    out.push_str(&body.replace(
        &format!("{decoration}{newline}"),
        &format!("{}{newline}", decoration.trim_end()),
    ));
    for _ in 0..postfix_count {
        out.push_str(&newline);
        out.push_str(&postfix);
    }
    if !end.is_empty() {
        out.push_str(&newline);
        out.push_str(&end);
    }
    Ok(out)
}

#[derive(Clone, Copy)]
enum SetFilter {
    Difference,
    Intersect,
    Union,
}

/// `difference`, `intersect` and `union` from `mathstuff.py`, each element once. The reference
/// builds a Python `set`, whose order is its own: small integers happen to come out sorted,
/// strings in an order that changes with `PYTHONHASHSEED`. This sorts when every element is an
/// integer, which is what the reference printed on every integer case measured, and keeps the
/// order of first appearance otherwise, which is the reference's own fallback for elements it
/// cannot hash.
fn set_filter(a: &Value, b: &Value, which: SetFilter) -> Result<Value, Error> {
    let a: Vec<Value> = a.try_iter()?.collect();
    let b: Vec<Value> = b.try_iter()?.collect();
    let candidates: Vec<Value> = match which {
        SetFilter::Difference => a.into_iter().filter(|x| !b.contains(x)).collect(),
        SetFilter::Intersect => a.into_iter().filter(|x| b.contains(x)).collect(),
        SetFilter::Union => a.into_iter().chain(b).collect(),
    };
    let mut out: Vec<Value> = Vec::new();
    for x in candidates {
        if !out.contains(&x) {
            out.push(x);
        }
    }
    if out.iter().all(Value::is_integer) {
        out.sort();
    }
    Ok(Value::from(out))
}

/// `flatten(levels=None, skip_nulls=True)`: nested lists spliced in, `levels` deep at most,
/// and `None`, `'None'` and `'null'` dropped while `skip_nulls` holds.
fn flatten(value: Value, levels: Option<i64>, kwargs: Kwargs) -> Result<Value, Error> {
    let levels = kwargs.get::<Option<i64>>("levels")?.or(levels);
    let skip_nulls = kwargs.get::<Option<bool>>("skip_nulls")?.unwrap_or(true);
    kwargs.assert_all_used()?;
    let serde_json::Value::Array(items) = json(&value) else {
        return Err(invalid("flatten expects a list"));
    };
    Ok(from_json_value(serde_json::Value::Array(flatten_items(
        items, levels, skip_nulls,
    ))))
}

fn flatten_items(
    items: Vec<serde_json::Value>,
    levels: Option<i64>,
    skip_nulls: bool,
) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for element in items {
        if skip_nulls && (element.is_null() || element == "None" || element == "null") {
            continue;
        }
        match (element, levels) {
            (serde_json::Value::Array(inner), None) => {
                out.extend(flatten_items(inner, None, skip_nulls));
            }
            (serde_json::Value::Array(inner), Some(n)) if n >= 1 => {
                out.extend(flatten_items(inner, Some(n - 1), skip_nulls));
            }
            (other, _) => out.push(other),
        }
    }
    out
}

/// One YAML document read by the engine's own reader; anything that is not a string, `None`
/// included, comes back as it is.
fn from_yaml(value: Value) -> Result<Value, Error> {
    let Some(text) = value.as_str() else {
        return Ok(value);
    };
    let docs = crate::yaml::load(text, "from_yaml").map_err(|e| invalid(format!("{e:#}")))?;
    match docs.as_slice() {
        [] => Ok(Value::from(())),
        [doc] => crate::yaml::to_json(doc)
            .map(from_json_value)
            .map_err(|e| invalid(format!("from_yaml: {e:#}"))),
        _ => Err(invalid(
            "from_yaml: expected a single document in the stream",
        )),
    }
}

/// `to_uuid`'s default namespace, ansible-core's own.
const UUID_NAMESPACE_ANSIBLE: &str = "361E6D51-FAEC-444A-9079-341386DA8E2E";

fn hex_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex digits"))
        .collect()
}

/// `uuid.uuid5(namespace, text)`: the SHA-1 of the namespace's sixteen bytes and the text, its
/// first sixteen bytes, version 5 and the RFC 4122 variant. The namespace is read the way
/// `uuid.UUID` reads one: braces, `urn:` and `uuid:` and hyphens dropped, 32 hex digits left.
fn to_uuid(value: Value, namespace: Option<String>, kwargs: Kwargs) -> Result<String, Error> {
    let namespace = kwargs
        .get::<Option<String>>("namespace")?
        .or(namespace)
        .unwrap_or_else(|| UUID_NAMESPACE_ANSIBLE.to_string());
    kwargs.assert_all_used()?;
    let hex = namespace.replace("urn:", "").replace("uuid:", "");
    let hex = hex.trim_matches(['{', '}']).replace('-', "");
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid(format!(
            "Invalid value '{namespace}' for 'namespace': badly formed hexadecimal UUID string"
        )));
    }
    let mut data = hex_bytes(&hex);
    data.extend_from_slice(text_of(&value).as_bytes());
    let mut id = hex_bytes(&sha1_hex(&data));
    id.truncate(16);
    id[6] = (id[6] & 0x0f) | 0x50;
    id[8] = (id[8] & 0x3f) | 0x80;
    let h = id.iter().fold(String::new(), |mut h, b| {
        let _ = write!(h, "{b:02x}");
        h
    });
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    ))
}

/// `shlex.quote`, with `None` read as the empty string: a word made only of `[A-Za-z0-9_]` and
/// `@%+=:,./-` is left alone, anything else is single-quoted with each `'` written `'"'"'`.
fn quote(value: Value) -> String {
    let s = if value.is_none() {
        String::new()
    } else {
        text_of(&value)
    };
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
    {
        return s;
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// `regex_escape(re_type='python')`: Python 3's `re.escape`, which escapes only its special
/// characters, or the POSIX basic set `].[^$*\`.
fn regex_escape(value: Value, re_type: Option<String>, kwargs: Kwargs) -> Result<String, Error> {
    let re_type = kwargs
        .get::<Option<String>>("re_type")?
        .or(re_type)
        .unwrap_or_else(|| "python".to_string());
    kwargs.assert_all_used()?;
    let special = match re_type.as_str() {
        "python" => "()[]{}?*+-|^$\\.&~# \t\n\r\x0b\x0c",
        "posix_basic" => "].[^$*\\",
        "posix_extended" => {
            return Err(invalid(format!(
                "Regex type ({re_type}) not yet implemented"
            )));
        }
        _ => return Err(invalid(format!("Invalid regex type ({re_type})"))),
    };
    let mut out = String::new();
    for c in text_of(&value).chars() {
        if special.contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_back_references_become_regex_crate_references() {
        assert_eq!(python_replacement(r"A\1-\g<name>"), "A${1}-${name}");
        assert_eq!(python_replacement("plain"), "plain");
    }

    /// What would make this red: DEL left as it is, which `json.dumps` writes as `\u007f`, or a
    /// character outside the basic plane written as one escape instead of a surrogate pair.
    #[test]
    fn python_json_escapes_what_ensure_ascii_escapes() {
        // `json.dumps` with its default `ensure_ascii`: DEL, a character past ASCII and one
        // outside the basic plane (a surrogate pair), and the escapes below space as serde writes
        // them, which are Python's as well.
        let v = serde_json::json!("a\u{7f}é\u{1F600}\u{1}");
        assert_eq!(
            python_json(&v, None, false, true, 0),
            concat!(
                "\"a", "\\u007f", "\\u00e9", "\\ud83d", "\\ude00", "\\u0001", "\""
            ),
        );
        assert_eq!(
            python_json(&v, None, false, false, 0),
            "\"a\u{7f}é\u{1F600}\\u0001\""
        );
    }

    /// `to_json` keeps the order the mapping was written in and `to_nice_json` sorts, both
    /// measured against ansible-core 2.19.12. The golden corpus carries the same pair, but
    /// only since its generator stopped sorting every mapping on the way in.
    #[test]
    fn python_json_uses_pythons_separators_indent_and_key_order() {
        let v = serde_json::json!({"b": 1, "a": [1, 2]});
        assert_eq!(
            python_json(&v, None, false, false, 0),
            r#"{"b": 1, "a": [1, 2]}"#
        );
        assert_eq!(
            python_json(&v, Some(4), true, false, 0),
            "{\n    \"a\": [\n        1,\n        2\n    ],\n    \"b\": 1\n}"
        );
    }

    fn render(text: &str, vars: serde_json::Value) -> Result<serde_json::Value, String> {
        super::super::Templar::new(std::env::temp_dir())
            .render(text, vars.as_object().unwrap())
            .map_err(|e| e.0)
    }

    fn text(text: &str) -> String {
        match render(text, serde_json::json!({})).unwrap() {
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        }
    }

    /// Each vector below is a line the reference printed, measured on ansible-core 2.19.12 and
    /// copied as it stands. What would make any of them red: the rule beside the filter it names
    /// (`core.py`, `mathstuff.py`) ported differently, or the filter missing (`unknown filter`).
    #[test]
    fn the_remaining_filters_print_what_the_reference_printed() {
        assert_eq!(
            text("{{ 'YWJj' | b64decode }} {{ 'abc' | b64encode }}"),
            "abc YWJj"
        );
        assert_eq!(
            text(
                "{{ [3,1,2,1] | difference([2]) }} {{ [3,1,2,1] | intersect([1,3]) }} {{ [3,1] | union([1,4]) }}"
            ),
            "[1, 3] [1, 3] [1, 3, 4]"
        );
        assert_eq!(
            text("{{ [1,[2,[3,[4]]]] | flatten }} {{ [1,[2,[3,[4]]]] | flatten(levels=1) }}"),
            "[1, 2, 3, 4] [1, 2, [3, [4]]]"
        );
        assert_eq!(
            text("{{ \"it's a b\" | quote }} {{ '' | quote }} {{ 'plain' | quote }}"),
            r#"'it'"'"'s a b' '' plain"#
        );
        assert_eq!(text("{{ 'a.b*c[d]' | regex_escape }}"), r"a\.b\*c\[d\]");
        assert_eq!(
            text("{{ 'volant' | to_uuid }}"),
            "69744a58-016b-51b4-91fb-2b5aaa59c628"
        );
        assert_eq!(
            text(
                "{{ 'x' | type_debug }} {{ 1 | type_debug }} {{ [1] | type_debug }} {{ {} | type_debug }} {{ none | type_debug }} {{ true | type_debug }} {{ 1.5 | type_debug }}"
            ),
            "str int list dict NoneType bool float"
        );
    }

    /// The three `comment` lines measured on the reference, with the text's newline a real one.
    #[test]
    fn comment_is_the_reference_s_port() {
        let vars = serde_json::json!({"managed": "Ansible managed", "two": "two\nlines", "x": "x"});
        let r = |t: &str| render(t, vars.clone()).unwrap();
        assert_eq!(r("{{ managed | comment }}"), "#\n# Ansible managed\n#");
        assert_eq!(r("{{ two | comment('c') }}"), "//\n// two\n// lines\n//");
        assert_eq!(r("{{ x | comment(decoration='; ') }}"), ";\n; x\n;");
        // The block styles, and the parameters the port reads, from `core.py` itself.
        assert_eq!(r("{{ x | comment('cblock') }}"), "/*\n *\n * x\n *\n */");
        assert_eq!(r("{{ x | comment('xml') }}"), "<!--\n -\n - x\n -\n-->");
        assert_eq!(r("{{ x | comment('erlang') }}"), "%\n% x\n%");
        assert_eq!(
            r("{{ x | comment(prefix='', postfix_count=2, beginning='>>', end='<<') }}"),
            ">>\n# x\n#\n#\n<<"
        );
        let err = render("{{ x | comment('pascal') }}", vars).unwrap_err();
        assert!(err.contains("Invalid style 'pascal'."), "{err}");
    }

    /// Volant's order for the three set filters, where the reference's is a Python `set`'s:
    /// sorted when every element is an integer, which is what the reference happened to print,
    /// and first appearance otherwise, where the reference's own order changes with
    /// `PYTHONHASHSEED`. So the string case compares as a set.
    #[test]
    fn the_set_filters_keep_each_element_once() {
        let r = |t: &str| render(t, serde_json::json!({})).unwrap();
        assert_eq!(
            r("{{ ['b','a','b'] | union(['c','a']) | sort }}"),
            serde_json::json!(["a", "b", "c"])
        );
        assert_eq!(
            r("{{ ['b','a'] | difference(['a']) }}"),
            serde_json::json!(["b"])
        );
        // A list of lists is not hashable, and the reference falls back to the input's order.
        assert_eq!(
            r("{{ [[1], [2]] | intersect([[2]]) }}"),
            serde_json::json!([[2]])
        );
    }

    #[test]
    fn flatten_skips_nulls_unless_told_not_to() {
        let r = |t: &str| render(t, serde_json::json!({})).unwrap();
        assert_eq!(
            r("{{ [1, none, 'None', 'null', [2, none]] | flatten }}"),
            serde_json::json!([1, 2])
        );
        assert_eq!(
            r("{{ [1, none, [2]] | flatten(skip_nulls=false) }}"),
            serde_json::json!([1, null, 2])
        );
        assert_eq!(
            r("{{ [1, [2]] | flatten(levels=0) }}"),
            serde_json::json!([1, [2]])
        );
    }

    #[test]
    fn from_yaml_reads_one_document_with_the_engine_s_reader() {
        let vars = serde_json::json!({"doc": "a: 1\nb: [x, y]", "bad": "a: [1"});
        assert_eq!(
            render("{{ doc | from_yaml }}", vars.clone()).unwrap(),
            serde_json::json!({"a": 1, "b": ["x", "y"]})
        );
        assert!(
            render("{{ bad | from_yaml }}", vars)
                .unwrap_err()
                .contains("from_yaml: invalid YAML"),
        );
        assert_eq!(
            render("{{ none | from_yaml }}", serde_json::json!({})).unwrap(),
            serde_json::Value::Null
        );
    }

    /// `shlex.quote` and `re.escape` beyond the measured vectors: what each leaves alone.
    #[test]
    fn quote_and_regex_escape_follow_python() {
        assert_eq!(
            text("{{ 'a@b%c+d=e:f,g.h/i-j_k' | quote }}"),
            "a@b%c+d=e:f,g.h/i-j_k"
        );
        assert_eq!(text("{{ none | quote }}"), "''");
        assert_eq!(text("{{ 'é' | quote }}"), "'é'");
        assert_eq!(text("{{ 'a b-c_d' | regex_escape }}"), r"a\ b\-c_d");
        assert_eq!(
            text("{{ 'a.b[c]^$*+' | regex_escape('posix_basic') }}"),
            r"a\.b\[c\]\^\$\*+"
        );
        assert!(
            render(
                "{{ 'x' | regex_escape('posix_extended') }}",
                serde_json::json!({})
            )
            .unwrap_err()
            .contains("Regex type (posix_extended) not yet implemented")
        );
    }

    #[test]
    fn b64_and_to_uuid_fail_naming_what_was_wrong() {
        let err = render("{{ 'not base64!' | b64decode }}", serde_json::json!({})).unwrap_err();
        assert!(err.contains("b64decode"), "{err}");
        // Whitespace is skipped, as `base64.b64decode` does without `validate=True`.
        assert_eq!(text("{{ 'YWJj ' | b64decode }}"), "abc");
        assert_eq!(text("{{ ' YW\tJj\n' | b64decode }}"), "abc");
        assert_eq!(text("{{ 'YWI_Pg==' | b64decode(urlsafe=true) }}"), "ab?>");
        assert_eq!(text("{{ 'ab?>' | b64encode(urlsafe=true) }}"), "YWI_Pg==");
        let err = render(
            "{{ 'x' | to_uuid(namespace='bogus') }}",
            serde_json::json!({}),
        )
        .unwrap_err();
        assert!(
            err.contains("Invalid value 'bogus' for 'namespace'"),
            "{err}"
        );
        // The namespace in any spelling `uuid.UUID` reads: braces, `urn:uuid:`, no hyphens.
        assert_eq!(
            text("{{ 'volant' | to_uuid(namespace='{361e6d51faec444a9079341386da8e2e}') }}"),
            "69744a58-016b-51b4-91fb-2b5aaa59c628"
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
