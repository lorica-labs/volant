// SPDX-License-Identifier: GPL-3.0-or-later
//! The dump itself: the emitter state and the node writers.

use minijinja::value::{Kwargs, ValueKind};
use minijinja::{Error, ErrorKind, Value};

use super::scalar::{analyze, is_break, is_collection, is_printable, scalar_text};

pub(super) fn dump(
    value: &Value,
    indent: Option<usize>,
    kwargs: Kwargs,
    default_indent: usize,
    default_flow: Option<bool>,
) -> Result<String, Error> {
    if crate::template::holds_undefined(value) {
        return Err(Error::from(ErrorKind::UndefinedError));
    }
    if super::super::pymethods::bytes::holds(value) {
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
    kwargs.assert_all_used()?;
    // PyYAML's own bounds on the two settings: anything outside them is its default.
    let best_indent = if (2..10).contains(&indent) { indent } else { 2 };
    let best_width = width.filter(|w| *w > best_indent * 2).unwrap_or(80);
    let mut e = Emitter {
        out: String::new(),
        column: 0,
        whitespace: true,
        indention: true,
        indent: None,
        indents: Vec::new(),
        flow_level: 0,
        best_indent,
        best_width,
        flow_style,
        sort_keys,
    };
    // The value itself, not its JSON form: JSON has no `inf` or `nan`, and no integer keys.
    e.node(value, false, false);
    // Document end. libyaml writes no `...` after an open-ended plain root scalar.
    e.write_indent();
    Ok(e.out)
}

enum Style {
    Plain,
    Single,
    Double,
}

struct Emitter {
    out: String,
    column: usize,
    whitespace: bool,
    indention: bool,
    indent: Option<usize>,
    indents: Vec<Option<usize>>,
    flow_level: usize,
    best_indent: usize,
    best_width: usize,
    flow_style: Option<bool>,
    sort_keys: bool,
}

impl Emitter {
    fn node(&mut self, v: &Value, mapping: bool, simple_key: bool) {
        match v.kind() {
            ValueKind::Map => {
                let entries = self.entries(v);
                let only_scalars = || {
                    entries
                        .iter()
                        .all(|(k, v)| !is_collection(k) && !is_collection(v))
                };
                if self.flow(entries.is_empty(), only_scalars) {
                    self.flow_mapping(&entries);
                } else {
                    self.block_mapping(&entries);
                }
            }
            ValueKind::Seq | ValueKind::Iterable => {
                let items: Vec<Value> = v.try_iter().map(Iterator::collect).unwrap_or_default();
                if self.flow(items.is_empty(), || !items.iter().any(is_collection)) {
                    self.flow_sequence(&items);
                } else {
                    self.block_sequence(&items, mapping);
                }
            }
            _ => self.scalar(v, simple_key),
        }
    }

    /// Flow style inside a flow collection, for an empty collection, when asked for, and, with
    /// no style asked for, for a collection that holds scalars only (the representer's
    /// `best_style`).
    fn flow(&self, empty: bool, only_scalars: impl FnOnce() -> bool) -> bool {
        self.flow_level > 0 || empty || self.flow_style.unwrap_or_else(only_scalars)
    }

    fn entries(&self, map: &Value) -> Vec<(Value, Value)> {
        let mut entries: Vec<(Value, Value)> = map
            .try_iter()
            .map(|keys| {
                keys.map(|k| {
                    let v = map.get_item(&k).unwrap_or_default();
                    (k, v)
                })
                .collect()
            })
            .unwrap_or_default();
        if self.sort_keys {
            entries.sort_by(|a, b| a.0.cmp(&b.0));
        }
        entries
    }

    /// libyaml's `yaml_emitter_check_simple_key` for a scalar key: on one line and no longer
    /// than 128 bytes. A string key carries no tag to count, and the empty key is simple.
    fn simple_key(key: &Value) -> bool {
        if is_collection(key) {
            return false;
        }
        let text = scalar_text(key).0;
        let chars: Vec<char> = text.chars().collect();
        text.len() <= 128 && !analyze(&chars).multiline
    }

    fn increase_indent(&mut self, flow: bool, indentless: bool) {
        self.indents.push(self.indent);
        self.indent = match self.indent {
            None if flow => Some(self.best_indent),
            None => Some(0),
            Some(i) if !indentless => Some(i + self.best_indent),
            same => same,
        };
    }

    fn pop_indent(&mut self) {
        self.indent = self.indents.pop().flatten();
    }

    fn flow_sequence(&mut self, items: &[Value]) {
        self.write_indicator("[", true, true, false);
        self.flow_level += 1;
        self.increase_indent(true, false);
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                self.write_indicator(",", false, false, false);
            }
            if self.column > self.best_width {
                self.write_indent();
            }
            self.node(item, false, false);
        }
        self.pop_indent();
        self.flow_level -= 1;
        self.write_indicator("]", false, false, false);
    }

    fn flow_mapping(&mut self, entries: &[(Value, Value)]) {
        self.write_indicator("{", true, true, false);
        self.flow_level += 1;
        self.increase_indent(true, false);
        for (i, (k, v)) in entries.iter().enumerate() {
            if i > 0 {
                self.write_indicator(",", false, false, false);
            }
            if self.column > self.best_width {
                self.write_indent();
            }
            if Self::simple_key(k) {
                self.node(k, true, true);
                self.write_indicator(":", false, false, false);
            } else {
                self.write_indicator("?", true, false, false);
                self.node(k, true, false);
                if self.column > self.best_width {
                    self.write_indent();
                }
                self.write_indicator(":", true, false, false);
            }
            self.node(v, true, false);
        }
        self.pop_indent();
        self.flow_level -= 1;
        self.write_indicator("}", false, false, false);
    }

    /// A sequence that is a mapping's value is not indented under its key.
    fn block_sequence(&mut self, items: &[Value], mapping: bool) {
        let indentless = mapping && !self.indention;
        self.increase_indent(false, indentless);
        for item in items {
            self.write_indent();
            self.write_indicator("-", true, false, true);
            self.node(item, false, false);
        }
        self.pop_indent();
    }

    fn block_mapping(&mut self, entries: &[(Value, Value)]) {
        self.increase_indent(false, false);
        for (k, v) in entries {
            self.write_indent();
            if Self::simple_key(k) {
                self.node(k, true, true);
                self.write_indicator(":", false, false, false);
            } else {
                self.write_indicator("?", true, false, true);
                self.node(k, true, false);
                self.write_indent();
                self.write_indicator(":", true, false, true);
            }
            self.node(v, true, false);
        }
        self.pop_indent();
    }

    fn scalar(&mut self, v: &Value, simple_key: bool) {
        let (text, implicit) = scalar_text(v);
        let chars: Vec<char> = text.chars().collect();
        let a = analyze(&chars);
        let plain_here = if self.flow_level > 0 {
            a.allow_flow_plain
        } else {
            a.allow_block_plain
        };
        let style = if implicit && !(simple_key && (a.empty || a.multiline)) && plain_here {
            Style::Plain
        } else if a.allow_single_quoted && !(simple_key && a.multiline) {
            Style::Single
        } else {
            Style::Double
        };
        self.increase_indent(true, false);
        let split = !simple_key;
        match style {
            Style::Plain => self.write_plain(&chars, split),
            Style::Single => self.write_single_quoted(&chars, split),
            Style::Double => self.write_double_quoted(&chars, split),
        }
        self.pop_indent();
    }

    fn write(&mut self, chars: &[char]) {
        self.column += chars.len();
        self.out.extend(chars);
    }

    fn write_indicator(
        &mut self,
        indicator: &str,
        need_whitespace: bool,
        whitespace: bool,
        indention: bool,
    ) {
        if !(self.whitespace || !need_whitespace) {
            self.out.push(' ');
            self.column += 1;
        }
        self.out.push_str(indicator);
        self.column += indicator.chars().count();
        self.whitespace = whitespace;
        self.indention = self.indention && indention;
    }

    fn write_indent(&mut self) {
        let indent = self.indent.unwrap_or(0);
        if !self.indention || self.column > indent || (self.column == indent && !self.whitespace) {
            self.write_line_break('\n');
        }
        if self.column < indent {
            self.whitespace = true;
            self.out
                .extend(std::iter::repeat_n(' ', indent - self.column));
            self.column = indent;
        }
    }

    fn write_line_break(&mut self, br: char) {
        self.whitespace = true;
        self.indention = true;
        self.column = 0;
        self.out.push(br);
    }

    fn write_plain(&mut self, text: &[char], split: bool) {
        if text.is_empty() {
            return;
        }
        if !self.whitespace {
            self.write(&[' ']);
        }
        self.whitespace = false;
        self.indention = false;
        // A plain scalar never holds a line break (`analyze` refuses one), so only spaces
        // matter: a single one past the width is where the line folds.
        let mut spaces = false;
        let (mut start, mut end) = (0, 0);
        while end <= text.len() {
            let ch = text.get(end).copied();
            if spaces {
                if ch != Some(' ') {
                    if start + 1 == end && self.column > self.best_width && split {
                        self.write_indent();
                        self.whitespace = false;
                        self.indention = false;
                    } else {
                        self.write(&text[start..end]);
                    }
                    start = end;
                }
            } else if ch.is_none_or(|c| c == ' ') {
                self.write(&text[start..end]);
                start = end;
            }
            if let Some(c) = ch {
                spaces = c == ' ';
            }
            end += 1;
        }
    }

    fn write_single_quoted(&mut self, text: &[char], split: bool) {
        self.write_indicator("'", true, false, false);
        let (mut spaces, mut breaks) = (false, false);
        let (mut start, mut end) = (0, 0);
        while end <= text.len() {
            let ch = text.get(end).copied();
            if spaces {
                if ch != Some(' ') {
                    if start + 1 == end
                        && self.column > self.best_width
                        && split
                        && start != 0
                        && end != text.len()
                    {
                        self.write_indent();
                    } else {
                        self.write(&text[start..end]);
                    }
                    start = end;
                }
            } else if breaks {
                if ch.is_none_or(|c| !is_break(c)) {
                    // One empty line per line break: the first break of a run is written twice.
                    if text[start] == '\n' {
                        self.write_line_break('\n');
                    }
                    for &br in &text[start..end] {
                        self.write_line_break(br);
                    }
                    self.write_indent();
                    start = end;
                }
            } else if ch.is_none_or(|c| c == ' ' || c == '\'' || is_break(c)) && start < end {
                self.write(&text[start..end]);
                start = end;
            }
            if ch == Some('\'') {
                self.write(&['\'', '\'']);
                start = end + 1;
            }
            if let Some(c) = ch {
                spaces = c == ' ';
                breaks = is_break(c);
            }
            end += 1;
        }
        self.write_indicator("'", false, false, false);
    }

    /// libyaml's `yaml_emitter_write_double_quoted`: a character that is not printable, a
    /// break, the byte order mark, `"` and `\` are escaped; past the width, the line folds at
    /// a single space, which the fold stands for, with a `\` kept before a second space.
    fn write_double_quoted(&mut self, text: &[char], split: bool) {
        self.write_indicator("\"", true, false, false);
        let mut spaces = false;
        for (i, &c) in text.iter().enumerate() {
            if !is_printable(c) || c == '\u{feff}' || is_break(c) || c == '"' || c == '\\' {
                let data = match c {
                    '\0' => "\\0".to_string(),
                    '\x07' => "\\a".to_string(),
                    '\x08' => "\\b".to_string(),
                    '\t' => "\\t".to_string(),
                    '\n' => "\\n".to_string(),
                    '\x0b' => "\\v".to_string(),
                    '\x0c' => "\\f".to_string(),
                    '\r' => "\\r".to_string(),
                    '\x1b' => "\\e".to_string(),
                    '"' => "\\\"".to_string(),
                    '\\' => "\\\\".to_string(),
                    '\u{85}' => "\\N".to_string(),
                    '\u{2028}' => "\\L".to_string(),
                    '\u{2029}' => "\\P".to_string(),
                    c if u32::from(c) <= 0xff => format!("\\x{:02X}", u32::from(c)),
                    c if u32::from(c) <= 0xffff => format!("\\u{:04X}", u32::from(c)),
                    c => format!("\\U{:08X}", u32::from(c)),
                };
                let data: Vec<char> = data.chars().collect();
                self.write(&data);
                spaces = false;
            } else if c == ' ' {
                if split
                    && !spaces
                    && self.column > self.best_width
                    && i != 0
                    && i + 1 != text.len()
                {
                    self.write_indent();
                    if text.get(i + 1) == Some(&' ') {
                        self.write(&['\\']);
                    }
                } else {
                    self.write(&[' ']);
                }
                spaces = true;
            } else {
                self.write(&[c]);
                spaces = false;
            }
        }
        self.write_indicator("\"", false, false, false);
    }
}
