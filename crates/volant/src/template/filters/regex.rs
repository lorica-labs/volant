// SPDX-License-Identifier: GPL-3.0-or-later
//! The `regex_*` filters and the `match`, `search` and `regex` tests, translated from Python's `re`
//! to the `regex` crate.

use minijinja::value::{Kwargs, Rest};
use minijinja::{Error, Value};

use super::invalid;
use super::text::text_of;

pub(super) fn compile(pattern: &str, kwargs: &Kwargs) -> Result<regex::Regex, Error> {
    let ignorecase: bool = kwargs.get::<Option<bool>>("ignorecase")?.unwrap_or(false);
    let multiline: bool = kwargs.get::<Option<bool>>("multiline")?.unwrap_or(false);
    regex::RegexBuilder::new(pattern)
        .case_insensitive(ignorecase)
        .multi_line(multiline)
        .build()
        .map_err(|e| invalid(format!("invalid regular expression '{pattern}': {e}")))
}

/// MiniJinja's string literals fold `\1`..`\7` into the raw byte `chr(1)`..`chr(7)` (a JSON-style
/// octal escape); checked directly against ansible-core 2.19, its Jinja dialect does not do this
/// at all: `\n`, `\t`, `\1`, `\x41` and even `\'` all stay literal in a string literal.
/// Undo the fold for the arguments that carry Python-style
/// backreferences, so `'\1'` keeps meaning "backreference one" rather than a control byte.
/// Only single-digit octal folds (`\0`-`\7`) are reversed, matching every golden case;
/// MiniJinja's multi-digit octal folds (`\12` -> one byte) lose which digits were consumed, so a
/// two-digit backreference (`\10` and above) would need a real fix, not this inverse map.
pub(super) fn undo_octal_fold(s: &str) -> String {
    s.chars()
        .map(|c| match c as u32 {
            n @ 0..=7 => format!("\\{n}"),
            _ => c.to_string(),
        })
        .collect()
}

/// Python back-references (`\1`, `\g<name>`) to the `regex` crate's `${1}` and `${name}`.
pub(super) fn python_replacement(text: &str) -> String {
    let with_named = regex::Regex::new(r"\\g<([A-Za-z0-9_]+)>")
        .expect("static pattern")
        .replace_all(text, "$${$1}");
    regex::Regex::new(r"\\(\d+)")
        .expect("static pattern")
        .replace_all(&with_named, "$${$1}")
        .into_owned()
}

pub(super) fn regex_replace(
    value: Value,
    pattern: String,
    replacement: String,
    kwargs: Kwargs,
) -> Result<Value, Error> {
    let text = value
        .as_str()
        .ok_or_else(|| invalid("regex_replace requires a string"))?;
    let re = compile(&pattern, &kwargs)?;
    kwargs.assert_all_used()?;
    let replacement = undo_octal_fold(&replacement);
    Ok(Value::from(
        re.replace_all(text, python_replacement(&replacement).as_str())
            .into_owned(),
    ))
}

/// `regex_search(value, pattern, *groups)`: the whole match, or the listed groups (`\1`, `\g<name>`)
/// as a list; `None` when nothing matches.
pub(super) fn regex_search(
    value: Value,
    pattern: String,
    groups: Rest<String>,
    kwargs: Kwargs,
) -> Result<Value, Error> {
    let text = value
        .as_str()
        .ok_or_else(|| invalid("regex_search requires a string"))?;
    let re = compile(&pattern, &kwargs)?;
    kwargs.assert_all_used()?;
    let Some(caps) = re.captures(text) else {
        return Ok(Value::from(()));
    };
    if groups.is_empty() {
        return Ok(Value::from(caps.get(0).map_or("", |m| m.as_str())));
    }
    let mut out = Vec::new();
    for g in groups.iter() {
        let g = undo_octal_fold(g);
        let m = if let Some(n) = g.strip_prefix('\\').and_then(|n| n.parse::<usize>().ok()) {
            caps.get(n)
        } else if let Some(name) = g.strip_prefix("\\g<").and_then(|n| n.strip_suffix('>')) {
            caps.name(name)
        } else {
            return Err(invalid(format!("Unknown match group '{g}'")));
        };
        out.push(Value::from(m.map_or("", |m| m.as_str())));
    }
    Ok(Value::from(out))
}

pub(super) fn regex_findall(value: Value, pattern: String, kwargs: Kwargs) -> Result<Value, Error> {
    let text = value
        .as_str()
        .ok_or_else(|| invalid("regex_findall requires a string"))?;
    let re = compile(&pattern, &kwargs)?;
    kwargs.assert_all_used()?;
    let matches: Vec<Value> = re
        .find_iter(text)
        .map(|m| Value::from(m.as_str()))
        .collect();
    Ok(Value::from(matches))
}

pub(super) fn regex_test(value: &Value, pattern: &str, anchored: bool, kwargs: Kwargs) -> bool {
    let Some(text) = value.as_str() else {
        return false;
    };
    let Ok(re) = compile(pattern, &kwargs) else {
        return false;
    };
    match re.find(text) {
        Some(m) if anchored => m.start() == 0,
        Some(_) => true,
        None => false,
    }
}

/// `regex_escape(re_type='python')`: Python 3's `re.escape`, which escapes only its special
/// characters, or the POSIX basic set `].[^$*\`.
pub(super) fn regex_escape(
    value: Value,
    re_type: Option<String>,
    kwargs: Kwargs,
) -> Result<String, Error> {
    let re_type = kwargs
        .get::<Option<String>>("re_type")?
        .or(re_type)
        .unwrap_or_else(|| "python".to_string());
    kwargs.assert_all_used()?;
    let special = match re_type.as_str() {
        "python" => "()[]{}?*+-|^$\\.&~# \t\n\r\x0b\x0c",
        "posix_basic" => "].[^$*\\",
        "posix_extended" => {
            return Err(invalid(format!(
                "Regex type ({re_type}) not yet implemented"
            )));
        }
        _ => return Err(invalid(format!("Invalid regex type ({re_type})"))),
    };
    let mut out = String::new();
    for c in text_of(&value).chars() {
        if special.contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    Ok(out)
}
