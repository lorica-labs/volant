// SPDX-License-Identifier: GPL-3.0-or-later
//! How PyYAML's `SafeLoader` reads a scalar, which is how ansible-core reads it.
//!
//! saphyr resolves a plain scalar by the YAML 1.2 core schema, and it drops the text, the style
//! and the tag of a scalar it has resolved. The loader here keeps all three
//! (`YamlLoader::early_parse(false)`), so that an alias, which saphyr copies from its anchor,
//! carries them too. This module then resolves the scalar the way PyYAML does: the implicit
//! resolver of `yaml/resolver.py` for an untagged plain scalar, and the constructors of
//! `yaml/constructor.py` for a tag.

use std::borrow::Cow;
use std::fmt::Write;
use std::sync::LazyLock;

use regex::Regex;
use saphyr::{Scalar, Yaml};
use saphyr_parser::{ScalarStyle, Tag};

/// The prefix of the tags PyYAML knows by their short form `!!`.
const CORE: &str = "tag:yaml.org,2002:";

/// PyYAML's float pattern. `$` of Python also matches before a final line feed, hence `\n?\z`.
static FLOAT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:[-+]?[0-9][0-9_]*\.[0-9_]*(?:[eE][-+][0-9]+)?|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN))\n?\z",
    )
    .expect("a fixed pattern")
});

/// PyYAML's integer pattern.
static INT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+)\n?\z",
    )
    .expect("a fixed pattern")
});

/// What PyYAML's resolver makes of `value` when nothing says otherwise: the first of its
/// patterns that matches, in its order (bool, float, int, null), and the string when none does.
/// Every pattern is tried only for the first characters PyYAML tries it for.
///
/// `<<` and `=` also have a pattern there, which no constructor serves. They stay strings here.
/// The merge key is the business of the code that flattens a mapping.
pub(super) fn plain(value: Cow<'_, str>) -> Result<Yaml<'_>, String> {
    let first = value.chars().next();
    let body = value.strip_suffix('\n').unwrap_or(&value);
    if matches!(
        first,
        Some('y' | 'Y' | 'n' | 'N' | 't' | 'T' | 'f' | 'F' | 'o' | 'O')
    ) && let Some(truth) = super::bool_from_str(body)
    {
        // A block scalar's last line feed passes the pattern and fails the lookup.
        return if body.len() == value.len() {
            Ok(Yaml::Value(Scalar::Boolean(truth)))
        } else {
            Err(py_repr(&value.to_lowercase()))
        };
    }
    if matches!(first, Some('-' | '+' | '.' | '0'..='9')) {
        if FLOAT.is_match(&value) {
            return float(&value).map(Yaml::Value);
        }
        if INT.is_match(&value) {
            return int(value);
        }
    }
    if matches!(body, "~" | "null" | "Null" | "NULL" | "") {
        return Ok(Yaml::Value(Scalar::Null));
    }
    if matches!(first, Some('0'..='9')) && TIMESTAMP_PATTERN.is_match(&value) {
        return timestamp(&value).map(Yaml::Value);
    }
    Ok(Yaml::Value(Scalar::String(value)))
}

/// How a scalar written with `style` and `tag` reads. An untagged scalar that is not plain is
/// a string. A tag of the core schema picks its constructor, and any other tag has none.
pub(super) fn read<'a>(
    value: Cow<'a, str>,
    style: ScalarStyle,
    tag: Option<&Tag>,
) -> Result<Yaml<'a>, String> {
    let Some(tag) = tag else {
        return if style == ScalarStyle::Plain {
            plain(value)
        } else {
            Ok(Yaml::Value(Scalar::String(value)))
        };
    };
    let Some(name) = tag
        .handle
        .strip_prefix(CORE)
        .map(|rest| format!("{rest}{}", tag.suffix))
    else {
        return Err(no_constructor(tag));
    };
    let scalar = |made: Result<Scalar<'a>, String>| made.map(Yaml::Value);
    match name.as_str() {
        "str" | "python/unicode" => Ok(Yaml::Value(Scalar::String(value))),
        "int" => int(value),
        "float" => scalar(float(&value)),
        "bool" => scalar(bool_constructor(&value)),
        "null" => Ok(Yaml::Value(Scalar::Null)),
        "timestamp" => scalar(timestamp(&value)),
        "seq" => Err("Expected a sequence node, but found scalar.".into()),
        "map" | "set" | "python/dict" => Err("Expected a mapping node, but found scalar.".into()),
        "omap" => {
            Err("While constructing an ordered map expected a sequence, but found scalar.".into())
        }
        "pairs" => Err("While constructing pairs expected a sequence, but found scalar.".into()),
        "binary" => Err("The '!!binary' tag is not supported.".into()),
        _ => Err(no_constructor(tag)),
    }
}

