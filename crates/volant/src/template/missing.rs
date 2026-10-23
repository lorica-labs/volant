// SPDX-License-Identifier: GPL-3.0-or-later
//! Naming what an undefined read could not find, as ansible-core words it: the variable
//! (`'nope' is undefined`), or the attribute with the type it was read from (`object of type
//! 'dict' has no attribute 'b'`). The engine raises one sentence for both and does not say which
//! name it was, so the source is read back against the variables the render saw.

use std::collections::HashSet;
use std::sync::LazyLock;

use minijinja::value::ValueKind;
use regex::Regex;

use super::{TemplateError, UNDEFINED, take_failure};

/// The words that look like a name in an expression and are not one.
const WORDS: [&str; 17] = [
    "and",
    "or",
    "not",
    "in",
    "is",
    "if",
    "else",
    "true",
    "false",
    "none",
    "for",
    "set",
    "elif",
    "endif",
    "endfor",
    "loop",
    "namespace",
];

/// A string literal, or a name with the attribute reads chained on it, then what follows it: a
/// call, or `=` for a keyword argument. A name behind `.`, `|` or `is` is an attribute, a filter or
/// a test, not a variable.
static NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"'[^']*'|"[^"]*"|(\.\s*|\|\s*|\bis\s+(?:not\s+)?)?\b([A-Za-z_]\w*)((?:\s*\.\s*[A-Za-z_]\w*|\s*\[\s*(?:'[^']*'|"[^"]*")\s*\])*)\s*(\(|==|=)?"#,
    )
    .expect("a fixed pattern")
});

static SEGMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\.\s*([A-Za-z_]\w*)|\[\s*(?:'([^']*)'|"([^"]*)")\s*\]"#).expect("a fixed pattern")
});

/// A read that cannot fail: it is the subject of `default` or of `is defined`.
static GUARDED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:\|\s*(?:ansible\.builtin\.)?(?:default|d)\b|is\s+(?:not\s+)?(?:un)?defined\b)",
    )
    .expect("a fixed pattern")
});

static BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)\{\{(.*?)\}\}|\{%(.*?)%\}").expect("a fixed pattern"));

static BOUND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\{%-?\s*(?:for\s+(.+?)\s+in\b|set\s+(\w+))").expect("a fixed pattern")
});

/// The Python name of the type a value has.
fn py_type(value: &minijinja::Value) -> &'static str {
    match value.kind() {
        ValueKind::Undefined => "Undefined",
        ValueKind::None => "NoneType",
        ValueKind::Bool => "bool",
        ValueKind::Number if value.is_integer() => "int",
        ValueKind::Number => "float",
        ValueKind::String => "str",
        ValueKind::Seq | ValueKind::Iterable => "list",
        _ => "dict",
    }
}

/// What the first read in `source` that finds nothing in `ctx` says, if there is one. The check
/// is on the playbook's own expression, in order; a read guarded by `default` or `is defined`
/// is not a candidate. An expression that fails on a later read after an earlier one was guarded
/// and still missing is worded by the earlier one only when that one is not guarded, so the
/// reading can miss, and then the engine's sentence stands.
fn reason(source: &str, ctx: &minijinja::Value) -> Option<String> {
    let blocks: Vec<&str> = if source.contains("{{") || source.contains("{%") {
        BLOCK
            .captures_iter(source)
            .filter_map(|c| c.get(1).or_else(|| c.get(2)))
            .map(|m| m.as_str())
            .collect()
    } else {
        vec![source]
    };
    let mut bound: HashSet<String> = HashSet::new();
    for c in BOUND.captures_iter(source) {
        for m in [c.get(1), c.get(2)].into_iter().flatten() {
            bound.extend(
                m.as_str()
                    .split(|ch: char| ch == ',' || ch.is_whitespace())
                    .filter(|n| !n.is_empty())
                    .map(str::to_string),
            );
        }
    }
    for code in blocks {
        for c in NAME.captures_iter(code) {
            if c.get(1).is_some() {
                continue;
            }
            let Some(root) = c.get(2).map(|m| m.as_str()) else {
                continue;
            };
            let after = c.get(4).map(|m| m.as_str());
            if after == Some("=")
                || WORDS.contains(&root.to_lowercase().as_str())
                || bound.contains(root)
            {
                continue;
            }
            let mut segments: Vec<String> = SEGMENT
                .captures_iter(c.get(3).map_or("", |m| m.as_str()))
                .filter_map(|s| s.get(1).or_else(|| s.get(2)).or_else(|| s.get(3)))
                .map(|m| m.as_str().to_string())
                .collect();
            if after == Some("(") {
                // A call: of a function when nothing is chained on it, else of a method, whose
                // name is not an attribute to look for.
                if segments.pop().is_none() {
                    continue;
                }
            } else if GUARDED.is_match(&code[c.get(0).map_or(0, |m| m.end())..]) {
                continue;
            }
            let mut value = ctx.get_attr(root).unwrap_or(minijinja::Value::UNDEFINED);
            if value.is_undefined() {
                return Some(format!("'{root}' is undefined"));
            }
            for segment in segments {
                let next = value
                    .get_attr(&segment)
                    .unwrap_or(minijinja::Value::UNDEFINED);
                if next.is_undefined() {
                    return Some(format!(
                        "object of type '{}' has no attribute '{segment}'",
                        py_type(&value)
                    ));
                }
                value = next;
            }
        }
    }
    None
}

/// `error` with its sentence replaced by the one that names what was missing, when it is the
/// engine's bare one and `source` says which read it was.
pub(super) fn named(error: TemplateError, source: &str, ctx: &minijinja::Value) -> TemplateError {
    let bare = error.0.strip_prefix(UNDEFINED).is_some_and(|rest| {
        rest.trim_start()
            .starts_with("The error was: undefined value")
    });
    if !bare {
        return error;
    }
    let found = reason(source, ctx);
    // Reading a variable can render it, which may leave a failure behind for the next render.
    let _ = take_failure(ctx);
    found.map_or(error, TemplateError)
}
