// SPDX-License-Identifier: GPL-3.0-or-later
//! Host ranges (`web[01:03]`, `db[a:c]`, `n[1:10:3]`) and `host:port` addresses, read the way
//! ansible-core 2.19.12's `expand_hostname_range` and `parse_address` read them.

use std::sync::LazyLock;

use regex::Regex;

const LETTERS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// Every host name a pattern stands for, in order, the end of a range included: `[01:03]` keeps
/// its zero padding, `[a:c]` counts letters, `[1:10:3]` steps. A name without `[` is itself.
/// The error is the reference's text, Python's own where the reference lets one through.
pub fn expand(line: &str) -> Result<Vec<String>, String> {
    if !line.contains('[') {
        return Ok(vec![line.to_string()]);
    }
    let replaced = line.replacen('[', "|", 1).replacen(']', "|", 1);
    let parts: Vec<&str> = replaced.split('|').collect();
    let [head, nrange, tail] = parts[..] else {
        return Err(if parts.len() < 3 {
            format!(
                "not enough values to unpack (expected 3, got {})",
                parts.len()
            )
        } else {
            "too many values to unpack (expected 3)".to_string()
        });
    };
    let bounds: Vec<&str> = nrange.split(':').collect();
    if bounds.len() != 2 && bounds.len() != 3 {
        return Err("host range must be begin:end or begin:end:step".to_string());
    }
    let beg = if bounds[0].is_empty() { "0" } else { bounds[0] };
    let end = bounds[1];
    let step = bounds.get(2).copied().unwrap_or("1");
    if end.is_empty() {
        return Err("host range must specify end value".to_string());
    }
    let width = if beg.starts_with('0') && beg.len() > 1 {
        if beg.len() != end.len() {
            return Err("host range must specify equal-length begin and end formats".to_string());
        }
        beg.len()
    } else {
        0
    };
    let letters = (LETTERS.find(beg), LETTERS.find(end), py_int(step));
    let seq: Vec<String> = if let (Some(b), Some(e), Ok(step)) = letters {
        if b > e {
            return Err("host range must have begin <= end".to_string());
        }
        if step == 0 {
            // Python's slice refuses a zero step with a ValueError, which sends the reference to
            // its numeric branch, where the letter is no number.
            return Err(py_int(beg).expect_err("a letter is no number"));
        }
        // A negative step over a rising slice gives nothing, as in Python.
        usize::try_from(step).map_or_else(
            |_| Vec::new(),
            |step| {
                LETTERS[b..=e]
                    .chars()
                    .step_by(step)
                    .map(String::from)
                    .collect()
            },
        )
    } else {
        let (b, e, step) = (py_int(beg)?, py_int(end)?, py_int(step)?);
        if step == 0 {
            return Err("range() arg 3 must not be zero".to_string());
        }
        numbers(b, e + 1, step)
            .map(|n| zero_fill(n, width))
            .collect()
    };
    let mut hosts = Vec::new();
    for item in seq {
        let name = format!("{head}{item}{tail}");
        if name.contains('[') {
            hosts.extend(expand(&name)?);
        } else {
            hosts.push(name);
        }
    }
    Ok(hosts)
}

/// Python's `range(start, stop, step)`.
fn numbers(start: i64, stop: i64, step: i64) -> impl Iterator<Item = i64> {
    let mut n = start;
    std::iter::from_fn(move || {
        let more = if step > 0 { n < stop } else { n > stop };
        more.then(|| {
            let cur = n;
            n += step;
            cur
        })
    })
}

/// `str(n).zfill(width)`: the sign stays in front of the zeros.
fn zero_fill(n: i64, width: usize) -> String {
    let digits = n.unsigned_abs().to_string();
    let sign = if n < 0 { "-" } else { "" };
    let pad = width.saturating_sub(sign.len() + digits.len());
    format!("{sign}{}{digits}", "0".repeat(pad))
}

