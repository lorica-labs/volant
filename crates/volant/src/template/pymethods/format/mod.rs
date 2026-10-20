// SPDX-License-Identifier: GPL-3.0-or-later
//! `str.format` and `str.format_map`, and the Python text of a value (`str` and `repr`) that
//! they and the `%` operator print.

use minijinja::value::ValueKind;
use minijinja::{Error, Value};

use super::args::{raise, type_of};
use super::limits::check;

pub(crate) mod float;
pub(crate) mod spec;

/// `str(value)`.
pub(crate) fn to_str(value: &Value) -> String {
    match value.kind() {
        ValueKind::String => value.as_str().unwrap_or_default().to_string(),
        ValueKind::None => "None".to_string(),
        ValueKind::Bool => if value.is_true() { "True" } else { "False" }.to_string(),
        ValueKind::Number if value.is_integer() => value.to_string(),
        ValueKind::Number => float::repr(f64::try_from(value.clone()).unwrap_or(f64::NAN)),
        _ => repr(value),
    }
}

/// `repr(value)`.
pub(crate) fn repr(value: &Value) -> String {
    if let Some(bytes) = super::bytes::of(value) {
        return bytes.python_repr();
    }
    match value.kind() {
        ValueKind::Number if !value.is_integer() => to_str(value),
        ValueKind::Seq if value.is_tuple() => {
            let items: Vec<String> = value
                .try_iter()
                .map(|it| it.map(|v| repr(&v)).collect())
                .unwrap_or_default();
            match items.as_slice() {
                [one] => format!("({one},)"),
                _ => format!("({})", items.join(", ")),
            }
        }
        ValueKind::Seq => format!(
            "[{}]",
            value
                .try_iter()
                .map(|it| it.map(|v| repr(&v)).collect::<Vec<_>>().join(", "))
                .unwrap_or_default()
        ),
        ValueKind::Map => format!(
            "{{{}}}",
            value
                .try_iter()
                .map(|keys| keys
                    .map(|k| {
                        let v = value.get_item(&k).unwrap_or(Value::UNDEFINED);
                        format!("{}: {}", repr(&k), repr(&v))
                    })
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        ),
        _ => crate::template::bigint::to_json(value).map_or_else(
            |_| value.to_string(),
            |json| crate::playbook::python_repr(&json),
        ),
    }
}

/// Whether a tuple is in `value`. Python's `str` of such a value differs from the text of a
/// printed node, which reads every tuple as a list.
pub(crate) fn holds_tuple(value: &Value) -> bool {
    match value.kind() {
        ValueKind::Seq if value.is_tuple() => true,
        ValueKind::Seq => value
            .try_iter()
            .is_ok_and(|mut it| it.any(|v| holds_tuple(&v))),
        ValueKind::Map => value.try_iter().is_ok_and(|mut keys| {
            keys.any(|k| holds_tuple(&k) || value.get_item(&k).is_ok_and(|v| holds_tuple(&v)))
        }),
        _ => false,
    }
}

/// `ascii(value)`: `repr` with every character outside ASCII escaped.
pub(crate) fn ascii(value: &Value) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for c in repr(value).chars() {
        match u32::from(c) {
            0..=0x7f => out.push(c),
            n @ 0x80..=0xff => {
                let _ = write!(out, "\\x{n:02x}");
            }
            n @ 0x100..=0xffff => {
                let _ = write!(out, "\\u{n:04x}");
            }
            n => {
                let _ = write!(out, "\\U{n:08x}");
            }
        }
    }
    out
}

/// Where the fields of a format string read their values from.
pub(super) struct Source<'a> {
    pub(super) pos: &'a [Value],
    pub(super) kw: &'a [(String, Value)],
    /// `format_map`: every name is a key of this mapping.
    pub(super) map: Option<&'a Value>,
}

#[derive(PartialEq, Clone, Copy)]
enum Numbering {
    Unset,
    Auto,
    Manual,
}

struct Fields<'a> {
    src: &'a Source<'a>,
    next: usize,
    mode: Numbering,
}

pub(super) fn format(text: &str, src: &Source<'_>) -> Result<String, Error> {
    let mut fields = Fields {
        src,
        next: 0,
        mode: Numbering::Unset,
    };
    fields.render(text, 2)
}

