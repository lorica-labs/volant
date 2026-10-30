// SPDX-License-Identifier: GPL-3.0-or-later
//! Python's `ast.literal_eval` for an INI inventory value, followed by the reference INI plugin's
//! conversions (`_parse_recursive_coerce_types_and_tag` in ansible-core 2.19.12): a tuple or a
//! set is a list, bytes are their UTF-8 text, and `...` or a complex number is its text.

use serde_json::{Map, Number, Value};

mod strings;

/// Python's parser refuses a literal nested deeper than this.
const MAX_DEPTH: usize = 200;

/// `text` read as `ast.literal_eval` reads it, or `None` where it raises and the plugin keeps the
/// text, as for `1abc`, `01`, a name or an f-string.
pub(super) fn literal_eval(text: &str) -> Option<Value> {
    let mut parser = Parser {
        text,
        at: 0,
        depth: 0,
    };
    // `literal_eval` strips leading blanks before it parses.
    parser.blank();
    let lit = parser.expr()?;
    parser.blank();
    (parser.at == text.len()).then_some(())?;
    lit.into_json()
}

enum Lit {
    /// In decimal, as Python's integers have no size limit.
    Int(String),
    Float(f64),
    Complex(f64, f64),
    Str(String),
    Bytes(Vec<u8>),
    Bool(bool),
    None,
    Ellipsis,
    List(Vec<Lit>),
    Set(Vec<Lit>),
    Dict(Vec<(Lit, Lit)>),
}

impl Lit {
    /// What Python's hash equality makes one dictionary key or one set item: `1`, `1.0` and
    /// `True` are one. `None` for a value Python cannot hash.
    fn identity(&self) -> Option<String> {
        Some(match self {
            Self::Int(n) => format!("n{n}"),
            Self::Bool(b) => format!("n{}", i32::from(*b)),
            #[allow(clippy::float_cmp)]
            Self::Float(f) if f.is_finite() && f.fract() == 0.0 => format!("n{f:.0}"),
            Self::Float(f) => format!("f{f}"),
            Self::Complex(r, i) => format!("c{r},{i}"),
            Self::Str(s) => format!("s{s}"),
            Self::Bytes(b) => format!("b{b:?}"),
            Self::None => "N".to_string(),
            Self::Ellipsis => "E".to_string(),
            Self::List(_) | Self::Set(_) | Self::Dict(_) => return None,
        })
    }

    fn into_json(self) -> Option<Value> {
        Some(match self {
            Self::Int(n) => Value::Number(n.parse::<Number>().ok()?),
            Self::Float(f) => Value::Number(Number::from_f64(f)?),
            Self::Complex(r, i) => Value::String(complex_repr(r, i)),
            Self::Str(s) => Value::String(s),
            Self::Bytes(b) => Value::String(String::from_utf8(b).ok()?),
            Self::Bool(b) => Value::Bool(b),
            Self::None => Value::Null,
            Self::Ellipsis => Value::String("...".to_string()),
            Self::List(items) | Self::Set(items) => Value::Array(
                items
                    .into_iter()
                    .map(Self::into_json)
                    .collect::<Option<_>>()?,
            ),
            Self::Dict(entries) => {
                let mut map = Map::new();
                for (key, value) in entries {
                    map.insert(key.key_text()?, value.into_json()?);
                }
                Value::Object(map)
            }
        })
    }

    /// A key as the variables hold it, as `crate::yaml` holds a YAML key of the same type.
    fn key_text(&self) -> Option<String> {
        Some(match self {
            Self::Str(s) => s.clone(),
            Self::Int(n) => n.clone(),
            Self::Float(f) => float_text(*f),
            Self::Bool(b) => b.to_string(),
            Self::None => "None".to_string(),
            _ => return None,
        })
    }
}

/// Python's `repr` of a float, close enough for a key or a complex part: `2.0` is `2.0`.
fn float_text(f: f64) -> String {
    let text = f.to_string();
    if f.is_finite() && !text.contains(['.', 'e', 'E']) {
        format!("{text}.0")
    } else {
        text
    }
}

/// Python's `str` of a complex number: `1j`, `(1+2j)`, `(-0-1j)`.
fn complex_repr(r: f64, i: f64) -> String {
    // A part with no fraction prints without `.0`; `-0.0` keeps its sign.
    let part = |f: f64| {
        let text = float_text(f);
        let text = text.strip_suffix(".0").unwrap_or(&text).to_string();
        if f == 0.0 && f.is_sign_negative() {
            "-0".to_string()
        } else {
            text
        }
    };
    if r == 0.0 && r.is_sign_positive() {
        return format!("{}j", part(i));
    }
    let sign = if i.is_sign_negative() { "-" } else { "+" };
    format!("({}{sign}{}j)", part(r), part(i.abs()))
}