/// PyYAML's message for a tag it has no constructor for.
pub(super) fn no_constructor(tag: &Tag) -> String {
    format!(
        "Could not determine a constructor for the tag {}.",
        py_repr(&format!("{}{}", tag.handle, tag.suffix))
    )
}

/// PyYAML's `construct_yaml_bool`: a lookup of the lower-cased text.
fn bool_constructor(value: &str) -> Result<Scalar<'static>, String> {
    let lower = value.to_lowercase();
    super::bool_from_str(&lower)
        .map(Scalar::Boolean)
        .ok_or_else(|| py_repr(&lower))
}

/// PyYAML's `construct_yaml_int`: sign, then `0b`, `0x`, a leading `0` for octal, base 60 with
/// `:`, or decimal. A value beyond what an `i64` holds is an integer node tagged `!!int` that
/// holds its decimal digits: [`wide_digits`] says how [`super::walk`] reads it back. Past 128
/// bits, only base 60 stays the string it was written as.
fn int(value: Cow<'_, str>) -> Result<Yaml<'_>, String> {
    let cleaned = value.replace('_', "");
    let (negative, digits) = match cleaned.as_bytes().first() {
        Some(b'-') => (true, &cleaned[1..]),
        Some(b'+') => (false, &cleaned[1..]),
        Some(_) => (false, cleaned.as_str()),
        None => return Err("string index out of range".into()),
    };
    let mut wide: Option<(u32, &str)> = None;
    let magnitude = if digits == "0" {
        Some(0)
    } else if let Some(rest) = digits.strip_prefix("0b") {
        wide = Some((2, rest));
        radix(rest, 2)?
    } else if let Some(rest) = digits.strip_prefix("0x") {
        wide = Some((16, rest));
        radix(rest, 16)?
    } else if digits.starts_with('0') {
        wide = Some((8, digits));
        radix(digits, 8)?
    } else if digits.contains(':') {
        let mut total: Option<i128> = Some(0);
        let mut place: Option<i128> = Some(1);
        for part in digits.rsplit(':') {
            let digit = radix(part, 10)?;
            total = total
                .zip(digit)
                .zip(place)
                .and_then(|((t, d), p)| d.checked_mul(p).and_then(|v| t.checked_add(v)));
            place = place.and_then(|p| p.checked_mul(60));
        }
        total
    } else if digits.is_empty() {
        return Err("string index out of range".into());
    } else {
        wide = Some((10, digits));
        radix(digits, 10)?
    };
    let signed = magnitude.map(|m| if negative { -m } else { m });
    if let Some(small) = signed.and_then(|v| i64::try_from(v).ok()) {
        return Ok(Yaml::Value(Scalar::Integer(small)));
    }
    let text = match (signed, wide) {
        (Some(v), _) => Some(v.to_string()),
        (None, Some((base, digits))) => to_decimal(base, digits, negative)?,
        (None, None) => None,
    };
    Ok(text.map_or_else(
        || Yaml::Value(Scalar::String(value)),
        |text| {
            let tag = Tag {
                handle: CORE.to_string(),
                suffix: "int".to_string(),
            };
            Yaml::Tagged(
                Cow::Owned(tag),
                Box::new(Yaml::Value(Scalar::String(text.into()))),
            )
        },
    ))
}

/// The digits of an integer read through a `!!int` node that [`int`] made, when `tag` is one.
pub(super) fn wide_digits<'a>(tag: &Tag, inner: &'a Yaml<'_>) -> Option<&'a str> {
    match inner {
        Yaml::Value(Scalar::String(text)) if tag.handle == CORE && tag.suffix == "int" => {
            Some(text)
        }
        _ => None,
    }
}

/// Python 3.11 and later refuse to turn an integer of more than this many decimal digits into
/// text, or text of that many digits into an integer.
const MAX_DIGITS: usize = 4300;

fn too_many_digits(count: usize) -> String {
    format!(
        "Exceeds the limit ({MAX_DIGITS} digits) for integer string conversion: value has {count} digits; use sys.set_int_max_str_digits() to increase the limit"
    )
}

/// The decimal text of `digits` written in `base`, for a number too wide for `i128`; `None` when
/// a character is not a digit of `base`. A decimal number is already text. For the other bases
/// the work is schoolbook multiplication on limbs of nine decimal digits, the square of the
/// length, so a number past Python's limit is refused before any work is done.
fn to_decimal(base: u32, digits: &str, negative: bool) -> Result<Option<String>, String> {
    const LIMB: u64 = 1_000_000_000;
    let digits = digits.trim();
    let sign = if negative { "-" } else { "" };
    if base == 10 {
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(None);
        }
        let digits = digits.trim_start_matches('0');
        if digits.len() > MAX_DIGITS {
            return Err(too_many_digits(digits.len()));
        }
        return Ok(Some(format!("{sign}{digits}")));
    }
    // Each digit of `base` is at most four bits; four bits are at most 1.21 decimal digits.
    if digits.len() * 5 / 4 > MAX_DIGITS + 1000 {
        return Err(too_many_digits(digits.len() * 5 / 4));
    }
    let mut limbs: Vec<u64> = vec![0];
    for c in digits.chars() {
        let Some(digit) = c.to_digit(base) else {
            return Ok(None);
        };
        let mut carry = u64::from(digit);
        for limb in &mut limbs {
            let next = *limb * u64::from(base) + carry;
            *limb = next % LIMB;
            carry = next / LIMB;
        }
        if carry > 0 {
            limbs.push(carry);
        }
    }
    let mut text = String::from(sign);
    let mut rest = limbs.iter().rev();
    if let Some(first) = rest.next() {
        write!(text, "{first}").ok();
    }
    for limb in rest {
        write!(text, "{limb:09}").ok();
    }
    if text.len() - sign.len() > MAX_DIGITS {
        return Err(too_many_digits(text.len() - sign.len()));
    }
    Ok(Some(text))
}

