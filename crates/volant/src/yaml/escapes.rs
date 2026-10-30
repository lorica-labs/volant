// SPDX-License-Identifier: GPL-3.0-or-later
//! Backslashes in the string literals of a template held as data, which ansible-core keeps as
//! written and the template engine would decode.
//!
//! Measured on ansible-core 2.19.12: in a variable, `{{ "a\nb" | length }}` is 4 and `'\x41'`
//! stays four characters, whether the variable comes from YAML, an INI or TOML inventory, an
//! inventory script or `-e key=value`, while `{% %}`, a bare expression and a template file
//! decode them. Volant doubles those backslashes when it loads the data, so that the engine reads
//! each escape as the two characters written, and undoes it where the text is shown as stored.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

/// A `{% raw %}` block, which Jinja2 does not read: its text stays as written.
static RAW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)\{%[-+]?\s*raw\s*[-+]?%\}.*?\{%[-+]?\s*endraw\s*[-+]?%\}")
        .expect("a valid pattern")
});

/// `text` with the backslashes of the string literals inside its `{{ }}` doubled, outside its
/// `{% raw %}` blocks.
pub(crate) fn literal_escapes(text: &str) -> String {
    if !text.contains('\\') || !text.contains("{{") {
        return text.to_string();
    }
    outside_raw(text, doubled)
}

/// `text` as written before [`literal_escapes`] doubled its backslashes.
pub(crate) fn as_written(text: &str) -> String {
    if !text.contains('\\') || !text.contains("{{") {
        return text.to_string();
    }
    outside_raw(text, halved)
}

/// [`literal_escapes`] on every string of `value`, as data from a source other than YAML gets.
pub(crate) fn escape_strings(value: &mut Value) {
    each_string(value, &literal_escapes);
}

/// [`as_written`] on every string of `value`, for a value shown as stored.
pub(crate) fn written_strings(value: &mut Value) {
    each_string(value, &as_written);
}

/// `value`, a value [`super::to_json`] made, with Volant's own encodings undone: what
/// ansible-core's loader returns for the same text. Template literals keep their backslashes as
/// written, an `!unsafe` string is its text, and a `!vault` value is `VAULT:` and its ciphertext.
/// For comparing the two loaders: see `just yaml-corpus`.
pub fn as_reference(value: &mut Value) {
    each_string(value, &|text| {
        if let Some(plain) = super::unsafe_text(text) {
            plain
        } else if let Some(ciphertext) = super::vault_text(text) {
            format!("VAULT:{}", ciphertext.trim())
        } else {
            as_written(text)
        }
    });
}

fn each_string(value: &mut Value, change: &dyn Fn(&str) -> String) {
    match value {
        Value::String(s) => *s = change(s),
        Value::Array(items) => items.iter_mut().for_each(|v| each_string(v, change)),
        Value::Object(map) => map.values_mut().for_each(|v| each_string(v, change)),
        _ => {}
    }
}

/// `text` with `change` applied to what lies outside its `{% raw %}` blocks.
fn outside_raw(text: &str, change: fn(&str) -> String) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut from = 0;
    for raw in RAW.find_iter(text) {
        out.push_str(&change(&text[from..raw.start()]));
        out.push_str(raw.as_str());
        from = raw.end();
    }
    out.push_str(&change(&text[from..]));
    out
}

/// Where the scan of a text stands: inside `{{ }}`, and inside a string literal there.
struct Scan {
    expression: bool,
    quote: Option<char>,
}

impl Scan {
    /// Copies `c`, and the `{{` or `}}` it opens, to `out` and follows where the text stands.
    /// Returns false for a backslash in a string literal, which the caller copies.
    fn step(
        &mut self,
        c: char,
        chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
        out: &mut String,
    ) -> bool {
        match (self.expression, self.quote, c) {
            (false, _, '{') if chars.peek() == Some(&'{') => {
                chars.next();
                out.push_str("{{");
                self.expression = true;
            }
            (true, None, '}') if chars.peek() == Some(&'}') => {
                chars.next();
                out.push_str("}}");
                self.expression = false;
            }
            (true, None, '"' | '\'') => {
                self.quote = Some(c);
                out.push(c);
            }
            (true, Some(_), '\\') => return false,
            (true, Some(open), _) if c == open => {
                self.quote = None;
                out.push(c);
            }
            _ => out.push(c),
        }
        true
    }
}

/// `\q` (the closing quote or a backslash) becomes `\\\q`, any other `\x` becomes `\\x`: either
/// decodes to the two characters written.
fn doubled(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut chars = text.chars().peekable();
    let mut scan = Scan {
        expression: false,
        quote: None,
    };
    while let Some(c) = chars.next() {
        if scan.step(c, &mut chars, &mut out) {
            continue;
        }
        match chars.next() {
            Some(next) if Some(next) == scan.quote || next == '\\' => {
                out.push_str("\\\\\\");
                out.push(next);
            }
            Some(next) => {
                out.push_str("\\\\");
                out.push(next);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// The inverse of [`doubled`]: `\\\q` is `\q` and `\\x` is `\x`.
fn halved(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut scan = Scan {
        expression: false,
        quote: None,
    };
    while let Some(c) = chars.next() {
        if scan.step(c, &mut chars, &mut out) {
            continue;
        }
        out.push('\\');
        if chars.peek() == Some(&'\\') {
            chars.next();
            if chars.peek() == Some(&'\\') {
                chars.next();
            }
        }
        if let Some(next) = chars.next() {
            out.push(next);
        }
    }
    out
}
