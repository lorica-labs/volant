// SPDX-License-Identifier: GPL-3.0-or-later
//! `to_yaml` and `to_nice_yaml`: PyYAML's `safe_dump` with `allow_unicode=True`, which is what
//! ansible-core calls. The emitter below is PyYAML's own (`emitter.py`), cut down to what a
//! document of plain data reaches: no anchors, no tags, no block scalars. It keeps PyYAML's state
//! (`column`, `whitespace`, `indention`, the indent stack) and its function names, because the
//! output is decided by those flags byte by byte and a rewrite in another shape drifts from it.

use std::sync::LazyLock;

use minijinja::value::Kwargs;
use minijinja::{Environment, Error, ErrorKind, Value};
use serde_json::Value as Json;

use crate::yaml::{PYYAML_FALSE, PYYAML_TRUE};

pub fn register(env: &mut Environment<'static>) {
    // `to_yaml` keeps PyYAML's default flow style (`None`: a collection of scalars inline),
    // `to_nice_yaml` asks for block style everywhere. Both sort keys.
    env.add_filter("to_yaml", |v: Value, kwargs: Kwargs| {
        dump(&v, None, kwargs, 2, None)
    });
    env.add_filter(
        "to_nice_yaml",
        |v: Value, indent: Option<usize>, kwargs: Kwargs| dump(&v, indent, kwargs, 4, Some(false)),
    );
}

fn dump(
    value: &Value,
    indent: Option<usize>,
    kwargs: Kwargs,
    default_indent: usize,
    default_flow: Option<bool>,
) -> Result<String, Error> {
    if value.is_undefined() {
        return Err(Error::from(ErrorKind::UndefinedError));
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
        open_ended: false,
        indent: None,
        indents: Vec::new(),
        flow_level: 0,
        best_indent,
        best_width,
        flow_style,
        sort_keys,
    };
    let data = serde_json::to_value(value)
        .map_err(|err| Error::new(ErrorKind::InvalidOperation, err.to_string()))?;
    e.node(&data, true, false, false);
    // Document end, then stream end, which closes an open-ended root scalar with `...`.
    e.write_indent();
    if e.open_ended {
        e.write_indicator("...", true, false, false);
        e.write_indent();
    }
    Ok(e.out)
}

/// PyYAML's YAML 1.1 implicit resolvers other than the booleans: int, float, null, merge,
/// value and timestamp, in `resolver.py`'s spelling. A string one of them matches would be read
/// back as something else, so it is never written plain.
static RESOLVED: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(concat!(
        r"^(?:",
        r"[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+",
        r"|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+",
        r"|[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?",
        r"|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN)",
        r"|~|null|Null|NULL|<<|=",
        r"|[0-9]{4}-[0-9]{2}-[0-9]{2}",
        r"|[0-9]{4}-[0-9]{1,2}-[0-9]{1,2}(?:[Tt]|[ \t]+)[0-9]{1,2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]*)?",
        r"(?:[ \t]*(?:Z|[-+][0-9]{1,2}(?::[0-9]{2})?))?",
        r")$"
    ))
    .expect("static pattern")
});

/// Whether PyYAML would resolve this plain string to a type other than `str`: the empty
/// string is null.
fn resolves_elsewhere(s: &str) -> bool {
    s.is_empty() || PYYAML_TRUE.contains(&s) || PYYAML_FALSE.contains(&s) || RESOLVED.is_match(s)
}

