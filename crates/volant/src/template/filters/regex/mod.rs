// SPDX-License-Identifier: GPL-3.0-or-later
//! The `regex_*` filters and the `match`, `search` and `regex` tests, as ansible-core runs them
//! on Python's `re`: the pattern is translated to the `regex` crate (`pattern`), the matches are
//! walked as Python walks them (`engine`), and the replacement is Python's (`replacement`).

use minijinja::value::{Kwargs, Rest};
use minijinja::{Error, Value};

use super::invalid;
use crate::playbook::python_repr;
use engine::compile;
use replacement::Template;

mod engine;
mod pattern;
mod replacement;

/// MiniJinja's string literals fold `\1`..`\7` into the raw byte `chr(1)`..`chr(7)` and `\b` into
/// `chr(8)` (a JSON-style escape); checked directly against ansible-core 2.19, its Jinja dialect
/// does not do this at all: `\n`, `\t`, `\1`, `\x41` and even `\'` all stay literal in a string
/// literal. Undo the fold for the arguments that carry Python-style backreferences and word
/// boundaries, so `'\1'` keeps meaning "backreference one" rather than a control byte.
/// Only single-digit octal folds (`\0`-`\7`) are reversed, matching every golden case;
/// MiniJinja's multi-digit octal folds (`\12` -> one byte) lose which digits were consumed, so a
/// two-digit backreference (`\10` and above) would need a real fix, not this inverse map.
/// `\\` is folded to one backslash and cannot be told from a lone one.
pub(super) fn undo_octal_fold(s: &str) -> String {
    s.chars()
        .map(|c| match c as u32 {
            n @ 0..=7 => format!("\\{n}"),
            _ => c.to_string(),
        })
        .collect()
}

/// [`undo_octal_fold`] for a pattern, where `chr(8)` is also the fold of the word boundary `\b`.
fn unfold_pattern(s: &str) -> String {
    undo_octal_fold(s).replace('\u{8}', "\\b")
}

/// `to_text(value, nonstring='simplerepr')`: text as it is, anything else as Python prints it.
fn to_text(value: &Value) -> String {
    if let Some(bytes) = super::super::pymethods::bytes::of(value) {
        return String::from_utf8_lossy(&bytes.0).into_owned();
    }
    value.as_str().map_or_else(
        || python_repr(&crate::template::bigint::to_json(value).unwrap_or(serde_json::Value::Null)),
        str::to_string,
    )
}

/// The arguments of a Python function, positional first and then by name. `names` are the
/// parameters after the value, in order. A parameter given twice or not at all named, or too
/// many positional arguments, is an error when `strict`.
pub(super) fn bind(
    filter: &str,
    names: &[&str],
    positional: &[Value],
    kwargs: &Kwargs,
    strict: bool,
) -> Result<Vec<Option<Value>>, Error> {
    if positional.len() > names.len() {
        return Err(invalid(format!(
            "{filter}() takes at most {} arguments after the value, {} given",
            names.len(),
            positional.len()
        )));
    }
    let mut slots: Vec<Option<Value>> = vec![None; names.len()];
    for (slot, value) in slots.iter_mut().zip(positional) {
        *slot = Some(value.clone());
    }
    for (slot, name) in slots.iter_mut().zip(names) {
        if kwargs.has(name) {
            if slot.is_some() {
                return Err(invalid(format!(
                    "{filter}() got multiple values for argument '{name}'"
                )));
            }
            *slot = Some(kwargs.get::<Value>(name)?);
        }
    }
    if strict {
        kwargs.assert_all_used()?;
    }
    Ok(slots)
}

fn flag(slot: Option<&Option<Value>>) -> bool {
    slot.and_then(Option::as_ref).is_some_and(Value::is_true)
}

/// A pattern or replacement argument, which Python only takes as text; `default` when absent.
fn text_arg(filter: &str, what: &str, slot: Option<&Option<Value>>) -> Result<String, Error> {
    match slot.and_then(Option::as_ref) {
        None => Ok(String::new()),
        Some(v) => v.as_str().map(str::to_string).ok_or_else(|| {
            invalid(format!(
                "{filter}: the {what} must be a string, not {}",
                python_repr(
                    &crate::template::bigint::to_json(v).unwrap_or(serde_json::Value::Null)
                )
            ))
        }),
    }
}

