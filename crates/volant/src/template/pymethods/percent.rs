// SPDX-License-Identifier: GPL-3.0-or-later
//! The `%` operator: Python's printf-style formatting of a text (`'%s-%d' % (a, 3)`) and the
//! modulo of two numbers.

use minijinja::value::{Kwargs, Rest, ValueKind};
use minijinja::{Error, ErrorKind, Value};

use super::args::{raise, type_of};
use super::format::{ascii, float, repr, to_str};
use super::limits::check;

/// `lhs % rhs`. `tuple` says that the right side was written out as a tuple or a list; a tuple
/// a variable holds is one as well.
pub(crate) fn percent(lhs: &Value, rhs: &Value, tuple: bool) -> Result<Value, Error> {
    if super::super::holds_undefined(lhs) || super::super::holds_undefined(rhs) {
        return Err(Error::from(ErrorKind::UndefinedError));
    }
    if let Some(text) = lhs.as_str() {
        return interpolate(text, rhs, tuple).map(Value::from);
    }
    modulo(lhs, rhs)
}

/// The `format` filter, which is `value % args` with the arguments written out: a tuple, or
/// a mapping when keywords are given.
pub(crate) fn format_filter(
    value: Value,
    args: Rest<Value>,
    kwargs: Kwargs,
) -> Result<Value, Error> {
    let names: Vec<String> = kwargs.args().map(str::to_string).collect();
    if !names.is_empty() && !args.is_empty() {
        return Err(raise(
            "can't handle positional and keyword arguments at the same time",
        ));
    }
    let text = to_str(&value);
    if names.is_empty() {
        let tuple = Value::from(args.0);
        return interpolate(&text, &tuple, true).map(Value::from);
    }
    let mut pairs = Vec::with_capacity(names.len());
    for name in &names {
        pairs.push((Value::from(name.as_str()), kwargs.get::<Value>(name)?));
    }
    interpolate(&text, &Value::from_pairs(pairs), false).map(Value::from)
}

/// An integer or a float as the numeric operators read it: a bool counts as 0 or 1.
enum Num {
    Int(i128),
    Float(f64),
}

fn number(value: &Value) -> Option<Num> {
    match value.kind() {
        ValueKind::Bool => Some(Num::Int(i128::from(value.is_true()))),
        ValueKind::Number if value.is_integer() => i128::try_from(value.clone()).ok().map(Num::Int),
        ValueKind::Number => f64::try_from(value.clone()).ok().map(Num::Float),
        _ => None,
    }
}

fn as_float(n: &Num) -> f64 {
    match n {
        Num::Int(i) => *i as f64,
        Num::Float(f) => *f,
    }
}

/// Python's `%` on numbers: the result takes the sign of the divisor.
fn modulo(lhs: &Value, rhs: &Value) -> Result<Value, Error> {
    let (Some(a), Some(b)) = (number(lhs), number(rhs)) else {
        return Err(raise(format!(
            "unsupported operand type(s) for %: '{}' and '{}'",
            type_of(lhs),
            type_of(rhs)
        )));
    };
    if let (Num::Int(x), Num::Int(y)) = (&a, &b) {
        if *y == 0 {
            return Err(raise("integer modulo by zero"));
        }
        let r = x.wrapping_rem(*y);
        return Ok(Value::from(if r != 0 && (r < 0) != (*y < 0) {
            r + y
        } else {
            r
        }));
    }
    let (x, y) = (as_float(&a), as_float(&b));
    if y == 0.0 {
        return Err(raise("float modulo by zero"));
    }
    let r = x % y;
    Ok(Value::from(if r != 0.0 && (r < 0.0) != (y < 0.0) {
        r + y
    } else {
        r
    }))
}

/// The flags, width and precision of one conversion.
#[derive(Default, Clone, Copy)]
struct Spec {
    left: bool,
    plus: bool,
    space: bool,
    zero: bool,
    alt: bool,
    width: usize,
    precision: Option<usize>,
}

/// The arguments of a format string: a tuple read one by one, one value, or a mapping.
struct Args<'a> {
    items: Vec<Value>,
    at: usize,
    mapping: Option<&'a Value>,
}

impl Args<'_> {
    fn next(&mut self) -> Result<Value, Error> {
        let item = self
            .items
            .get(self.at)
            .cloned()
            .ok_or_else(|| raise("not enough arguments for format string"))?;
        self.at += 1;
        Ok(item)
    }

    /// The width or precision a `*` takes from the arguments. A width past an index is Python's
    /// `ssize_t` error, and a precision past an `int` is its `int` one.
    fn star(&mut self, precision: bool) -> Result<i64, Error> {
        let v = self.next()?;
        let Some(Num::Int(i)) = number(&v) else {
            return Err(raise("* wants int"));
        };
        let fits = if precision {
            i32::try_from(i).map(i64::from).ok()
        } else {
            i64::try_from(i).ok()
        };
        fits.ok_or_else(|| {
            raise(if precision {
                "Python int too large to convert to C int"
            } else {
                "Python int too large to convert to C ssize_t"
            })
        })
    }
}

