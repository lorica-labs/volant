// SPDX-License-Identifier: GPL-3.0-or-later

//! The type checkers of ansible-core 2.19.12's `module_utils/common/validation.py`, each with
//! the conversion it makes and the sentence it raises when it cannot make it.

use serde_json::{Value, json};

use crate::executor::prepare::display;
use crate::executor::python_type;

/// The value `wanted` reads `value` as, or the reason it cannot. A type the reference has no
/// checker for fails the way calling its missing checker does.
pub(super) fn convert(value: &Value, wanted: &str) -> Result<Value, String> {
    match wanted {
        "str" | "path" => Ok(Value::String(to_str(value))),
        "list" => check_list(value),
        "dict" => check_dict(value),
        "bool" => check_bool(value),
        "int" => check_int(value),
        "float" => check_float(value),
        "raw" => Ok(value.clone()),
        "jsonarg" | "json" => check_jsonarg(value),
        "bytes" => human_to_bytes(value, false)
            .ok_or_else(|| format!("{} cannot be converted to a Byte value", class(value))),
        "bits" => human_to_bytes(value, true)
            .ok_or_else(|| format!("{} cannot be converted to a Bit value", class(value))),
        _ => Err("'NoneType' object is not callable".into()),
    }
}

/// The name `native_type_name` gives a value: its Python type, a `!!binary` value `bytes`.
pub(super) fn native_type(value: &Value) -> &'static str {
    if is_bytes(value) {
        "bytes"
    } else {
        python_type(value)
    }
}

/// The class name of a value as the loader made it: tagged, except `None` and booleans, which
/// cannot carry a tag.
pub(super) fn tagged_name(value: &Value) -> String {
    match native_type(value) {
        plain @ ("bool" | "NoneType") => plain.to_string(),
        native => format!(
            "_AnsibleTagged{}{}",
            native[..1].to_uppercase(),
            &native[1..]
        ),
    }
}

/// `type(value)` as Python prints it.
fn class(value: &Value) -> String {
    let name = tagged_name(value);
    if name.starts_with('_') {
        format!("<class 'ansible.module_utils._internal._datatag.{name}'>")
    } else {
        format!("<class '{name}'>")
    }
}

fn is_bytes(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|s| crate::yaml::binary_text(s).is_some())
}

/// `str(value)`, `None` read as the empty string as `check_type_str` reads it.
fn to_str(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        other => display(other),
    }
}

