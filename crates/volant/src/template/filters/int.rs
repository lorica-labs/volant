// SPDX-License-Identifier: GPL-3.0-or-later
//! The `int` filter, as ansible-core 2.19.12 runs it: `int(value, default=0, base=10)`.
//!
//! A text goes through Python's `int(text, base)` first. When that raises, the filter tries
//! `int(float(text))`, and when that raises too it answers `default`. So an invalid `base`, a
//! `base` that is no integer and a text that holds no number in that base are all the same
//! case: the float reading of the text, or `default`. Any value that is not a text ignores
//! `base`.

use std::fmt::Write;

use minijinja::value::{Kwargs, Rest, ValueKind};
use minijinja::{Error, ErrorKind, Value};

use crate::template::bigint::integer;

/// More digits than this make Python refuse a text in a base that is not a power of two. The
/// filter then answers its default, as for any text `int()` refuses. A base that is a power of
/// two has no such limit in Python; here the limit is the same for every base.
const MAX_DIGITS: usize = 4300;

pub(super) fn to_int(value: Value, args: Rest<Value>, kwargs: Kwargs) -> Result<Value, Error> {
    let (default, base) = arguments(&args, &kwargs)?;
    // Python's `int(inf)` raises `OverflowError`, which the filter does not catch.
    if value.kind() == ValueKind::Number && f64::try_from(value.clone()).is_ok_and(f64::is_infinite)
    {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            "cannot convert float infinity to integer",
        ));
    }
    Ok(match super::json(&value) {
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(Value::from)
            .or_else(|| integer(&n.to_string()))
            .unwrap_or_else(|| float_to_int(n.as_f64().unwrap_or(f64::NAN), default)),
        serde_json::Value::Bool(b) => Value::from(i64::from(b)),
        serde_json::Value::String(text) => base_of(base.as_ref())
            .and_then(|base| parse_int(&text, base))
            .or_else(|| parse_float(&text).map(|f| float_to_int(f, default.clone())))
            .unwrap_or(default),
        _ => default,
    })
}

/// `default` then `base`, by position or by name, as Python's signature takes them.
fn arguments(args: &[Value], kwargs: &Kwargs) -> Result<(Value, Option<Value>), Error> {
    let too_many = |msg: String| Error::new(ErrorKind::TooManyArguments, msg);
    if args.len() > 2 {
        return Err(too_many(format!(
            "do_int() takes from 1 to 3 positional arguments but {} were given",
            args.len() + 1
        )));
    }
    let mut slots: [Option<Value>; 2] = [None, None];
    for (slot, value) in slots.iter_mut().zip(args) {
        *slot = Some(value.clone());
    }
    for (slot, name) in slots.iter_mut().zip(["default", "base"]) {
        if kwargs.has(name) {
            if slot.is_some() {
                return Err(too_many(format!(
                    "do_int() got multiple values for argument '{name}'"
                )));
            }
            *slot = Some(kwargs.get::<Value>(name)?);
        }
    }
    if let Some(name) = kwargs.args().find(|n| !["default", "base"].contains(n)) {
        return Err(too_many(format!(
            "do_int() got an unexpected keyword argument '{name}'"
        )));
    }
    kwargs.assert_all_used()?;
    let [default, base] = slots;
    Ok((default.unwrap_or_else(|| Value::from(0)), base))
}

/// The base Python's `int(text, base)` accepts: 0, or 2 to 36. A Python `int` only: a `bool`
/// counts as 0 or 1, a float, a text or `none` raise `TypeError`. `None` when Python raises.
fn base_of(base: Option<&Value>) -> Option<u32> {
    let Some(base) = base else { return Some(10) };
    let n = match base.kind() {
        ValueKind::Bool => i64::from(base.is_true()),
        ValueKind::Number if base.is_integer() => i64::try_from(base.clone()).ok()?,
        _ => return None,
    };
    u32::try_from(n)
        .ok()
        .filter(|base| *base == 0 || (2..=36).contains(base))
}