fn interpolate(fmt: &str, rhs: &Value, tuple: bool) -> Result<String, Error> {
    let mapping = (rhs.kind() == ValueKind::Map).then_some(rhs);
    let items = if (tuple && rhs.kind() == ValueKind::Seq) || rhs.is_tuple() {
        rhs.try_iter()?.collect()
    } else {
        vec![rhs.clone()]
    };
    let mut args = Args {
        items,
        at: 0,
        mapping,
    };
    let chars: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut at = 0;
    while at < chars.len() {
        if chars[at] != '%' {
            out.push(chars[at]);
            at += 1;
            continue;
        }
        at += 1;
        let keyed = match key_of(&chars, &mut at)? {
            Some(key) => {
                let Some(map) = args.mapping else {
                    return Err(raise("format requires a mapping"));
                };
                let found = map
                    .get_item(&Value::from(key.as_str()))
                    .ok()
                    .filter(|v| !v.is_undefined())
                    .ok_or_else(|| raise(format!("'{key}'")))?;
                // A positional conversion after a keyed one finds nothing left to read.
                args.at = args.items.len();
                Some(found)
            }
            None => None,
        };
        let spec = spec_of(&chars, &mut at, &mut args)?;
        let Some(&conv) = chars.get(at) else {
            return Err(raise("incomplete format"));
        };
        if conv == '%' {
            out.push('%');
            at += 1;
            continue;
        }
        let arg = match keyed {
            Some(v) => v,
            None => args.next()?,
        };
        let piece = convert(conv, &arg, &spec, at)?;
        // Each field is within the limit; many of them together are checked here.
        check(out.len() as u128 + piece.len() as u128)?;
        out.push_str(&piece);
        at += 1;
    }
    if args.mapping.is_none() && args.at < args.items.len() {
        return Err(raise(
            "not all arguments converted during string formatting",
        ));
    }
    Ok(out)
}