fn count_arg(filter: &str, name: &str, slot: Option<&Option<Value>>) -> Result<i64, Error> {
    match slot.and_then(Option::as_ref) {
        None => Ok(0),
        Some(v) => i64::try_from(v.clone())
            .map_err(|_| invalid(format!("{filter}: {name} must be an integer"))),
    }
}

pub(super) fn regex_replace(
    value: Value,
    rest: Rest<Value>,
    kwargs: Kwargs,
) -> Result<Value, Error> {
    let a = bind(
        "regex_replace",
        &[
            "pattern",
            "replacement",
            "ignorecase",
            "multiline",
            "count",
            "mandatory_count",
        ],
        &rest,
        &kwargs,
        true,
    )?;
    let text = to_text(&value);
    let pattern = unfold_pattern(&text_arg("regex_replace", "pattern", a.first())?);
    let replacement = undo_octal_fold(&text_arg("regex_replace", "replacement", a.get(1))?);
    let count = count_arg("regex_replace", "count", a.get(4))?;
    let mandatory = count_arg("regex_replace", "mandatory_count", a.get(5))?;
    let re = compile(&pattern, flag(a.get(2)), flag(a.get(3)))?;
    let template = Template::parse(&replacement, re.groups, &re.names)
        .map_err(|why| invalid(format!("regex_replace: invalid replacement: {why}")))?;
    let with_caps = template.reads_groups();
    let mut out = String::with_capacity(text.len());
    let (mut last, mut done) = (0, 0_i64);
    re.scan(&text, with_caps, |hit| {
        // A negative count replaces nothing, as in Python.
        if count < 0 || (count > 0 && done >= count) {
            return false;
        }
        out.push_str(&text[last..hit.start]);
        match &hit.caps {
            Some(caps) => template.expand(caps, &mut out),
            None => template.expand_text(&mut out),
        }
        last = hit.end;
        done += 1;
        true
    })?;
    out.push_str(&text[last..]);
    if mandatory != 0 && mandatory != done {
        return Err(invalid(format!(
            "'{pattern}' should match {mandatory} times, but matches {count} times in '{text}'"
        )));
    }
    Ok(Value::from(out))
}

/// `regex_search(value, pattern, *groups)`: the whole match, or the listed groups (`\1`,
/// `\g<name>`) as a list, with `None` for a group that took no part; `None` when nothing
/// matches. Other keyword arguments are ignored, as in the reference.
pub(super) fn regex_search(
    value: Value,
    pattern: Value,
    groups: Rest<Value>,
    kwargs: Kwargs,
) -> Result<Value, Error> {
    let text = to_text(&value);
    let mut wanted = Vec::new();
    for g in groups.iter() {
        let arg = undo_octal_fold(&text_arg(
            "regex_search",
            "group argument",
            Some(&Some(g.clone())),
        )?);
        wanted.push(group_ref(&arg)?);
    }
    let pattern = unfold_pattern(&text_arg("regex_search", "pattern", Some(&Some(pattern)))?);
    let ignorecase = kwargs
        .get::<Option<Value>>("ignorecase")?
        .is_some_and(|v| v.is_true());
    let multiline = kwargs
        .get::<Option<Value>>("multiline")?
        .is_some_and(|v| v.is_true());
    let re = compile(&pattern, ignorecase, multiline)?;
    let Some(hit) = re.search(&text, true)? else {
        return Ok(Value::from(()));
    };
    let caps = hit.caps.as_ref().expect("captures were asked for");
    if wanted.is_empty() {
        return Ok(Value::from(&text[hit.start..hit.end]));
    }
    let mut out = Vec::new();
    for group in &wanted {
        let number = match group {
            GroupRef::Number(n) if *n <= re.groups => *n,
            GroupRef::Name(name) => re
                .names
                .iter()
                .find(|(known, _)| known == name)
                .map(|(_, n)| *n)
                .ok_or_else(|| invalid("regex_search: no such group"))?,
            GroupRef::Number(_) => return Err(invalid("regex_search: no such group")),
        };
        let found = caps.get(number);
        out.push(found.map_or_else(|| Value::from(()), |m| Value::from(m.as_str())));
    }
    Ok(Value::from(out))
}

enum GroupRef {
    Number(usize),
    Name(String),
}