fn is_break(c: char) -> bool {
    matches!(c, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

fn is_scalar(v: &Json) -> bool {
    !matches!(v, Json::Array(_) | Json::Object(_))
}

/// Python's `repr` of a float, then PyYAML's `represent_float`: lower case, and `.0` before an
/// exponent that has no decimal point (`1e+17` would not read back as a float).
fn py_float(f: f64) -> String {
    if f.is_nan() {
        return ".nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { ".inf" } else { "-.inf" }.into();
    }
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').expect("exponent form");
    let exp: i32 = exp.parse().expect("exponent");
    let text = if (-4..16).contains(&exp) {
        let s = f.to_string();
        if s.contains('.') { s } else { format!("{s}.0") }
    } else {
        format!(
            "{mantissa}e{}{:02}",
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        )
    };
    if !text.contains('.') && text.contains('e') {
        text.replacen('e', ".0e", 1)
    } else {
        text
    }
}

/// `analyze_scalar`'s answer, reduced to the styles a scalar of plain data can take.
struct Analysis {
    empty: bool,
    multiline: bool,
    allow_flow_plain: bool,
    allow_block_plain: bool,
    allow_single_quoted: bool,
}

fn analyze(s: &[char]) -> Analysis {
    if s.is_empty() {
        return Analysis {
            empty: true,
            multiline: false,
            allow_flow_plain: false,
            allow_block_plain: true,
            allow_single_quoted: true,
        };
    }
    let ws = |c: char| matches!(c, '\0' | ' ' | '\t' | '\r') || is_break(c);
    let (mut block_indicators, mut flow_indicators) = (false, false);
    let (mut line_breaks, mut special_characters) = (false, false);
    let (mut leading_space, mut leading_break) = (false, false);
    let (mut trailing_space, mut trailing_break) = (false, false);
    let (mut break_space, mut space_break) = (false, false);
    let head: String = s.iter().take(3).collect();
    if head == "---" || head == "..." {
        block_indicators = true;
        flow_indicators = true;
    }
    let mut preceded_by_whitespace = true;
    let mut followed_by_whitespace = s.len() == 1 || ws(s[1]);
    let (mut previous_space, mut previous_break) = (false, false);
    let last = s.len() - 1;
    for (index, &ch) in s.iter().enumerate() {
        if index == 0 {
            if "#,[]{}&*!|>'\"%@`".contains(ch) {
                flow_indicators = true;
                block_indicators = true;
            }
            if ch == '?' || ch == ':' {
                flow_indicators = true;
                block_indicators |= followed_by_whitespace;
            }
            if ch == '-' && followed_by_whitespace {
                flow_indicators = true;
                block_indicators = true;
            }
        } else {
            if ",?[]{}".contains(ch) {
                flow_indicators = true;
            }
            if ch == ':' {
                flow_indicators = true;
                block_indicators |= followed_by_whitespace;
            }
            if ch == '#' && preceded_by_whitespace {
                flow_indicators = true;
                block_indicators = true;
            }
        }
        if is_break(ch) {
            line_breaks = true;
        }
        if !(ch == '\n' || ('\x20'..='\x7e').contains(&ch)) {
            // `allow_unicode=True`: printable non-ASCII is written as it is.
            let unicode = (ch == '\u{85}'
                || ('\u{a0}'..='\u{d7ff}').contains(&ch)
                || ('\u{e000}'..='\u{fffd}').contains(&ch)
                || ('\u{10000}'..'\u{10ffff}').contains(&ch))
                && ch != '\u{feff}';
            special_characters |= !unicode;
        }
        if ch == ' ' {
            leading_space |= index == 0;
            trailing_space |= index == last;
            break_space |= previous_break;
            previous_space = true;
            previous_break = false;
        } else if is_break(ch) {
            leading_break |= index == 0;
            trailing_break |= index == last;
            space_break |= previous_space;
            previous_space = false;
            previous_break = true;
        } else {
            previous_space = false;
            previous_break = false;
        }
        preceded_by_whitespace = ws(ch);
        followed_by_whitespace = index + 2 >= s.len() || ws(s[index + 2]);
    }
    let mut a = Analysis {
        empty: false,
        multiline: line_breaks,
        allow_flow_plain: true,
        allow_block_plain: true,
        allow_single_quoted: true,
    };
    if leading_space || leading_break || trailing_space || trailing_break {
        a.allow_flow_plain = false;
        a.allow_block_plain = false;
    }
    if break_space || space_break || special_characters {
        a.allow_flow_plain = false;
        a.allow_block_plain = false;
        a.allow_single_quoted = false;
    }
    if line_breaks {
        a.allow_flow_plain = false;
        a.allow_block_plain = false;
    }
    if flow_indicators {
        a.allow_flow_plain = false;
    }
    if block_indicators {
        a.allow_block_plain = false;
    }
    a
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
    open_ended: bool,
    indent: Option<usize>,
    indents: Vec<Option<usize>>,
    flow_level: usize,
    best_indent: usize,
    best_width: usize,
    flow_style: Option<bool>,
    sort_keys: bool,
}

impl Emitter {
    fn node(&mut self, v: &Json, root: bool, mapping: bool, simple_key: bool) {
        match v {
            Json::Array(items) => {
                if self.flow(items.is_empty(), || items.iter().all(is_scalar)) {
                    self.flow_sequence(items);
                } else {
                    self.block_sequence(items, mapping);
                }
            }
            Json::Object(map) => {
                if self.flow(map.is_empty(), || map.values().all(is_scalar)) {
                    self.flow_mapping(map);
                } else {
                    self.block_mapping(map);
                }
            }
            _ => self.scalar(v, root, simple_key),
        }
    }

    /// Flow style inside a flow collection, for an empty collection, when asked for, and, with
    /// no style asked for, for a collection that holds scalars only (the representer's
    /// `best_style`).
    fn flow(&self, empty: bool, only_scalars: impl FnOnce() -> bool) -> bool {
        self.flow_level > 0 || empty || self.flow_style.unwrap_or_else(only_scalars)
    }

    fn entries<'a>(&self, map: &'a serde_json::Map<String, Json>) -> Vec<(&'a String, &'a Json)> {
        let mut entries: Vec<_> = map.iter().collect();
        if self.sort_keys {
            entries.sort_by(|a, b| a.0.cmp(b.0));
        }
        entries
    }

    /// `check_simple_key` for a string key: shorter than 128 with its `!!str` tag, not empty
    /// and on one line.
    fn simple_key(key: &str) -> bool {
        let chars: Vec<char> = key.chars().collect();
        let a = analyze(&chars);
        chars.len() + "!!str".len() < 128 && !a.empty && !a.multiline
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

    fn flow_sequence(&mut self, items: &[Json]) {
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
            self.node(item, false, false, false);
        }
        self.pop_indent();
        self.flow_level -= 1;
        self.write_indicator("]", false, false, false);
    }

    fn flow_mapping(&mut self, map: &serde_json::Map<String, Json>) {
        self.write_indicator("{", true, true, false);
        self.flow_level += 1;
        self.increase_indent(true, false);
        for (i, (k, v)) in self.entries(map).into_iter().enumerate() {
            if i > 0 {
                self.write_indicator(",", false, false, false);
            }
            if self.column > self.best_width {
                self.write_indent();
            }
            let key = Json::String(k.clone());
            if Self::simple_key(k) {
                self.node(&key, false, true, true);
                self.write_indicator(":", false, false, false);
            } else {
                self.write_indicator("?", true, false, false);
                self.node(&key, false, true, false);
                if self.column > self.best_width {
                    self.write_indent();
                }
                self.write_indicator(":", true, false, false);
            }
            self.node(v, false, true, false);
        }
        self.pop_indent();
        self.flow_level -= 1;
        self.write_indicator("}", false, false, false);
    }

    /// A sequence that is a mapping's value is not indented under its key.
    fn block_sequence(&mut self, items: &[Json], mapping: bool) {
        let indentless = mapping && !self.indention;
        self.increase_indent(false, indentless);
        for item in items {
            self.write_indent();
            self.write_indicator("-", true, false, true);
            self.node(item, false, false, false);
        }
        self.pop_indent();
    }

    fn block_mapping(&mut self, map: &serde_json::Map<String, Json>) {
        self.increase_indent(false, false);
        for (k, v) in self.entries(map) {
            self.write_indent();
            let key = Json::String(k.clone());
            if Self::simple_key(k) {
                self.node(&key, false, true, true);
                self.write_indicator(":", false, false, false);
            } else {
                self.write_indicator("?", true, false, true);
                self.node(&key, false, true, false);
                self.write_indent();
                self.write_indicator(":", true, false, true);
            }
            self.node(v, false, true, false);
        }
        self.pop_indent();
    }

    fn scalar(&mut self, v: &Json, root: bool, simple_key: bool) {
        let (text, implicit) = match v {
            Json::Null => ("null".to_string(), true),
            Json::Bool(b) => (b.to_string(), true),
            Json::Number(n) => match n.as_f64() {
                Some(f) if n.is_f64() => (py_float(f), true),
                _ => (n.to_string(), true),
            },
            Json::String(s) => (s.clone(), !resolves_elsewhere(s)),
            Json::Array(_) | Json::Object(_) => unreachable!("collections are not scalars"),
        };
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
            Style::Plain => self.write_plain(&chars, split, root),
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
        self.open_ended = false;
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

    fn write_plain(&mut self, text: &[char], split: bool, root: bool) {
        if root {
            self.open_ended = true;
        }
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

    fn write_double_quoted(&mut self, text: &[char], split: bool) {
        self.write_indicator("\"", true, false, false);
        let (mut start, mut end) = (0, 0);
        while end <= text.len() {
            let ch = text.get(end).copied();
            let escaped = ch.is_none_or(|c| {
                matches!(
                    c,
                    '"' | '\\' | '\u{85}' | '\u{2028}' | '\u{2029}' | '\u{feff}'
                ) || !(('\x20'..='\x7e').contains(&c)
                    || ('\u{a0}'..='\u{d7ff}').contains(&c)
                    || ('\u{e000}'..='\u{fffd}').contains(&c))
            });
            if escaped {
                if start < end {
                    self.write(&text[start..end]);
                    start = end;
                }
                if let Some(c) = ch {
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
                    start = end + 1;
                }
            }
            // Past the width, at a space or right after an escape, the line folds with a `\`.
            if 0 < end
                && end + 1 < text.len()
                && (ch == Some(' ') || start >= end)
                && (self.column + end) as isize - start as isize > self.best_width as isize
                && split
            {
                if start < end {
                    self.write(&text[start..end]);
                    start = end;
                }
                self.write(&['\\']);
                self.write_indent();
                self.whitespace = false;
                self.indention = false;
                if text[start] == ' ' {
                    self.write(&['\\']);
                }
            }
            end += 1;
        }
        self.write_indicator("\"", false, false, false);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value, json};

    use super::super::Templar;

    fn dump(filter: &str, value: Value) -> String {
        let mut vars = Map::new();
        vars.insert("v".into(), value);
        match Templar::new(std::env::temp_dir())
            .render(&format!("{{{{ v | {filter} }}}}"), &vars)
            .unwrap()
        {
            Value::String(s) => s,
            other => panic!("not a string: {other}"),
        }
    }

    /// The mapping the reference dumped, written here in an order that is not sorted.
    fn nested() -> Value {
        json!({
            "u": "é",
            "t": true,
            "s": "1",
            "q": "a: b",
            "m": "line1\nline2",
            "f": 1.5,
            "e": "",
            "b": [1, "two", {"c": null}],
            "a": "yes",
        })
    }

    /// Both outputs measured on ansible-core 2.19.12, copied byte for byte: keys sorted, indent
    /// 4 by default, the dashes of a list not indented under their key, `'yes'`, `'1'` and `''`
    /// quoted because YAML 1.1 would read them as something else, `é` left as it is, and a
    /// string holding a newline single-quoted and folded with an empty line for it.
    ///
    /// What would make this red: keys emitted in insertion order (the input above is not
    /// sorted), a list indented under its key, or any quoting rule of the emitter changed.
    #[test]
    fn to_nice_yaml_prints_the_reference_s_bytes() {
        assert_eq!(
            dump("to_nice_yaml", nested()),
            "a: 'yes'\nb:\n- 1\n- two\n-   c: null\ne: ''\nf: 1.5\nm: 'line1\n\n    line2'\nq: 'a: b'\ns: '1'\nt: true\nu: é\n"
        );
        assert_eq!(
            dump("to_nice_yaml(indent=2)", nested()),
            "a: 'yes'\nb:\n- 1\n- two\n- c: null\ne: ''\nf: 1.5\nm: 'line1\n\n  line2'\nq: 'a: b'\ns: '1'\nt: true\nu: é\n"
        );
    }

    /// One string per rule that stops PyYAML writing a string plain. Beyond the two measured
    /// outputs, each line here was checked against PyYAML 6.0.3's `safe_dump` with
    /// `allow_unicode=True, default_flow_style=False, indent=4`.
    #[test]
    fn a_string_is_quoted_exactly_when_pyyaml_quotes_it() {
        let cases: &[(&str, &str)] = &[
            // YAML 1.1 would read these as a boolean, an integer, a float, null, a timestamp,
            // a merge key or a value key.
            ("On", "'On'"),
            ("NO", "'NO'"),
            ("y", "y"),
            ("0x1F", "'0x1F'"),
            ("017", "'017'"),
            ("08", "08"),
            ("1_000", "'1_000'"),
            ("1:30", "'1:30'"),
            ("-1", "'-1'"),
            (".5", "'.5'"),
            ("1.0e+3", "'1.0e+3'"),
            ("1e3", "1e3"),
            (".inf", "'.inf'"),
            ("~", "'~'"),
            ("Null", "'Null'"),
            ("2001-12-14", "'2001-12-14'"),
            ("<<", "'<<'"),
            ("=", "'='"),
            // Indicators: `: ` and ` #` inside, one of the leading indicators, a document mark.
            ("a:b", "a:b"),
            ("a #b", "'a #b'"),
            ("a#b", "a#b"),
            ("- x", "'- x'"),
            ("-x", "-x"),
            ("?x", "?x"),
            ("*x", "'*x'"),
            ("!x", "'!x'"),
            ("%x", "'%x'"),
            ("@x", "'@x'"),
            ("[x", "'[x'"),
            ("{x", "'{x'"),
            ("x,y", "x,y"),
            ("'x", "'''x'"),
            ("\"x", "'\"x'"),
            ("---x", "'---x'"),
            // Leading and trailing space, a tab, a space before a newline, a final newline.
            (" x", "' x'"),
            ("x ", "'x '"),
            ("a\tb", "\"a\\tb\""),
            ("a \nb", "\"a \\nb\""),
            ("a\n", "'a\n\n    '"),
            ("it's", "it's"),
        ];
        for (input, want) in cases {
            assert_eq!(
                dump("to_nice_yaml", json!({ "k": input })),
                format!("k: {want}\n"),
                "{input:?}"
            );
        }
    }

    /// PyYAML folds a plain scalar at a single space once the line has passed 80 columns, and
    /// `width` moves that limit; checked against PyYAML 6.0.3.
    #[test]
    fn a_long_line_folds_at_the_width_pyyaml_uses() {
        let words = ["aaaa"; 20].join(" ");
        assert_eq!(
            dump("to_nice_yaml", json!({ "k": words })),
            format!(
                "k: {}\n    {}\n",
                ["aaaa"; 16].join(" "),
                ["aaaa"; 4].join(" ")
            )
        );
        assert_eq!(
            dump("to_nice_yaml(width=1000)", json!({ "k": words })),
            format!("k: {words}\n")
        );
    }

    /// `to_yaml` keeps PyYAML's default flow style: a collection that holds only scalars is
    /// written inline, the others as blocks, with an indent of 2. A plain scalar at the root is
    /// followed by the document end marker, as PyYAML writes it.
    #[test]
    fn to_yaml_writes_a_collection_of_scalars_inline() {
        assert_eq!(
            dump(
                "to_yaml",
                json!({"b": [1, "two"], "a": 1, "c": {"d": "x", "e": [[1], []]}})
            ),
            "a: 1\nb: [1, two]\nc:\n  d: x\n  e:\n  - [1]\n  - []\n"
        );
        assert_eq!(
            dump("to_yaml", json!({"a": "yes", "b": 1})),
            "{a: 'yes', b: 1}\n"
        );
        assert_eq!(dump("to_yaml", json!("abc")), "abc\n...\n");
        assert_eq!(dump("to_yaml", json!("yes")), "'yes'\n");
        assert_eq!(dump("to_yaml", json!([])), "[]\n");
        assert_eq!(dump("to_nice_yaml", json!({})), "{}\n");
        assert_eq!(
            dump("to_nice_yaml", json!([[1, 2], {"a": []}])),
            "-   - 1\n    - 2\n-   a: []\n"
        );
        assert_eq!(
            dump("to_nice_yaml", json!({"k": 1e17, "l": -0.0, "m": 1e-5})),
            "k: 1.0e+17\nl: -0.0\nm: 1.0e-05\n"
        );
    }
}
