// SPDX-License-Identifier: GPL-3.0-or-later
//! TOML text laid out the way `tomli-w` 1.x writes it, which `ansible-inventory --toml` prints.
//! Not `toml_edit`'s layout: arrays go one element per line with a trailing comma, a table
//! that holds only tables has no header, and keys keep the order they were given.

use std::fmt::Write;

use serde_json::{Map, Value};

const INDENT: &str = "    ";
const MAX_LINE: usize = 100;

/// The document for `table`, or the reference's words for a value TOML cannot hold.
pub fn dumps(table: &Map<String, Value>) -> Result<String, String> {
    let mut out = String::new();
    chunks(table, "", false, &mut out)?;
    Ok(out)
}

fn chunks(
    table: &Map<String, Value>,
    name: &str,
    inside_aot: bool,
    out: &mut String,
) -> Result<(), String> {
    let mut literals = Vec::new();
    let mut tables: Vec<(&str, &Map<String, Value>, bool)> = Vec::new();
    for (key, value) in table {
        match value {
            Value::Object(map) => tables.push((key, map, false)),
            Value::Array(items) if is_aot(items) && !suitable_all(items)? => {
                for item in items {
                    if let Value::Object(map) = item {
                        tables.push((key, map, true));
                    }
                }
            }
            _ => literals.push((key, value)),
        }
    }
    let mut yielded = false;
    if inside_aot || (!name.is_empty() && (!literals.is_empty() || tables.is_empty())) {
        yielded = true;
        if inside_aot {
            let _ = writeln!(out, "[[{name}]]");
        } else {
            let _ = writeln!(out, "[{name}]");
        }
    }
    if !literals.is_empty() {
        yielded = true;
        for (key, value) in literals {
            let _ = writeln!(out, "{} = {}", key_part(key), literal(value, 0)?);
        }
    }
    for (key, map, in_aot) in tables {
        if yielded {
            out.push('\n');
        }
        yielded = true;
        let part = key_part(key);
        let shown = if name.is_empty() {
            part
        } else {
            format!("{name}.{part}")
        };
        chunks(map, &shown, in_aot, out)?;
    }
    Ok(())
}

/// A non-empty array of tables.
fn is_aot(items: &[Value]) -> bool {
    !items.is_empty() && items.iter().all(Value::is_object)
}

/// Whether every table of the array fits on one line, as an inline table.
fn suitable_all(items: &[Value]) -> Result<bool, String> {
    for item in items {
        if let Value::Object(map) = item {
            let rendered = format!("{INDENT}{},", inline_table(map)?);
            if rendered.chars().count() > MAX_LINE || rendered.contains('\n') {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn literal(value: &Value, nest: usize) -> Result<String, String> {
    Ok(match value {
        Value::Null => return Err("Object of type 'NoneType' is not TOML serializable".into()),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => {
            let text = n.to_string();
            if text.contains(['.', 'e', 'E']) {
                n.as_f64().map_or(text, crate::template::python_float_repr)
            } else {
                text
            }
        }
        Value::String(text) if crate::yaml::vault_text(text).is_some() => {
            return Err("Object of type 'EncryptedString' is not TOML serializable".into());
        }
        Value::String(text) => {
            string(&crate::yaml::unsafe_text(text).unwrap_or_else(|| text.clone()))
        }
        Value::Array(items) if items.is_empty() => "[]".into(),
        Value::Array(items) => {
            let inner = INDENT.repeat(nest + 1);
            let rows: Result<Vec<_>, _> = items
                .iter()
                .map(|item| Ok::<_, String>(format!("{inner}{}", literal(item, nest + 1)?)))
                .collect();
            format!("[\n{},\n{}]", rows?.join(",\n"), INDENT.repeat(nest))
        }
        Value::Object(map) => inline_table(map)?,
    })
}

fn inline_table(map: &Map<String, Value>) -> Result<String, String> {
    if map.is_empty() {
        return Ok("{}".into());
    }
    let rows: Result<Vec<_>, _> = map
        .iter()
        .map(|(key, value)| Ok::<_, String>(format!("{} = {}", key_part(key), literal(value, 0)?)))
        .collect();
    Ok(format!("{{ {} }}", rows?.join(", ")))
}

fn key_part(part: &str) -> String {
    let bare = !part.is_empty()
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if bare { part.to_string() } else { string(part) }
}

/// A basic string: quotes, backslashes and control characters other than the tab escaped, the
/// rest as it is.
fn string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '\u{8}' => out.push_str("\\b"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c != '\t' && (c < ' ' || c == '\u{7f}') => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn dump(value: &Value) -> Result<String, String> {
        dumps(value.as_object().unwrap())
    }

    #[test]
    fn layout_follows_tomli_w() {
        let value = json!({
            "name": "a\"b\\c\td\u{1}\u{7f}é\n",
            "ratio": 1.5, "big": 1.0e16, "on": true, "none": [], "empty": {},
            "list": [1, [2, 3]],
            "mixed": {"x": 1, "sub": {"y": 2}, "k.d": "v"},
            "short": [{"a": 1}, {"b": [1]}],
            "long": [{"text": "x".repeat(100)}, {"b": 1}],
        });
        let expected = [
            "name = \"a\\\"b\\\\c\td\\u0001\\u007fé\\n\"",
            "ratio = 1.5",
            "big = 1e+16",
            "on = true",
            "none = []",
            "list = [
    1,
    [
        2,
        3,
    ],
]",
            "
[empty]",
            "
[mixed]
x = 1
\"k.d\" = \"v\"",
            "
[mixed.sub]
y = 2",
            "
[[short]]
a = 1",
            "
[[short]]
b = [
    1,
]",
            &format!(
                "
[[long]]
text = \"{}\"",
                "x".repeat(100)
            ),
            "
[[long]]
b = 1
",
        ]
        .join(
            "
",
        );
        assert_eq!(dump(&value).unwrap(), expected);
    }

    #[test]
    fn null_is_refused_by_name() {
        assert_eq!(
            dump(&json!({"a": {"b": [1, null]}})).unwrap_err(),
            "Object of type 'NoneType' is not TOML serializable"
        );
    }
}