/// One extra argument of `regex_search`: `\g<name>` or `\1`, as the reference reads them.
/// `\g<2>` names a group `"2"`, which no pattern has, so the reference fails on it.
fn group_ref(arg: &str) -> Result<GroupRef, Error> {
    if let Some(rest) = arg.strip_prefix("\\g") {
        let run: &str = rest.split(char::is_whitespace).next().unwrap_or("");
        let name = run
            .strip_prefix('<')
            .and_then(|r| r.rfind('>').map(|end| &r[..end]))
            .filter(|n| !n.is_empty());
        return name
            .map(|n| GroupRef::Name(n.to_string()))
            .ok_or_else(|| invalid(format!("regex_search: bad group argument '{arg}'")));
    }
    if let Some(rest) = arg.strip_prefix('\\') {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        return digits
            .parse::<usize>()
            .map(GroupRef::Number)
            .map_err(|_| invalid(format!("regex_search: bad group argument '{arg}'")));
    }
    Err(invalid("regex_search: Unknown argument"))
}

/// `regex_findall(value, regex, multiline=False, ignorecase=False)`: `re.findall`. Without a
/// group the matches, with one the text of that group, with more a list of group texts for each
/// match; a group that took no part is empty.
pub(super) fn regex_findall(
    value: Value,
    rest: Rest<Value>,
    kwargs: Kwargs,
) -> Result<Value, Error> {
    let a = bind(
        "regex_findall",
        &["regex", "multiline", "ignorecase"],
        &rest,
        &kwargs,
        true,
    )?;
    let text = to_text(&value);
    let pattern = unfold_pattern(&text_arg("regex_findall", "pattern", a.first())?);
    let re = compile(&pattern, flag(a.get(2)), flag(a.get(1)))?;
    let group_text =
        |caps: &regex::Captures<'_>, n: usize| Value::from(caps.get(n).map_or("", |m| m.as_str()));
    let mut found = Vec::new();
    re.scan(&text, re.groups > 0, |hit| {
        found.push(match (&hit.caps, re.groups) {
            (Some(caps), 1) => group_text(caps, 1),
            (Some(caps), n) => {
                Value::from((1..=n).map(|g| group_text(caps, g)).collect::<Vec<_>>())
            }
            (None, _) => Value::from(&text[hit.start..hit.end]),
        });
        true
    })?;
    Ok(Value::from(found))
}

/// The `match`, `search` and `regex` tests: `re.compile(pattern, flags)` and one of its
/// methods, `match_type`, applied to the value as text.
pub(super) fn regex_test(
    value: &Value,
    rest: &[Value],
    kwargs: &Kwargs,
    test: &str,
) -> Result<bool, Error> {
    let mut names = vec!["pattern", "ignorecase", "multiline"];
    if test == "regex" {
        names.push("match_type");
    }
    let a = bind(test, &names, rest, kwargs, true)?;
    let match_type = match a.get(3).and_then(Option::as_ref) {
        Some(v) => v
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| invalid("regex: match_type must name a method of a compiled pattern"))?,
        None if test == "regex" => "search".to_string(),
        None => test.to_string(),
    };
    let pattern = unfold_pattern(&text_arg(test, "pattern", a.first())?);
    let re = compile(&pattern, flag(a.get(1)), flag(a.get(2)))?;
    let text = to_text(value);
    match match_type.as_str() {
        "search" | "findall" => Ok(re.search(&text, false)?.is_some()),
        "match" => Ok(re.search(&text, false)?.is_some_and(|hit| hit.start == 0)),
        other => Err(invalid(format!(
            "regex: match_type '{other}' is not supported: use search, match or findall"
        ))),
    }
}

/// `regex_escape(re_type='python')`: Python 3's `re.escape`, which escapes only its special
/// characters, or the POSIX basic set `].[^$*\`.
pub(super) fn regex_escape(
    value: Value,
    rest: Rest<Value>,
    kwargs: Kwargs,
) -> Result<String, Error> {
    let a = bind("regex_escape", &["re_type"], &rest, &kwargs, true)?;
    let re_type = match a.first().and_then(Option::as_ref) {
        None => "python".to_string(),
        Some(v) => to_text(v),
    };
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
    for c in to_text(&value).chars() {
        if special.contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    Ok(out)
}
