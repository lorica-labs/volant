// SPDX-License-Identifier: GPL-3.0-or-later
//! The filters' options, `canonical` and `default_style` among them: PyYAML's two that change
//! how every scalar is written.

use std::fmt::Write as _;

use minijinja::value::{Kwargs, ValueKind};
use minijinja::{Error, ErrorKind, Value};

use super::super::scalar::{analyze, scalar_text};
use super::{Emitter, emitter, run};

pub(in crate::template::yaml_dump) fn dump(
    value: &Value,
    indent: Option<usize>,
    kwargs: Kwargs,
    default_indent: usize,
    default_flow: Option<bool>,
) -> Result<String, Error> {
    if crate::template::holds_undefined(value) {
        return Err(Error::from(ErrorKind::UndefinedError));
    }
    if crate::template::pymethods::bytes::holds(value) {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            "to_yaml does not write bytes yet",
        ));
    }
    let indent = kwargs
        .get::<Option<usize>>("indent")?
        .or(indent)
        .unwrap_or(default_indent);
    let width = kwargs.get::<Option<usize>>("width")?;
    let flow_style = if kwargs.has("default_flow_style") {
        kwargs.get::<Option<bool>>("default_flow_style")?
    } else {
        default_flow
    };
    let sort_keys = kwargs.get::<Option<bool>>("sort_keys")?.unwrap_or(true);
    let canonical = kwargs.get::<Option<bool>>("canonical")?.unwrap_or(false);
    let style = match kwargs.get::<Option<String>>("default_style")?.as_deref() {
        None | Some("") => None,
        Some(one @ ("|" | "\"" | "'")) => one.chars().next(),
        Some(other) => {
            return Err(Error::new(
                ErrorKind::InvalidOperation,
                format!("to_yaml does not write default_style={other:?} yet"),
            ));
        }
    };
    kwargs.assert_all_used()?;
    if canonical {
        let mut out = String::from("---\n");
        write_canonical(&mut out, value, 0, sort_keys);
        out.push('\n');
        return Ok(out);
    }
    // A styled scalar is not one the representer's `best_style` counts, so a collection holding
    // one is a block unless `default_flow_style` says otherwise.
    let flow_style = flow_style.or(style.map(|_| false));
    let mut e = emitter(indent, width, flow_style, sort_keys, None, false);
    e.style = style;
    Ok(run(value, e))
}

impl Emitter {
    /// A scalar under `default_style`, as PyYAML writes it: the style asked for where libyaml
    /// allows it, double-quoted otherwise, and a tag on a value that is not text, which a quoted
    /// or block scalar no longer implies. Measured on ansible-core 2.19.12: `3` under `|` is
    /// `! |-\n  3` (Ansible's dumper writes the bare `!`), and a mapping key is double-quoted.
    pub(super) fn styled_scalar(&mut self, v: &Value, simple_key: bool, style: char) {
        let (text, _) = scalar_text(v);
        let chars: Vec<char> = text.chars().collect();
        let a = analyze(&chars);
        if tag_of(v).is_some_and(|t| t != "str") {
            if !self.whitespace {
                self.write(&[' ']);
            }
            self.write(&['!']);
            self.whitespace = false;
            self.indention = false;
        }
        self.increase_indent(true, false);
        match style {
            '|' if a.allow_block && self.flow_level == 0 && !simple_key => {
                self.write_literal(&chars);
            }
            '\'' if a.allow_single_quoted && !(simple_key && a.multiline) => {
                self.write_single_quoted(&chars, !simple_key);
            }
            _ => self.write_double_quoted(&chars, !simple_key),
        }
        self.pop_indent();
    }
}

/// The YAML tag of a scalar.
fn tag_of(v: &Value) -> Option<&'static str> {
    Some(match v.kind() {
        ValueKind::Undefined | ValueKind::None => "null",
        ValueKind::Bool => "bool",
        ValueKind::Number if v.is_integer() => "int",
        _ if crate::template::bigint::is_big(v) => "int",
        ValueKind::Number => "float",
        ValueKind::String => "str",
        _ => return None,
    })
}

/// PyYAML's canonical form, measured on its output: every node tagged, every scalar
/// double-quoted, every collection in flow style with one entry a line and a trailing comma.
/// A long scalar is not folded at the width, as PyYAML folds it.
fn write_canonical(out: &mut String, v: &Value, indent: usize, sort_keys: bool) {
    let inner = " ".repeat(indent + 2);
    let outer = " ".repeat(indent);
    match v.kind() {
        ValueKind::Map => {
            let mut keys: Vec<Value> = v.try_iter().map(Iterator::collect).unwrap_or_default();
            if sort_keys {
                keys.sort();
            }
            if keys.is_empty() {
                out.push_str("!!map {}");
                return;
            }
            out.push_str("!!map {\n");
            for key in keys {
                let value = v.get_item(&key).unwrap_or_default();
                let _ = write!(out, "{inner}? ");
                write_canonical(out, &key, indent + 2, sort_keys);
                let _ = write!(out, "\n{inner}: ");
                write_canonical(out, &value, indent + 2, sort_keys);
                out.push_str(",\n");
            }
            let _ = write!(out, "{outer}}}");
        }
        ValueKind::Seq | ValueKind::Iterable => {
            let items: Vec<Value> = v.try_iter().map(Iterator::collect).unwrap_or_default();
            if items.is_empty() {
                out.push_str("!!seq []");
                return;
            }
            out.push_str("!!seq [\n");
            for item in items {
                out.push_str(&inner);
                write_canonical(out, &item, indent + 2, sort_keys);
                out.push_str(",\n");
            }
            let _ = write!(out, "{outer}]");
        }
        _ => {
            let (text, _) = scalar_text(v);
            let tag = tag_of(v).unwrap_or("str");
            let _ = write!(out, "!!{tag} \"{}\"", escaped(&text));
        }
    }
}

/// The text of a double-quoted scalar, escaped as PyYAML escapes it with `allow_unicode`.
fn escaped(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\0' => out.push_str("\\0"),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{b}' => out.push_str("\\v"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            '\u{1b}' => out.push_str("\\e"),
            '\u{85}' => out.push_str("\\N"),
            '\u{a0}' => out.push_str("\\_"),
            '\u{2028}' => out.push_str("\\L"),
            '\u{2029}' => out.push_str("\\P"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                let _ = write!(out, "\\x{:02X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}