/// Python's `int(text, base)` for the digits the resolver lets through: the number, or `None`
/// when it does not fit an `i128`. Python allows white space around the digits and a sign.
fn radix(text: &str, base: u32) -> Result<Option<i128>, String> {
    let trimmed = text.trim();
    let (negative, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    // Python lets the digits of base 2, 8 or 16 start with that base's own prefix.
    let digits = match (
        base,
        digits.get(..2).map(str::to_ascii_lowercase).as_deref(),
    ) {
        (2, Some("0b")) | (8, Some("0o")) | (16, Some("0x")) => &digits[2..],
        _ => digits,
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(base)) {
        return Err(format!(
            "invalid literal for int() with base {base}: {}",
            py_repr(text)
        ));
    }
    let mut total: i128 = 0;
    for c in digits.chars() {
        let digit = i128::from(c.to_digit(base).unwrap_or(0));
        match total
            .checked_mul(i128::from(base))
            .and_then(|t| t.checked_add(digit))
        {
            Some(t) => total = t,
            None => return Ok(None),
        }
    }
    Ok(Some(if negative { -total } else { total }))
}

/// PyYAML's `construct_yaml_float`: lower-cased, a sign, `.inf`, `.nan`, base 60 with `:`, or
/// the number Python's `float` reads.
fn float(value: &str) -> Result<Scalar<'static>, String> {
    let cleaned = value.replace('_', "").to_lowercase();
    let (negative, digits) = match cleaned.as_bytes().first() {
        Some(b'-') => (true, &cleaned[1..]),
        Some(b'+') => (false, &cleaned[1..]),
        Some(_) => (false, cleaned.as_str()),
        None => return Err("string index out of range".into()),
    };
    let number = if digits == ".inf" {
        f64::INFINITY
    } else if digits == ".nan" {
        // PyYAML returns the not-a-number value whatever the sign.
        return Ok(Scalar::FloatingPoint(f64::NAN.into()));
    } else if digits.contains(':') {
        let mut total = 0.0;
        let mut place = 1.0;
        for part in digits.rsplit(':') {
            total += python_float(part)? * place;
            place *= 60.0;
        }
        total
    } else {
        python_float(digits)?
    };
    Ok(Scalar::FloatingPoint(
        (if negative { -number } else { number }).into(),
    ))
}

/// Python's `float(text)`.
fn python_float(text: &str) -> Result<f64, String> {
    text.trim()
        .parse::<f64>()
        .map_err(|_| format!("could not convert string to float: {}", py_repr(text)))
}