/// Python's `==` between two values, where `1`, `1.0` and `True` are equal.
pub(super) fn py_eq(a: &Value, b: &Value) -> bool {
    let number = |v: &Value| match v {
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::Number(n) => n.as_f64(),
        _ => None,
    };
    match (number(a), number(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

fn check_list(value: &Value) -> Result<Value, String> {
    match value {
        Value::Array(_) => Ok(value.clone()),
        Value::String(s) if !is_bytes(value) => Ok(json!(s.split(',').collect::<Vec<_>>())),
        Value::Number(_) | Value::Bool(_) => Ok(json!([to_str(value)])),
        _ => Err(format!("{} cannot be converted to a list", class(value))),
    }
}

fn check_dict(value: &Value) -> Result<Value, String> {
    match value {
        Value::Object(_) => Ok(value.clone()),
        Value::String(s) if !is_bytes(value) => dict_from_str(s),
        _ => Err(format!("{} cannot be converted to a dict", class(value))),
    }
}

/// `check_type_dict` on a string: JSON when it opens a brace, else `k1=v1, k2=v2` or
/// `k1=v1 k2=v2` with quotes and backslashes.
pub(super) fn dict_from_str(text: &str) -> Result<Value, String> {
    if text.starts_with('{') {
        return serde_json::from_str(text)
            .map_err(|_| "unable to evaluate string as dictionary".to_string());
    }
    if !text.contains('=') {
        return Err("dictionary requested, could not parse JSON or key=value".into());
    }
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quote = None;
    let mut escaped = false;
    for c in text.trim().chars() {
        if escaped {
            field.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if quote.is_none() && (c == '\'' || c == '"') {
            quote = Some(c);
        } else if quote == Some(c) {
            quote = None;
        } else if quote.is_none() && (c == ',' || c == ' ') {
            if !field.is_empty() {
                fields.push(std::mem::take(&mut field));
            }
        } else {
            field.push(c);
        }
    }
    if !field.is_empty() {
        fields.push(field);
    }
    let mut map = serde_json::Map::new();
    for field in fields {
        let Some((key, value)) = field.split_once('=') else {
            return Err(
                "unable to evaluate string in the \"key=value\" format as dictionary".into(),
            );
        };
        map.insert(key.to_string(), json!(value));
    }
    Ok(Value::Object(map))
}

/// `check_type_bool`, through `boolean()` for a string or a number. The reference lists its
/// booleans in the order of a Python set, which changes from one run to the next.
fn check_bool(value: &Value) -> Result<Value, String> {
    const TRUE: [&str; 6] = ["y", "yes", "on", "1", "true", "t"];
    const FALSE: [&str; 6] = ["n", "no", "off", "0", "false", "f"];
    let parsed = match value {
        Value::Bool(_) => return Ok(value.clone()),
        Value::String(s) if !is_bytes(value) => {
            let s = s.trim().to_lowercase();
            if TRUE.contains(&s.as_str()) {
                Some(true)
            } else if FALSE.contains(&s.as_str()) {
                Some(false)
            } else {
                None
            }
        }
        Value::Number(n) => match n.as_f64() {
            Some(1.0) => Some(true),
            Some(0.0) => Some(false),
            _ => None,
        },
        _ => return Err(format!("{} cannot be converted to a bool", class(value))),
    };
    parsed.map(Value::Bool).ok_or_else(|| {
        format!(
            "The value '{}' is not a valid boolean. Valid booleans include: 0, 1, '0', 't', 'true', 'n', 'off', 'false', 'f', '1', 'no', 'yes', 'y', 'on'",
            to_str(value)
        )
    })
}

/// `check_type_int`: an integer as it is, anything `Decimal` reads without a fractional part as
/// that integer.
fn check_int(value: &Value) -> Result<Value, String> {
    let whole = |f: f64| (f.is_finite() && f.fract() == 0.0).then(|| json!(f as i64));
    let converted = match value {
        Value::Bool(_) => Some(value.clone()),
        Value::Number(n) if !n.is_f64() => Some(value.clone()),
        Value::Number(n) => n.as_f64().and_then(whole),
        Value::String(s) if !is_bytes(value) => {
            let s = s.trim();
            s.parse::<i64>()
                .ok()
                .map(|i| json!(i))
                .or_else(|| s.parse::<f64>().ok().and_then(whole))
        }
        _ => None,
    };
    converted.ok_or_else(|| {
        format!(
            "\"{}\" cannot be converted to an int",
            crate::playbook::python_repr(value)
        )
    })
}

fn check_float(value: &Value) -> Result<Value, String> {
    match value {
        Value::Number(_) | Value::Bool(_) => Ok(value.clone()),
        Value::String(s) if !is_bytes(value) => s
            .trim()
            .parse::<f64>()
            .map(|f| serde_json::Number::from_f64(f).map_or_else(|| value.clone(), Value::Number))
            .map_err(|_| format!("{} cannot be converted to a float", class(value))),
        _ => Err(format!("{} cannot be converted to a float", class(value))),
    }
}

fn check_jsonarg(value: &Value) -> Result<Value, String> {
    match value {
        Value::String(s) => Ok(json!(s.trim())),
        Value::Array(_) | Value::Object(_) => Ok(json!(value.to_string())),
        _ => Err(format!(
            "{} cannot be converted to a json string",
            class(value)
        )),
    }
}

/// `human_to_bytes`: `2K` is 2048, `1Mb` is 1048576 bits; a unit longer than one letter must be
/// the byte one (`KB`, `kilobyte`) or the bit one (`Kb`, `kilobit`) as asked.
fn human_to_bytes(value: &Value, bits: bool) -> Option<Value> {
    // `str()` of a `bytes` value is `b'...'`, which is never a number.
    if is_bytes(value) {
        return None;
    }
    let text = to_str(value);
    let end = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, rest) = text.split_at(end);
    if number.matches('.').count() > 1 || !number.ends_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let number: f64 = number.parse().ok()?;
    let rest = rest.trim_start();
    let unit_end = rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    let (unit, tail) = rest.split_at(unit_end);
    if !tail.trim().is_empty() {
        return None;
    }
    if unit.is_empty() {
        return Some(json!(number.round_ties_even() as i64));
    }
    const RANGES: [(char, i32, &str, &str); 9] = [
        ('B', 0, "byte", "bit"),
        ('K', 10, "kilobyte", "kilobit"),
        ('M', 20, "megabyte", "megabit"),
        ('G', 30, "gigabyte", "gigabit"),
        ('T', 40, "terabyte", "terabit"),
        ('P', 50, "petabyte", "petabit"),
        ('E', 60, "exabyte", "exabit"),
        ('Z', 70, "zetabyte", "zetabit"),
        ('Y', 80, "yottabyte", "yottabit"),
    ];
    let key = unit.chars().next()?.to_ascii_uppercase();
    let &(_, shift, byte_name, bit_name) = RANGES.iter().find(|r| r.0 == key)?;
    if unit.len() > 1 {
        let (name, class) = if bits {
            (bit_name, "b")
        } else {
            (byte_name, "B")
        };
        // The byte range's own short unit is the class letter alone.
        let short = if key == 'B' {
            class.to_string()
        } else {
            format!("{key}{class}")
        };
        if unit.to_lowercase() != name && unit != short {
            return None;
        }
    }
    Some(json!((number * 2f64.powi(shift)).round_ties_even() as i64))
}