struct Parser<'a> {
    text: &'a str,
    at: usize,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }

    fn rest(&self) -> &str {
        &self.text[self.at..]
    }

    /// Skips blanks and a comment, which Python's tokenizer drops.
    fn blank(&mut self) {
        while let Some(b) = self.peek() {
            match b {
                b' ' | b'\t' | b'\x0c' => self.at += 1,
                b'#' => self.at = self.text.len(),
                _ => break,
            }
        }
    }

    fn eat(&mut self, token: &str) -> bool {
        self.blank();
        if self.rest().starts_with(token) {
            self.at += token.len();
            true
        } else {
            false
        }
    }

    fn expr(&mut self) -> Option<Lit> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return None;
        }
        self.blank();
        let lit = self.atom();
        self.depth -= 1;
        lit
    }

    fn atom(&mut self) -> Option<Lit> {
        let b = self.peek()?;
        if self.rest().starts_with("...") {
            self.at += 3;
            return Some(Lit::Ellipsis);
        }
        match b {
            b'[' => {
                self.at += 1;
                Some(Lit::List(self.items(b"]")?))
            }
            b'(' => {
                self.at += 1;
                self.parenthesized()
            }
            b'{' => {
                self.at += 1;
                self.braced()
            }
            b'+' | b'-' | b'0'..=b'9' | b'.' => self.signed(),
            _ if b == b'_' || b.is_ascii_alphabetic() => self.word(),
            b'\'' | b'"' => self.strings(),
            _ => None,
        }
    }

    /// The items up to `close`, a trailing comma allowed.
    fn items(&mut self, close: &[u8]) -> Option<Vec<Lit>> {
        let mut items = Vec::new();
        loop {
            self.blank();
            if self.peek()? == close[0] {
                self.at += 1;
                return Some(items);
            }
            items.push(self.expr()?);
            self.blank();
            match self.peek()? {
                b',' => self.at += 1,
                b if b == close[0] => {}
                _ => return None,
            }
        }
    }

    /// `(1)` is `1`, `()`, `(1,)` and `(1, 2)` are tuples.
    fn parenthesized(&mut self) -> Option<Lit> {
        if self.eat(")") {
            return Some(Lit::List(Vec::new()));
        }
        let first = self.expr()?;
        if self.eat(")") {
            return Some(first);
        }
        if !self.eat(",") {
            return None;
        }
        let mut items = vec![first];
        items.extend(self.items(b")")?);
        Some(Lit::List(items))
    }

    /// A dict or a set; `{}` is an empty dict.
    fn braced(&mut self) -> Option<Lit> {
        if self.eat("}") {
            return Some(Lit::Dict(Vec::new()));
        }
        let first = self.expr()?;
        if self.eat(":") {
            let mut entries: Vec<(Lit, Lit)> = Vec::new();
            let mut key = first;
            loop {
                let value = self.expr()?;
                let id = key.identity()?;
                // A key written twice keeps its first spelling and its last value.
                match entries
                    .iter()
                    .position(|(k, _)| k.identity().as_ref() == Some(&id))
                {
                    Some(at) => entries[at].1 = value,
                    None => entries.push((key, value)),
                }
                if self.eat("}") {
                    return Some(Lit::Dict(entries));
                }
                if !self.eat(",") {
                    return None;
                }
                if self.eat("}") {
                    return Some(Lit::Dict(entries));
                }
                key = self.expr()?;
                if !self.eat(":") {
                    return None;
                }
            }
        }
        let mut items = vec![first];
        if !self.eat("}") {
            if !self.eat(",") {
                return None;
            }
            items.extend(self.items(b"}")?);
        }
        let mut seen = std::collections::HashSet::new();
        let mut unique = Vec::new();
        for item in items {
            if seen.insert(item.identity()?) {
                unique.push(item);
            }
        }
        Some(Lit::Set(unique))
    }

    /// `True`, `False`, `None`, `set()`, or a string with a prefix.
    fn word(&mut self) -> Option<Lit> {
        let len = self
            .rest()
            .bytes()
            .take_while(|b| *b == b'_' || b.is_ascii_alphanumeric())
            .count();
        let word = &self.rest()[..len];
        if matches!(self.rest().as_bytes().get(len), Some(b'\'' | b'"')) {
            return self.strings();
        }
        let lit = match word {
            "True" => Lit::Bool(true),
            "False" => Lit::Bool(false),
            "None" => Lit::None,
            "set" => {
                self.at += len;
                return (self.eat("(") && self.eat(")")).then_some(Lit::Set(Vec::new()));
            }
            _ => return None,
        };
        self.at += len;
        Some(lit)
    }

    /// A number with one sign, or a real number plus or minus an imaginary one.
    fn signed(&mut self) -> Option<Lit> {
        let negative = match self.peek()? {
            b'-' => {
                self.at += 1;
                true
            }
            b'+' => {
                self.at += 1;
                false
            }
            _ => false,
        };
        self.blank();
        let number = match (self.number()?, negative) {
            (n, false) => n,
            (Lit::Int(n), true) if n == "0" => Lit::Int(n),
            (Lit::Int(n), true) => Lit::Int(format!("-{n}")),
            (Lit::Float(f), true) => Lit::Float(-f),
            (Lit::Complex(r, i), true) => Lit::Complex(-r, -i),
            _ => return None,
        };
        let real = match &number {
            Lit::Int(n) => n.parse().ok()?,
            Lit::Float(f) => *f,
            _ => return Some(number),
        };
        let mark = self.at;
        self.blank();
        let minus = match self.peek() {
            Some(b'+') => false,
            Some(b'-') => true,
            _ => {
                self.at = mark;
                return Some(number);
            }
        };
        self.at += 1;
        self.blank();
        let Lit::Complex(r, i) = self.number()? else {
            return None;
        };
        Some(if minus {
            Lit::Complex(real - r, 0.0 - i)
        } else {
            Lit::Complex(real + r, 0.0 + i)
        })
    }

    /// An unsigned number: an integer in any base, a float, or an imaginary number.
    fn number(&mut self) -> Option<Lit> {
        let text = self.text;
        let rest = &text[self.at..];
        let len = number_len(rest);
        let token = &rest[..len];
        if len == 0
            || rest[len..]
                .bytes()
                .next()
                .is_some_and(|b| b == b'_' || b.is_ascii_alphanumeric() || b == b'.')
        {
            return None;
        }
        self.at += len;
        let radix = match token.get(..2) {
            Some("0x" | "0X") => 16,
            Some("0o" | "0O") => 8,
            Some("0b" | "0B") => 2,
            _ => 10,
        };
        if radix != 10 {
            let digits = &token[2..];
            let digits = digits.strip_prefix('_').unwrap_or(digits);
            let value = u128::from_str_radix(&grouped(digits)?, radix).ok()?;
            return Some(Lit::Int(value.to_string()));
        }
        let (body, imaginary) = match token.strip_suffix(['j', 'J']) {
            Some(body) => (body, true),
            None => (token, false),
        };
        let float = body.contains(['.', 'e', 'E']);
        let clean = clean_decimal(body)?;
        if imaginary {
            return Some(Lit::Complex(0.0, clean.parse().ok()?));
        }
        if float {
            return Some(Lit::Float(clean.parse().ok()?));
        }
        // `01` is refused; `00` is zero.
        if clean.len() > 1 && clean.starts_with('0') && clean.bytes().any(|b| b != b'0') {
            return None;
        }
        let digits = clean.trim_start_matches('0');
        Some(Lit::Int(
            if digits.is_empty() { "0" } else { digits }.to_string(),
        ))
    }
}