/// Python's `int(text)`: blanks around, a sign, `_` only between digits.
pub(super) fn py_int(text: &str) -> Result<i64, String> {
    let t = text.trim();
    let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
    let valid = !digits.is_empty()
        && !digits.starts_with('_')
        && !digits.ends_with('_')
        && !digits.contains("__")
        && digits.chars().all(|c| c.is_ascii_digit() || c == '_');
    valid
        .then(|| t.replace('_', "").parse().ok())
        .flatten()
        .ok_or_else(|| format!("invalid literal for int() with base 10: '{text}'"))
}

const NUMERIC_RANGE: &str = r"\[[0-9]+:[0-9]+(?::[0-9]+)?\]";
const HEX_RANGE: &str = r"\[[0-9a-f]+:[0-9a-f]+(?::[0-9]+)?\]";
const ALNUM_RANGE: &str = r"\[(?:[a-z]:[a-z]|[0-9]+:[0-9]+)(?::[0-9]+)?\]";

static BRACKETED_HOSTPORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[(.+)\]:([0-9]+)$").expect("a valid pattern"));
static HOSTPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^((?:[^:\[\]]|\[[^\]]*\])*):([0-9]+)$").expect("a valid pattern")
});
static IPV4: LazyLock<Regex> = LazyLock::new(|| {
    let c = format!(r"(?:[01]?[0-9]{{1,2}}|2[0-4][0-9]|25[0-5]|{NUMERIC_RANGE})");
    Regex::new(&format!(r"(?i)^(?:{c}\.){{3}}{c}$")).expect("a valid pattern")
});
static IPV6: LazyLock<Regex> = LazyLock::new(|| {
    let c = format!(r"(?:[0-9a-f]{{1,4}}|{HEX_RANGE})");
    let forms = [
        format!(r"(?:{c}:){{7}}{c}"),
        format!(r"(?:{c}:){{1,6}}:"),
        format!(r"(?:{c}:)(?::{c}){{1,6}}"),
        format!(r"(?:{c}:){{2}}(?::{c}){{1,5}}"),
        format!(r"(?:{c}:){{3}}(?::{c}){{1,4}}"),
        format!(r"(?:{c}:){{4}}(?::{c}){{1,3}}"),
        format!(r"(?:{c}:){{5}}(?::{c}){{1,2}}"),
        format!(r"(?:{c}:){{6}}(?::{c})"),
        format!(r":(?::{c}){{1,6}}"),
        format!(r"{c}?::"),
        format!(r"(?:0:){{6}}(?:{c}\.){{3}}{c}"),
        format!(r"::(?:ffff:)?(?:{c}\.){{3}}{c}"),
        format!(r"(?:0:){{5}}ffff:(?:{c}\.){{3}}{c}"),
    ];
    Regex::new(&format!("(?i)^(?:{})$", forms.join("|"))).expect("a valid pattern")
});
static HOSTNAME: LazyLock<Regex> = LazyLock::new(|| {
    // A label starts with a word character or a range and does not end with `_` or `-`; the
    // reference says the last part with a look-behind, which this engine has no use for.
    let r = ALNUM_RANGE;
    let label = format!(r"(?:(?:[^\W_]|{r})|(?:\w|{r})(?:[\w-]|{r})*(?:[^\W_]|{r}))");
    Regex::new(&format!(r"(?i)^{label}(?:\.{label})*$")).expect("a valid pattern")
});

/// `parse_address`: a host name or address and the port after it, if any, or the reference's
/// reason for refusing the text.
pub fn parse_address(address: &str, allow_ranges: bool) -> Result<(String, Option<i64>), String> {
    let mut address = address.to_string();
    let mut port = None;
    for pattern in [&*BRACKETED_HOSTPORT, &*HOSTPORT] {
        if let Some(m) = pattern.captures(&address) {
            port = Some(
                m[2].parse()
                    .map_err(|_| format!("Not a valid network hostname: {address}"))?,
            );
            address = m[1].to_string();
        }
    }
    if ![&*IPV4, &*IPV6, &*HOSTNAME]
        .iter()
        .any(|p| p.is_match(&address))
    {
        return Err(format!("Not a valid network hostname: {address}"));
    }
    if !allow_ranges && address.contains('[') {
        return Err("Detected range in host but was asked to ignore ranges".to_string());
    }
    Ok((address, port))
}

#[cfg(test)]
mod tests;
