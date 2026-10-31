// SPDX-License-Identifier: GPL-3.0-or-later
//! Naming what an undefined read could not find, as ansible-core words it: the variable
//! (`'nope' is undefined`), or the attribute with the type it was read from (`object of type
//! 'dict' has no attribute 'b'`). The engine raises one sentence for both and does not say which
//! name it was, so the source is read back against the variables the render saw.

use std::cell::RefCell;
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
        r#"'[^']*'|"[^"]*"|(\.\s*|\|\s*|\bis\s+(?:not\s+)?)?\b([A-Za-z_]\w*)((?:\s*\.\s*(?:[A-Za-z_]\w*|\d+)|\s*\[\s*(?:'[^']*'|"[^"]*"|-?\d+)\s*\])*)\s*(\(|==|=)?"#,
    )
    .expect("a fixed pattern")
});

static SEGMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"\.\s*([A-Za-z_]\w*)|\[\s*(?:'([^']*)'|"([^"]*)")\s*\]|\.\s*(\d+)|\[\s*(-?\d+)\s*\]"#,
    )
    .expect("a fixed pattern")
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

/// One read chained on a name: an attribute or key, or a number.
enum Segment {
    Key(String),
    Index(i64),
}

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

/// The item at `i` of a list or a string, a negative `i` counted from the end, or what
/// ansible-core 2.19.12 says when there is none: an empty list is one of its lazy lists, and any
/// other list or string answers as an attribute read. A mapping read by a number is not worded
/// here, its sentence depending on where the mapping came from.
fn element(value: &minijinja::Value, i: i64) -> Result<minijinja::Value, Option<String>> {
    let lazy = "ansible._internal._templating._lazy_containers._AnsibleLazyTemplateList";
    if !matches!(value.kind(), ValueKind::Seq | ValueKind::String) {
        let next = value
            .get_item(&minijinja::Value::from(i))
            .unwrap_or(minijinja::Value::UNDEFINED);
        return if next.is_undefined() {
            Err(None)
        } else {
            Ok(next)
        };
    }
    let len = value.len().and_then(|n| i64::try_from(n).ok()).unwrap_or(0);
    let at = if i < 0 { i + len } else { i };
    if (0..len).contains(&at) {
        return value
            .get_item(&minijinja::Value::from(at))
            .map_err(|_| None);
    }
    if len == 0 && value.kind() == ValueKind::Seq {
        return Err(Some(format!("{lazy} object has no element {i}")));
    }
    Err(Some(format!(
        "object of type '{}' has no attribute {i}",
        py_type(value)
    )))
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
            let mut segments: Vec<Segment> = SEGMENT
                .captures_iter(c.get(3).map_or("", |m| m.as_str()))
                .filter_map(
                    |s| match s.get(1).or_else(|| s.get(2)).or_else(|| s.get(3)) {
                        Some(key) => Some(Segment::Key(key.as_str().to_string())),
                        None => s
                            .get(4)
                            .or_else(|| s.get(5))?
                            .as_str()
                            .parse()
                            .ok()
                            .map(Segment::Index),
                    },
                )
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
                // A variable whose own template read a name with no value: that read is named.
                let inner = ctx
                    .downcast_object_ref::<super::Context>()
                    .and_then(|c| c.undefined_in(root));
                let said = inner.as_ref().map(|(said, _)| said.clone());
                super::written::set_inner(inner);
                return Some(said.unwrap_or_else(|| format!("'{root}' is undefined")));
            }
            for segment in segments {
                value = match segment {
                    Segment::Key(key) => {
                        let next = value.get_attr(&key).unwrap_or(minijinja::Value::UNDEFINED);
                        if next.is_undefined() {
                            return Some(format!(
                                "object of type '{}' has no attribute '{key}'",
                                py_type(&value)
                            ));
                        }
                        next
                    }
                    Segment::Index(i) => match element(&value, i) {
                        Ok(next) => next,
                        Err(said) => return said,
                    },
                };
            }
        }
    }
    None
}

thread_local! {
    /// What the last value a plugin handed back undefined on purpose says when it is read, as
    /// `lookup('env', 'X', default=undef())` does for a variable that is not set.
    static HANDED: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// An undefined value that says `why` when it is read: what the reference's `_undef(why)` is.
pub(super) fn undefined_saying(why: String) -> minijinja::Value {
    HANDED.with(|h| *h.borrow_mut() = Some(why));
    minijinja::Value::UNDEFINED
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
    super::written::set_inner(None);
    let handed = HANDED.with(|h| h.borrow_mut().take());
    let found = reason(source, ctx).or(handed);
    // Reading a variable can render it, which may leave a failure behind for the next render.
    let _ = take_failure(ctx);
    found.map_or(error, TemplateError)
}