/// The length of the number token at the start of `rest`: digits, `_`, `.`, an exponent with
/// its sign, a radix prefix's letters and a `j`.
fn number_len(rest: &str) -> usize {
    let bytes = rest.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let b = bytes[at];
        let exponent_sign = matches!(b, b'+' | b'-')
            && at > 0
            && matches!(bytes[at - 1], b'e' | b'E')
            && !matches!(bytes.get(..2), Some(b"0x" | b"0X"));
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || exponent_sign {
            at += 1;
        } else {
            break;
        }
    }
    at
}

/// `digits` without its `_`, which may only sit between two digits.
fn grouped(digits: &str) -> Option<String> {
    let valid = !digits.is_empty()
        && !digits.starts_with('_')
        && !digits.ends_with('_')
        && !digits.contains("__");
    valid.then(|| digits.replace('_', ""))
}

/// A decimal or float body without its `_`, each run of digits grouped as Python allows.
fn clean_decimal(body: &str) -> Option<String> {
    let mut out = String::with_capacity(body.len());
    for part in body.split(['.', 'e', 'E', '+', '-']) {
        if !part.is_empty() {
            let digits = grouped(part)?;
            digits.bytes().all(|b| b.is_ascii_digit()).then_some(())?;
        }
    }
    out.push_str(&body.replace('_', ""));
    (out != "." && !out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests;