/// `int(f)`: the digits of the truncated float, exactly, however many there are. `default` for
/// `inf` and `nan` read from a text: Python raises, and the filter catches it.
fn float_to_int(f: f64, default: Value) -> Value {
    if !f.is_finite() {
        return default;
    }
    let whole = f.trunc();
    if whole.abs() < 9.2e18 {
        return Value::from(whole as i64);
    }
    integer(&format!("{whole:.0}")).unwrap_or(default)
}

/// `float(text)`: Rust's reading plus the underscores Python allows between two digits.
fn parse_float(text: &str) -> Option<f64> {
    let text = text.trim_matches(char::is_whitespace);
    let bytes = text.as_bytes();
    let spaced = |i: usize| {
        let digit = |at: Option<&u8>| at.is_some_and(u8::is_ascii_digit);
        digit(i.checked_sub(1).and_then(|before| bytes.get(before))) && digit(bytes.get(i + 1))
    };
    if bytes
        .iter()
        .enumerate()
        .any(|(i, b)| *b == b'_' && !spaced(i))
    {
        return None;
    }
    text.replace('_', "").parse().ok()
}

/// `int(text, base)`: the sign, the `0x`, `0o` and `0b` prefix of the base (all of them for base
/// 0), digits of that base, and a single underscore between two digits or after the prefix.
fn parse_int(text: &str, base: u32) -> Option<Value> {
    // Plain digits that fit 64 bits are the usual text. Anything else takes the full reading.
    if base != 0
        && let Ok(n) = i64::from_str_radix(text, base)
    {
        return Some(Value::from(n));
    }
    let text = text.trim_matches(char::is_whitespace);
    let (negative, rest) = match text.as_bytes().first()? {
        b'-' => (true, &text[1..]),
        b'+' => (false, &text[1..]),
        _ => (false, text),
    };
    let prefix = match rest.get(..2).map(str::to_ascii_lowercase).as_deref() {
        Some("0x") => 16,
        Some("0o") => 8,
        Some("0b") => 2,
        _ => 0,
    };
    let (base, digits) = if prefix != 0 && (base == 0 || base == prefix) {
        let after = &rest[2..];
        (prefix, after.strip_prefix('_').unwrap_or(after))
    } else if base == 0 {
        // A leading zero without a prefix is only valid for zero itself.
        if rest.starts_with('0') && rest.bytes().any(|b| b != b'0' && b != b'_') {
            return None;
        }
        (10, rest)
    } else {
        (base, rest)
    };
    let digits = clean_digits(digits, base)?;
    if digits.len() > MAX_DIGITS {
        return None;
    }
    let mut limbs = vec![0u32];
    for digit in digits {
        let mut carry = u64::from(digit);
        for limb in &mut limbs {
            let step = u64::from(*limb) * u64::from(base) + carry;
            *limb = (step % 1_000_000_000) as u32;
            carry = step / 1_000_000_000;
        }
        if carry > 0 {
            limbs.push(carry as u32);
        }
    }
    let (top, rest) = limbs.split_last()?;
    let mut decimal = format!("{}{top}", if negative { "-" } else { "" });
    for limb in rest.iter().rev() {
        let _ = write!(decimal, "{limb:09}");
    }
    integer(&decimal)
}

/// The value of each digit, `None` when a character is no digit of `base` or an underscore does
/// not sit between two digits. At least one digit.
fn clean_digits(text: &str, base: u32) -> Option<Vec<u8>> {
    let mut digits = Vec::with_capacity(text.len());
    let mut after_underscore = false;
    for c in text.chars() {
        if c == '_' {
            if digits.is_empty() || after_underscore {
                return None;
            }
            after_underscore = true;
            continue;
        }
        after_underscore = false;
        digits.push(u8::try_from(c.to_digit(base)?).ok()?);
    }
    (!digits.is_empty() && !after_underscore).then_some(digits)
}