impl Fields<'_> {
    fn render(&mut self, text: &str, depth: u8) -> Result<String, Error> {
        if depth == 0 {
            return Err(raise("Max string recursion exceeded"));
        }
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::new();
        let mut at = 0;
        while at < chars.len() {
            let c = chars[at];
            at += 1;
            match c {
                '{' if chars.get(at) == Some(&'{') => {
                    out.push('{');
                    at += 1;
                }
                '}' if chars.get(at) == Some(&'}') => {
                    out.push('}');
                    at += 1;
                }
                '}' => return Err(raise("Single '}' encountered in format string")),
                '{' => {
                    let start = at;
                    let mut open = 1;
                    while at < chars.len() && open > 0 {
                        match chars[at] {
                            '{' => open += 1,
                            '}' => open -= 1,
                            _ => {}
                        }
                        at += 1;
                    }
                    if open > 0 {
                        return Err(raise(if start >= chars.len() {
                            "Single '{' encountered in format string"
                        } else {
                            "expected '}' before end of string"
                        }));
                    }
                    let field: String = chars[start..at - 1].iter().collect();
                    let piece = self.field(&field, depth)?;
                    check(out.len() as u128 + piece.len() as u128)?;
                    out.push_str(&piece);
                }
                c => out.push(c),
            }
        }
        Ok(out)
    }

    fn field(&mut self, field: &str, depth: u8) -> Result<String, Error> {
        // The name ends at the first `!` or `:`, but a `:` or `!` inside brackets belongs to the
        // key.
        let mut bracket = false;
        let mut end = field.len();
        for (at, c) in field.char_indices() {
            match c {
                '[' => bracket = true,
                ']' => bracket = false,
                '!' | ':' if !bracket => {
                    end = at;
                    break;
                }
                _ => {}
            }
        }
        let (name, rest) = field.split_at(end);
        let (conversion, spec) = match rest.strip_prefix('!') {
            Some(after) => {
                let mut chars = after.chars();
                let conv = chars.next();
                let tail = chars.as_str();
                match (conv, tail.strip_prefix(':'), tail.is_empty()) {
                    (Some(c), Some(spec), _) => (Some(c), spec),
                    (Some(c), None, true) => (Some(c), ""),
                    (None, ..) => {
                        return Err(raise(
                            "end of string while looking for conversion specifier",
                        ));
                    }
                    _ => return Err(raise("expected ':' after conversion specifier")),
                }
            }
            None => (None, rest.strip_prefix(':').unwrap_or("")),
        };
        let value = self.lookup(name)?;
        let value = match conversion {
            None => value,
            Some('s') => Value::from(to_str(&value)),
            Some('r') => Value::from(repr(&value)),
            Some('a') => Value::from(ascii(&value)),
            Some(c) => return Err(raise(format!("Unknown conversion specifier {c}"))),
        };
        let spec = if spec.contains('{') {
            self.render(spec, depth - 1)?
        } else {
            spec.to_string()
        };
        spec::format_value(&value, &spec)
    }

    fn lookup(&mut self, name: &str) -> Result<Value, Error> {
        let split = name.find(['.', '[']).unwrap_or(name.len());
        let (head, mut tail) = name.split_at(split);
        let mut value = self.first(head)?;
        while !tail.is_empty() {
            if let Some(after) = tail.strip_prefix('.') {
                let end = after.find(['.', '[']).unwrap_or(after.len());
                let attr = &after[..end];
                if attr.is_empty() {
                    return Err(raise("Empty attribute in format string"));
                }
                return Err(raise(format!(
                    "'{}' object has no attribute '{attr}'",
                    type_of(&value)
                )));
            } else if let Some(after) = tail.strip_prefix('[') {
                let Some(end) = after.find(']') else {
                    return Err(raise("Missing ']' in format string"));
                };
                let key = &after[..end];
                value = item(&value, key)?;
                tail = &after[end + 1..];
                if !tail.is_empty() && !tail.starts_with(['.', '[']) {
                    return Err(raise(
                        "Only '.' or '[' may follow ']' in format field specifier",
                    ));
                }
            } else {
                break;
            }
        }
        Ok(value)
    }

    fn first(&mut self, head: &str) -> Result<Value, Error> {
        if let Some(map) = self.src.map {
            if head.is_empty() || head.chars().all(|c| c.is_ascii_digit()) {
                return Err(raise("Format string contains positional fields"));
            }
            return map
                .get_item(&Value::from(head))
                .ok()
                .filter(|v| !v.is_undefined())
                .ok_or_else(|| raise(format!("'{head}'")));
        }
        let number = if head.is_empty() {
            if self.mode == Numbering::Manual {
                return Err(raise(
                    "cannot switch from manual field specification to automatic field numbering",
                ));
            }
            self.mode = Numbering::Auto;
            self.next += 1;
            Some(self.next - 1)
        } else if head.chars().all(|c| c.is_ascii_digit()) {
            if self.mode == Numbering::Auto {
                return Err(raise(
                    "cannot switch from automatic field numbering to manual field specification",
                ));
            }
            self.mode = Numbering::Manual;
            head.parse().ok()
        } else {
            None
        };
        match number {
            Some(n) => self.src.pos.get(n).cloned().ok_or_else(|| {
                raise(format!(
                    "Replacement index {n} out of range for positional args tuple"
                ))
            }),
            None => self
                .src
                .kw
                .iter()
                .find(|(k, _)| k == head)
                .map(|(_, v)| v.clone())
                .ok_or_else(|| raise(format!("'{head}'"))),
        }
    }
}

/// `value[key]` in a format field: an integer key for a list or text, a text key for a mapping.
fn item(value: &Value, key: &str) -> Result<Value, Error> {
    let found = if let Ok(n) = key.parse::<usize>() {
        let by_pos = value
            .get_item(&Value::from(n))
            .ok()
            .filter(|v| !v.is_undefined());
        by_pos.or_else(|| {
            (value.kind() == ValueKind::Map)
                .then(|| {
                    value
                        .get_item(&Value::from(key))
                        .ok()
                        .filter(|v| !v.is_undefined())
                })
                .flatten()
        })
    } else {
        value
            .get_item(&Value::from(key))
            .ok()
            .filter(|v| !v.is_undefined())
    };
    found.ok_or_else(|| match value.kind() {
        ValueKind::Map => raise(format!("'{key}'")),
        ValueKind::String => raise("string index out of range"),
        ValueKind::Seq => raise("list index out of range"),
        _ => raise(format!("'{}' object is not subscriptable", type_of(value))),
    })
}
