// SPDX-License-Identifier: GPL-3.0-or-later
//! Ansible's filters, tests and lookups on top of MiniJinja's Jinja2 builtins. Each one keeps
//! Ansible's argument names and its edge cases, checked by tests/golden.

use std::fmt::Write;
use std::net::Ipv6Addr;
use std::path::PathBuf;
use std::str::FromStr;

use minijinja::value::{Kwargs, Rest, ValueKind};
use minijinja::{Environment, Error, ErrorKind, Value};

use volant_protocol::encoding::{b64_decode, b64_encode, sha1_hex};

use super::truthy;
use super::{add_filter, add_test};

pub fn register(env: &mut Environment<'static>, base_dir: PathBuf) {
    // minijinja's own filters and tests, registered again so that an undefined input stays
    // undefined through them as well (see `add_filter`), and so that they answer under
    // `ansible.builtin.` the way the reference's Jinja builtins do (`[1] | ansible.builtin.length`
    // is 1, measured on ansible-core 2.19.12). Those this engine replaces are registered below.
    {
        use minijinja::filters as f;
        add_filter(env, "safe", f::safe);
        add_filter(env, "escape", f::escape);
        add_filter(env, "e", f::escape);
        add_filter(env, "lower", f::lower);
        add_filter(env, "upper", f::upper);
        add_filter(env, "title", f::title);
        add_filter(env, "capitalize", f::capitalize);
        add_filter(env, "replace", f::replace);
        add_filter(env, "length", f::length);
        add_filter(env, "count", f::length);
        add_filter(env, "dictsort", f::dictsort);
        add_filter(env, "items", f::items);
        add_filter(env, "reverse", f::reverse);
        add_filter(env, "trim", f::trim);
        add_filter(env, "join", f::join);
        add_filter(env, "lines", f::lines);
        add_filter(env, "round", f::round);
        add_filter(env, "abs", f::abs);
        add_filter(env, "attr", f::attr);
        add_filter(env, "first", f::first);
        add_filter(env, "last", f::last);
        add_filter(env, "min", f::min);
        add_filter(env, "max", f::max);
        add_filter(env, "sort", f::sort);
        add_filter(env, "list", f::list);
        add_filter(env, "string", f::string);
        add_filter(env, "batch", f::batch);
        add_filter(env, "slice", f::slice);
        add_filter(env, "sum", f::sum);
        add_filter(env, "indent", f::indent);
        add_filter(env, "select", f::select);
        add_filter(env, "reject", f::reject);
        add_filter(env, "selectattr", f::selectattr);
        add_filter(env, "rejectattr", f::rejectattr);
        add_filter(env, "map", f::map);
        add_filter(env, "groupby", f::groupby);
        add_filter(env, "unique", f::unique);
        add_filter(env, "chain", f::chain);
        add_filter(env, "zip", f::zip);
        add_filter(env, "pprint", f::pprint);
        add_filter(env, "format", f::format);
    }
    {
        use minijinja::tests as t;
        add_test(env, "undefined", t::is_undefined);
        add_test(env, "defined", t::is_defined);
        add_test(env, "none", t::is_none);
        add_test(env, "safe", t::is_safe);
        add_test(env, "escaped", t::is_safe);
        add_test(env, "boolean", t::is_boolean);
        add_test(env, "odd", t::is_odd);
        add_test(env, "even", t::is_even);
        add_test(env, "divisibleby", t::is_divisibleby);
        add_test(env, "number", t::is_number);
        add_test(env, "integer", t::is_integer);
        add_test(env, "int", t::is_integer);
        add_test(env, "float", t::is_float);
        add_test(env, "string", t::is_string);
        add_test(env, "sequence", t::is_sequence);
        add_test(env, "iterable", t::is_iterable);
        add_test(env, "mapping", t::is_mapping);
        add_test(env, "startingwith", t::is_startingwith);
        add_test(env, "endingwith", t::is_endingwith);
        add_test(env, "lower", t::is_lower);
        add_test(env, "upper", t::is_upper);
        add_test(env, "sameas", t::is_sameas);
        for name in ["eq", "equalto", "=="] {
            add_test(env, name, t::is_eq);
        }
        for name in ["ne", "!="] {
            add_test(env, name, t::is_ne);
        }
        for name in ["lt", "lessthan", "<"] {
            add_test(env, name, t::is_lt);
        }
        for name in ["le", "<="] {
            add_test(env, name, t::is_le);
        }
        for name in ["gt", "greaterthan", ">"] {
            add_test(env, name, t::is_gt);
        }
        for name in ["ge", ">="] {
            add_test(env, name, t::is_ge);
        }
        add_test(env, "in", t::is_in);
        add_test(env, "true", t::is_true);
        add_test(env, "false", t::is_false);
        add_test(env, "filter", t::is_filter);
        add_test(env, "test", t::is_test);
    }

    add_filter(env, "default", default);
    add_filter(env, "d", default);
    add_filter(env, "bool", to_bool);
    add_filter(env, "int", to_int);
    add_filter(env, "float", to_float);
    add_filter(env, "mandatory", mandatory);
    add_filter(env, "ternary", ternary);
    add_filter(env, "combine", combine);
    add_filter(env, "dict2items", dict2items);
    add_filter(env, "items2dict", items2dict);
    add_filter(env, "to_json", to_json);
    add_filter(env, "to_nice_json", to_nice_json);
    add_filter(env, "from_json", from_json);
    add_filter(env, "basename", basename);
    add_filter(env, "dirname", dirname);
    add_filter(env, "split", split);
    add_filter(env, "regex_replace", regex_replace);
    add_filter(env, "regex_search", regex_search);
    add_filter(env, "regex_findall", regex_findall);
    add_filter(env, "b64decode", b64decode);
    add_filter(env, "b64encode", b64encode);
    add_filter(env, "comment", comment);
    add_filter(env, "difference", |a: Value, b: Value| {
        set_filter(&a, &b, SetFilter::Difference)
    });
    add_filter(env, "intersect", |a: Value, b: Value| {
        set_filter(&a, &b, SetFilter::Intersect)
    });
    add_filter(env, "union", |a: Value, b: Value| {
        set_filter(&a, &b, SetFilter::Union)
    });
    add_filter(env, "flatten", flatten);
    add_filter(env, "from_yaml", from_yaml);
    add_filter(env, "to_uuid", to_uuid);
    // Measured on ansible-core 2.19.12: `nope | type_debug` is `UndefinedMarker`.
    add_filter(env, "type_debug", |v: Value| {
        if v.is_undefined() {
            "UndefinedMarker"
        } else {
            super::type_name(&json(&v))
        }
    });
    add_filter(env, "quote", quote);
    add_filter(env, "regex_escape", regex_escape);
    add_filter(env, "extract", extract);
    // Not through `add_filter`: `ansible.utils.ipwrap` is a collection filter, and the reference
    // never exposes it as `ansible.builtin.ipwrap`.
    env.add_filter(
        "ansible.utils.ipwrap",
        super::undefined_through("ipwrap", ipwrap),
    );
    super::yaml_dump::register(env);

    super::tests::register(env);
    add_test(env, "truthy", |v: Value| truthy(&json(&v)));
    add_test(env, "falsy", |v: Value| !truthy(&json(&v)));
    add_test(env, "match", |v: Value, pattern: String, kwargs: Kwargs| {
        regex_test(&v, &pattern, true, kwargs)
    });
    add_test(
        env,
        "search",
        |v: Value, pattern: String, kwargs: Kwargs| regex_test(&v, &pattern, false, kwargs),
    );
    add_test(env, "regex", |v: Value, pattern: String, kwargs: Kwargs| {
        regex_test(&v, &pattern, false, kwargs)
    });
    add_test(env, "contains", |seq: Value, item: Value| {
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

/// `json`, for a filter whose result carries the value on. An undefined value inside it would
/// come out as `null`; measured on ansible-core 2.19.12, `{'k': nope} | dict2items`,
/// `[[nope]] | flatten` and `{'a': 1} | combine({'k': nope})` are undefined reads.
fn data(v: &Value) -> Result<serde_json::Value, Error> {
    if super::holds_undefined(v) {
        return Err(Error::from(ErrorKind::UndefinedError));
    }
    Ok(json(v))
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
    let serde_json::Value::Object(mut out) = data(&first)? else {
        return Err(invalid("|combine expects dictionaries"));
    };
    for other in rest.iter() {
        let serde_json::Value::Object(other) = data(other)? else {
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
    let serde_json::Value::Object(map) = data(&value)? else {
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
    let serde_json::Value::Array(items) = data(&value)? else {
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
        &data(&value)?,
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
        &data(&value)?,
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
    let serde_json::Value::Array(items) = data(&value)? else {
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

/// `extract(item, container, morekeys=None)`, `core.py`'s: `container[item]`, then each of
/// `morekeys` (a list, or one key) in turn. A key that is not there gives an undefined value,
/// and the keys after it are not read, as the reference hands its undefined marker on: `default`
/// and `is defined` see it, and anything else fails as an undefined variable. A `hostvars`
/// container is read through its own object, so a host's untrusted values taint the render
/// here exactly as a bare `hostvars[h]` does.
///
/// `morekeys` is taken as it arrives rather than as an `Option`, which minijinja fills with
/// `None` for an undefined argument as well as for an omitted one: an undefined `morekeys` is a
/// key that cannot be read, so the result is undefined, as the reference's is.
fn extract(item: Value, container: Value, Rest(morekeys): Rest<Value>) -> Result<Value, Error> {
    if morekeys.len() > 1 {
        return Err(Error::from(ErrorKind::TooManyArguments));
    }
    let mut keys = vec![item];
    match morekeys.into_iter().next() {
        Some(more) if more.kind() == ValueKind::Seq => keys.extend(more.try_iter()?),
        Some(more) if !more.is_none() => keys.push(more),
        _ => {}
    }
    let mut value = container;
    for key in &keys {
        if value.is_undefined() {
            break;
        }
        value = value.get_item(key)?;
    }
    Ok(value)
}

/// `ansible.utils.ipwrap`, ported from the collection's own filter (`plugins/filter/ipwrap.py`,
/// read on the dev machine): every IPv6 address, with or without a valid prefix (0 to 128), is
/// bracketed; a string that is not one - a hostname, an IPv4 address or subnet, an out-of-range
/// or non-numeric prefix, the empty string - is left as it is, and a list is wrapped element by
/// element. Measured on ansible-core 2.19.12 (A9):
/// `['192.0.2.1', '2001:db8::1', 'example.org', '192.0.2.0/24', '2001:db8::/64', '']` becomes
/// `["192.0.2.1", "[2001:db8::1]", "example.org", "192.0.2.0/24", "[2001:db8::]/64", ""]`, and a
/// bare integer fails with the reference's own wording, `format`'s second placeholder included:
/// `The filter plugin 'ansible.utils.ipwrap' failed: Unrecognized type <<class 'int'>> for
/// ipwrap filter <value>`.
fn ipwrap(value: Value) -> Result<Value, Error> {
    match value.kind() {
        ValueKind::String | ValueKind::Bool => Ok(ipwrap_scalar(&value)),
        ValueKind::Seq => Ok(Value::from(
            value
                .try_iter()?
                .map(|item| ipwrap_scalar(&item))
                .collect::<Vec<_>>(),
        )),
        _ => Err(invalid(format!(
            "The filter plugin 'ansible.utils.ipwrap' failed: Unrecognized type <<class '{}'>> for ipwrap filter <value>",
            super::type_name(&json(&value))
        ))),
    }
}

/// One value through the filter. The address is parsed on the part before `/`, which is what
/// `std::net::Ipv6Addr::from_str` gets: on a hit, that part is bracketed and the `/prefix` kept
/// outside the brackets. On a miss the value comes back unchanged - the reference's own `except
/// Exception: return value` - and that covers more than "not an IPv6 address at all": `netaddr`
/// builds an `IPNetwork` out of the address and the prefix together, so a prefix past 128 (an
/// IPv6 address has no more bits) or one that is not a plain integer also raises there and is
/// left untouched here, checked against the real filter on the dev machine:
/// `'2001:db8::/129' | ansible.utils.ipwrap` and `'2001:db8::/abc' | ansible.utils.ipwrap` both
/// answer their own input, not a bracketed one.
fn ipwrap_scalar(value: &Value) -> Value {
    let Some(text) = value.as_str() else {
        return value.clone();
    };
    let (address, prefix) = match text.split_once('/') {
        Some((address, prefix)) => (address, Some(prefix)),
        None => (text, None),
    };
    if Ipv6Addr::from_str(address).is_err() {
        return value.clone();
    }
    let Some(prefix) = prefix else {
        return Value::from(format!("[{address}]"));
    };
    match prefix.parse::<u8>() {
        Ok(bits) if bits <= 128 => Value::from(format!("[{address}]/{prefix}")),
        _ => value.clone(),
    }
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

    /// The A9 vectors, copied as measured on ansible-core 2.19.12: an IPv6 address, with or
    /// without a prefix, is bracketed on the part before `/`; a hostname, an IPv4 address or
    /// subnet, and the empty string are left as they are; a list is wrapped element by element;
    /// a bare integer is refused with the reference's own wording, `format`'s second placeholder
    /// (the literal word `value`) included.
    ///
    /// What would make this red: an IPv4 address or a CIDR parsed as if it were IPv6, the
    /// `/prefix` folded inside the brackets instead of kept outside, or the error message not
    /// matching the reference's byte for byte.
    #[test]
    fn ipwrap_wraps_ipv6_and_nothing_else() {
        assert_eq!(
            render(
                "{{ ['192.0.2.1', '2001:db8::1', 'example.org', '192.0.2.0/24', '2001:db8::/64', ''] | ansible.utils.ipwrap }}",
                serde_json::json!({})
            )
            .unwrap(),
            serde_json::json!([
                "192.0.2.1",
                "[2001:db8::1]",
                "example.org",
                "192.0.2.0/24",
                "[2001:db8::]/64",
                ""
            ])
        );
        assert_eq!(
            text("{{ '2001:db8::1' | ansible.utils.ipwrap }}"),
            "[2001:db8::1]"
        );
        assert_eq!(text("{{ 'host1' | ansible.utils.ipwrap }}"), "host1");
        assert_eq!(
            text("{{ '192.0.2.1' | ansible.utils.ipwrap }}"),
            "192.0.2.1"
        );
        let err = render("{{ 42 | ansible.utils.ipwrap }}", serde_json::json!({})).unwrap_err();
        assert!(
            err.contains(
                "The filter plugin 'ansible.utils.ipwrap' failed: Unrecognized type <<class 'int'>> for ipwrap filter <value>"
            ),
            "{err}"
        );
    }

    /// A prefix `netaddr` would refuse building the `IPNetwork` from - past 128 (an IPv6 address
    /// has no more bits) or not a plain integer - leaves the value untouched rather than
    /// bracketing the address anyway. Checked against the real filter on the dev machine:
    /// `'2001:db8::/129'` and `'2001:db8::/abc'` both answer their own input.
    ///
    /// What would make this red: a prefix accepted without checking its range or that it parses,
    /// which would still bracket the address (`[2001:db8::]/129`).
    #[test]
    fn ipwrap_leaves_an_address_alone_when_its_prefix_does_not_parse() {
        assert_eq!(
            text("{{ '2001:db8::/129' | ansible.utils.ipwrap }}"),
            "2001:db8::/129"
        );
        assert_eq!(
            text("{{ '2001:db8::/abc' | ansible.utils.ipwrap }}"),
            "2001:db8::/abc"
        );
        // 128 itself, the last valid one, still wraps.
        assert_eq!(
            text("{{ '2001:db8::/128' | ansible.utils.ipwrap }}"),
            "[2001:db8::]/128"
        );
    }

    /// `ansible.utils.ipwrap` is not one of the aliases `add_filter` gives every registered
    /// filter: the reference never exposes it as `ansible.builtin.ipwrap`, so a role that
    /// misspells it that way must fail here exactly as it does there.
    #[test]
    fn ipwrap_is_not_aliased_under_ansible_builtin() {
        let err = render("{{ 'x' | ansible.builtin.ipwrap }}", serde_json::json!({})).unwrap_err();
        assert!(err.contains("unknown filter"), "{err}");
    }

    /// One expression per filter, test and method the `k3s-io/k3s-ansible` roles and playbooks
    /// call (commit `1a600b6`, every Jinja expression walked with Jinja2's own parser), each
    /// answer printed by ansible-core 2.19.12 on localhost. What would make a row red: the name
    /// missing (`unknown filter`, `unknown test`, `method ... is not available yet`) or its rule
    /// ported differently from the reference's.
    #[test]
    fn every_name_k3s_ansible_calls_answers_what_the_reference_answered() {
        use serde_json::json;
        let rows = [
            (
                "{{ '2001:db8::1' | ansible.utils.ipwrap }}",
                json!("[2001:db8::1]"),
            ),
            ("{{ 'YWJj' | b64decode }}", json!("abc")),
            ("{{ '/a/b/c.tar' | basename }}", json!("c.tar")),
            ("{{ 'yes' | bool }}", json!(true)),
            (
                "{{ {'a': 1} | combine({'b': 2}) }}",
                json!({"a": 1, "b": 2}),
            ),
            ("{{ missing | default('d') }}", json!("d")),
            ("{{ ['x', 'y'] | first }}", json!("x")),
            ("{{ [[1], [1, 2]] | flatten }}", json!([1, 1, 2])),
            ("{{ 'a: 1' | from_yaml }}", json!({"a": 1})),
            ("{{ [1, 2] | length }}", json!(2)),
            ("{{ 'ab' | list }}", json!(["a", "b"])),
            ("{{ [{'a': 1}] | map(attribute='a') | list }}", json!([1])),
            ("{{ '10.0.0.1' | regex_escape }}", json!(r"10\.0\.0\.1")),
            ("{{ 'abc' | regex_replace('b', '') }}", json!("ac")),
            (
                "{{ 'k3s version v1.2.3' | regex_search('v[0-9.]+') }}",
                json!("v1.2.3"),
            ),
            (
                "{{ [{'s': {'e': true}, 'i': 1}, {'s': {'e': false}, 'i': 2}] | selectattr('s.e') | map(attribute='i') | list }}",
                json!([1]),
            ),
            (
                "{{ [{'d': 1}, {}] | selectattr('d', 'defined') | map(attribute='d') | list }}",
                json!([1]),
            ),
            ("{{ 'a,b' | split(',') }}", json!(["a", "b"])),
            ("{{ 1 | string }}", json!("1")),
            ("{{ true | ternary('t', 'f') }}", json!("t")),
            ("{{ {'a': 1} | to_nice_yaml }}", json!("a: 1\n")),
            ("{{ ' x ' | trim }}", json!("x")),
            ("{{ [1, 1, 2] | unique | list }}", json!([1, 2])),
            (
                "{{ ['a', 'b'] | map('extract', {'a': 1, 'b': 2}) | list }}",
                json!([1, 2]),
            ),
            (
                "{{ 'k3s version v1.30.2+k3s1 (abc)'.split(' ')[2] }}",
                json!("v1.30.2+k3s1"),
            ),
            ("{{ 'a b  c'.split() | length }}", json!(3)),
            ("{{ missing is defined }}", json!(false)),
            ("{{ false is false }}", json!(true)),
            ("{{ 'Archlinux' is match('Arch') }}", json!(true)),
            ("{{ 'aarch64' is search('arch') }}", json!(true)),
            ("{{ missing is undefined }}", json!(true)),
            ("{{ '2.19.12' is version('2.15', '>=') }}", json!(true)),
            (
                "{{ '2.19.12' is version_compare('2.15', '>=') }}",
                json!(true),
            ),
            (
                "{{ 'v1.30.2+k3s1' is version('v1.31.0+k3s1', '<') }}",
                json!(true),
            ),
            (
                "{{ 'v1.31.0+k3s1' is version('v1.31.0+k3s1', '<=') }}",
                json!(true),
            ),
            (
                "{{ '6.8.0-45-generic' is version('6.18', '>=') }}",
                json!(false),
            ),
            ("{{ '1.8.7' is version('1.8.5', '<') }}", json!(false)),
        ];
        let failed: Vec<String> = rows
            .iter()
            .filter_map(|(expr, want)| match render(expr, json!({})) {
                Ok(got) if &got == want => None,
                other => Some(format!("{expr} -> {other:?}")),
            })
            .collect();
        assert!(failed.is_empty(), "{failed:#?}");
    }

    /// `extract`'s vectors, each produced by ansible-core 2.19.12 on localhost. What would make
    /// a row red: `morekeys` not walked in order (the list) or iterated when it is one string
    /// key (`'xy'` read as `'xy'` itself, not as `'x'` then `'y'`), a list container not indexed by
    /// position, or a missing key answered with something other than an undefined value, which
    /// `default` and `is defined` see and a bare render refuses.
    #[test]
    fn extract_reads_container_item_then_each_more_key() {
        use serde_json::json;
        let r = |t: &str| render(t, json!({}));
        assert_eq!(
            r("{{ 'a' | extract({'a': {'x': {'y': 5} } }, ['x', 'y']) }}"),
            Ok(json!(5))
        );
        assert_eq!(
            r("{{ 'a' | extract({'a': {'xy': 7} }, 'xy') }}"),
            Ok(json!(7))
        );
        assert_eq!(r("{{ 'a' | extract({'a': 7}, none) }}"), Ok(json!(7)));
        assert_eq!(
            r("{{ [0, 2] | map('extract', ['p', 'q', 'r']) | list }}"),
            Ok(json!(["p", "r"]))
        );
        assert_eq!(
            r("{{ 'z' | extract({'a': 1}) | default('none') }}"),
            Ok(json!("none"))
        );
        assert_eq!(
            r("{{ 'z' | extract({'a': 1}, ['x', 'y']) | default('none') }}"),
            Ok(json!("none"))
        );
        assert_eq!(
            r("{{ 'a' | extract({'a': {'x': 1} }, ['nope', 'y']) | default('none') }}"),
            Ok(json!("none"))
        );
        assert_eq!(
            r("{{ 'z' | extract({'a': 1}) is defined }}"),
            Ok(json!(false))
        );
        // The reference: `object of type 'dict' has no attribute 'z'`, as an undefined variable.
        let err = r("{{ 'z' | extract({'a': 1}) }}").unwrap_err();
        assert!(err.starts_with(super::super::UNDEFINED), "{err}");
        // An undefined `morekeys` is a key that cannot be read, not an omitted argument: the
        // reference fails `'missing' is undefined` bare, and `default` catches it.
        let err = r("{{ 'a' | extract({'a': 1}, missing) }}").unwrap_err();
        assert!(err.starts_with(super::super::UNDEFINED), "{err}");
        assert_eq!(
            r("{{ 'a' | extract({'a': 1}, missing) | default('d') }}"),
            Ok(json!("d"))
        );
        // The reference: `extract() takes from 3 to 4 positional arguments but 5 were given`.
        let err = r("{{ 'a' | extract({'a': 1}, 'x', 'y') }}").unwrap_err();
        assert!(err.contains("too many arguments"), "{err}");
        assert_eq!(
            r("{{ 'a' | ansible.builtin.extract({'a': 3}) }}"),
            Ok(json!(3))
        );
    }

    /// A filter and a registered test still answer under `ansible.builtin.<name>`. Guard: remove
    /// the aliasing this goes through and both renders fail by `unknown filter`/`unknown test`.
    #[test]
    fn a_filter_and_a_test_answer_under_their_ansible_builtin_alias() {
        assert_eq!(
            render(
                "{{ missing | ansible.builtin.default('y') }}",
                serde_json::json!({})
            )
            .unwrap(),
            serde_json::json!("y")
        );
        assert_eq!(
            render(
                "{{ 'a b' is ansible.builtin.match('a') }}",
                serde_json::json!({})
            )
            .unwrap(),
            serde_json::json!(true)
        );
    }
}