/// The `(key)` of a conversion, when there is one.
fn key_of(chars: &[char], at: &mut usize) -> Result<Option<String>, Error> {
    if chars.get(*at) != Some(&'(') {
        return Ok(None);
    }
    let start = *at + 1;
    let mut depth = 1;
    *at += 1;
    while *at < chars.len() && depth > 0 {
        match chars[*at] {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        *at += 1;
    }
    if depth > 0 {
        return Err(raise("incomplete format key"));
    }
    Ok(Some(chars[start..*at - 1].iter().collect()))
}

/// The flags, width, precision and length modifier before the conversion character.
fn spec_of(chars: &[char], at: &mut usize, args: &mut Args<'_>) -> Result<Spec, Error> {
    let mut spec = Spec::default();
    while let Some(&c) = chars.get(*at) {
        match c {
            '-' => spec.left = true,
            '+' => spec.plus = true,
            ' ' => spec.space = true,
            '0' => spec.zero = true,
            '#' => spec.alt = true,
            _ => break,
        }
        *at += 1;
    }
    if chars.get(*at) == Some(&'*') {
        *at += 1;
        let w = args.star(false)?;
        spec.left |= w < 0;
        spec.width = usize::try_from(w.unsigned_abs()).unwrap_or(usize::MAX);
    } else {
        spec.width = digits(chars, at, "width")?.unwrap_or(0);
    }
    if chars.get(*at) == Some(&'.') {
        *at += 1;
        spec.precision = Some(if chars.get(*at) == Some(&'*') {
            *at += 1;
            usize::try_from(args.star(true)?).unwrap_or(0)
        } else {
            digits(chars, at, "precision")?.unwrap_or(0)
        });
    }
    while chars.get(*at).is_some_and(|c| "hlL".contains(*c)) {
        *at += 1;
    }
    check(spec.width as u128)?;
    check(spec.precision.unwrap_or(0) as u128)?;
    Ok(spec)
}

/// The number a width or a precision is written as; more than an index holds is `what` too big.
fn digits(chars: &[char], at: &mut usize, what: &str) -> Result<Option<usize>, Error> {
    let from = *at;
    while chars.get(*at).is_some_and(char::is_ascii_digit) {
        *at += 1;
    }
    if from == *at {
        return Ok(None);
    }
    chars[from..*at]
        .iter()
        .collect::<String>()
        .parse::<i64>()
        .ok()
        .and_then(|n| usize::try_from(n).ok())
        .map(Some)
        .ok_or_else(|| raise(format!("{what} too big")))
}

fn pad(body: &str, spec: &Spec) -> String {
    let len = body.chars().count();
    let fill = " ".repeat(spec.width.saturating_sub(len));
    if spec.left {
        format!("{body}{fill}")
    } else {
        format!("{fill}{body}")
    }
}

/// A number's text: the sign and base prefix first, then the digits, zero-padded to the width
/// when the `0` flag is set and the `-` flag is not.
fn numeric(negative: bool, prefix: &str, body: &str, spec: &Spec) -> String {
    let sign = match (negative, spec.plus, spec.space) {
        (true, _, _) => "-",
        (false, true, _) => "+",
        (false, false, true) => " ",
        _ => "",
    };
    let head = format!("{sign}{prefix}");
    if spec.zero && !spec.left {
        let used = head.chars().count() + body.chars().count();
        let zeros = "0".repeat(spec.width.saturating_sub(used));
        return format!("{head}{zeros}{body}");
    }
    pad(&format!("{head}{body}"), spec)
}

fn convert(conv: char, arg: &Value, spec: &Spec, at: usize) -> Result<String, Error> {
    match conv {
        's' | 'r' | 'a' => {
            let text = match conv {
                's' => to_str(arg),
                'r' => repr(arg),
                _ => ascii(arg),
            };
            let text: String = match spec.precision {
                Some(p) => text.chars().take(p).collect(),
                None => text,
            };
            Ok(pad(&text, spec))
        }
        'c' => {
            let c = match (arg.as_str(), number(arg)) {
                (Some(t), _) if t.chars().count() == 1 => t.chars().next(),
                (None, Some(Num::Int(i))) => Some(
                    u32::try_from(i)
                        .ok()
                        .and_then(char::from_u32)
                        .ok_or_else(|| raise("%c arg not in range(0x110000)"))?,
                ),
                _ => return Err(raise("%c requires int or char")),
            };
            Ok(pad(&c.map(String::from).unwrap_or_default(), spec))
        }
        'd' | 'i' | 'u' => {
            let Some(n) = number(arg) else {
                return Err(raise(format!(
                    "%{conv} format: a real number is required, not {}",
                    type_of(arg)
                )));
            };
            let (negative, text) = match n {
                Num::Int(i) => (i < 0, i.unsigned_abs().to_string()),
                Num::Float(f) if f.is_nan() => {
                    return Err(raise("cannot convert float NaN to integer"));
                }
                Num::Float(f) if f.is_infinite() => {
                    return Err(raise("cannot convert float infinity to integer"));
                }
                Num::Float(f) => (
                    f < 0.0 && f.trunc() != 0.0,
                    format!("{:.0}", f.trunc().abs()),
                ),
            };
            Ok(numeric(negative, "", &min_digits(text, spec), spec))
        }
        'o' | 'x' | 'X' => {
            let Some(Num::Int(i)) = number(arg) else {
                return Err(raise(format!(
                    "%{conv} format: an integer is required, not {}",
                    type_of(arg)
                )));
            };
            let m = i.unsigned_abs();
            let (text, base) = match conv {
                'o' => (format!("{m:o}"), "0o"),
                'x' => (format!("{m:x}"), "0x"),
                _ => (format!("{m:X}"), "0X"),
            };
            let prefix = if spec.alt { base } else { "" };
            Ok(numeric(i < 0, prefix, &min_digits(text, spec), spec))
        }
        'e' | 'E' | 'f' | 'F' | 'g' | 'G' => {
            let Some(n) = number(arg) else {
                return Err(raise(format!("must be real number, not {}", type_of(arg))));
            };
            let f = as_float(&n);
            let negative = f.is_sign_negative() && !f.is_nan();
            let body = if f.is_finite() {
                let a = f.abs();
                let p = spec.precision.unwrap_or(6);
                match conv {
                    'e' | 'E' => float::exp_style(a, p, spec.alt, conv == 'E'),
                    'f' | 'F' => float::fixed(a, p, spec.alt),
                    _ => float::general(a, p, spec.alt, conv == 'G'),
                }
            } else {
                float::special(f.abs(), conv.is_ascii_uppercase())
            };
            // `inf` and `nan` are padded with blanks, never with zeros.
            let spec = Spec {
                zero: spec.zero && f.is_finite(),
                ..*spec
            };
            Ok(numeric(negative, "", &body, &spec))
        }
        other => Err(raise(format!(
            "unsupported format character '{other}' (0x{:02x}) at index {at}",
            u32::from(other)
        ))),
    }
}

/// An integer's digits, padded with zeros to the precision.
fn min_digits(text: String, spec: &Spec) -> String {
    match spec.precision {
        Some(p) if text.len() < p => format!("{}{text}", "0".repeat(p - text.len())),
        _ => text,
    }
}