/// Python's `repr` of a string, which its error messages quote the offending value with.
pub(super) fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => _ = write!(out, "\\x{:02x}", u32::from(c)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// The timestamp pattern of PyYAML's resolver.
static TIMESTAMP_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:[0-9]{4}-[0-9]{2}-[0-9]{2}|[0-9]{4}-[0-9][0-9]?-[0-9][0-9]?(?:[Tt]|[ \t]+)[0-9][0-9]?:[0-9]{2}:[0-9]{2}(?:\.[0-9]*)?(?:[ \t]*(?:Z|[-+][0-9][0-9]?(?::[0-9]{2})?))?)\n?\z",
    )
    .expect("a fixed pattern")
});

/// The pattern of PyYAML's `construct_yaml_timestamp`, with a group for each part.
static TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?P<year>[0-9]{4})-(?P<month>[0-9][0-9]?)-(?P<day>[0-9][0-9]?)(?:(?:[Tt]|[ \t]+)(?P<hour>[0-9][0-9]?):(?P<minute>[0-9]{2}):(?P<second>[0-9]{2})(?:\.(?P<fraction>[0-9]*))?(?:[ \t]*(?P<tz>Z|(?P<sign>[-+])(?P<tz_hour>[0-9][0-9]?)(?::(?P<tz_minute>[0-9]{2}))?))?)?\n?\z",
    )
    .expect("a fixed pattern")
});

/// PyYAML's `construct_yaml_timestamp`. The value model has no date, so the date or the date and
/// time come out as the string Python's `isoformat` gives, which is what ansible-core prints.
fn timestamp(value: &str) -> Result<Scalar<'static>, String> {
    let Some(parts) = TIMESTAMP.captures(value) else {
        return Err("'NoneType' object has no attribute 'groupdict'".into());
    };
    let number = |name: &str| {
        parts
            .name(name)
            .map_or(0, |m| m.as_str().parse::<u32>().unwrap_or(0))
    };
    let (year, month, day) = (number("year"), number("month"), number("day"));
    if year == 0 {
        return Err("year 0 is out of range".into());
    }
    if !(1..=12).contains(&month) {
        return Err("month must be in 1..12".into());
    }
    if day == 0 || day > days_in_month(year, month) {
        return Err("day is out of range for month".into());
    }
    let mut text = format!("{year:04}-{month:02}-{day:02}");
    if parts.name("hour").is_none() {
        return Ok(Scalar::String(text.into()));
    }
    let (hour, minute, second) = (number("hour"), number("minute"), number("second"));
    if hour > 23 {
        return Err("hour must be in 0..23".into());
    }
    if minute > 59 {
        return Err("minute must be in 0..59".into());
    }
    if second > 59 {
        return Err("second must be in 0..59".into());
    }
    _ = write!(text, "T{hour:02}:{minute:02}:{second:02}");
    let fraction = parts.name("fraction").map_or("", |m| m.as_str());
    let micros: u32 = format!("{:0<6}", &fraction[..fraction.len().min(6)])
        .parse()
        .unwrap_or(0);
    if micros != 0 {
        _ = write!(text, ".{micros:06}");
    }
    if let Some(sign) = parts.name("sign") {
        let minutes = i64::from(number("tz_hour")) * 60 + i64::from(number("tz_minute"));
        let seconds = if sign.as_str() == "-" {
            -minutes * 60
        } else {
            minutes * 60
        };
        if seconds.abs() >= 86_400 {
            return Err(offset_error(seconds));
        }
        let (sign, minutes) = (if seconds < 0 { '-' } else { '+' }, seconds.abs() / 60);
        _ = write!(text, "{sign}{:02}:{:02}", minutes / 60, minutes % 60);
    } else if parts.name("tz").is_some() {
        text.push_str("+00:00");
    }
    Ok(Scalar::String(text.into()))
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Python's message for a time zone offset of a day or more, naming it as `repr` does.
fn offset_error(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let mut shown = Vec::new();
    if days != 0 {
        shown.push(format!("days={days}"));
    }
    if rest != 0 {
        shown.push(format!("seconds={rest}"));
    }
    format!(
        "offset must be a timedelta strictly between -timedelta(hours=24) and timedelta(hours=24), not datetime.timedelta({}).",
        shown.join(", ")
    )
}
