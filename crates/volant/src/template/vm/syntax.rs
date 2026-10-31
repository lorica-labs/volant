// SPDX-License-Identifier: GPL-3.0-or-later
//! A syntax error in Jinja's words. minijinja says what it expected in its own terms
//! (`unexpected end of input, expected expression (in <expression>:1)`); ansible-core prints the
//! sentence of Jinja's parser behind `Syntax error in expression:` or `Syntax error in
//! template:` (`unexpected 'end of template'`). Measured on ansible-core 2.19.12 for the errors
//! a playbook commonly makes; any other keeps minijinja's sentence.

use std::cell::Cell;

use minijinja::ErrorKind;

thread_local! {
    /// Set while a template that is one `{{ }}` is evaluated as the expression inside it: its
    /// errors are a template's, and its positions count from `offset` in the template.
    static TEMPLATE: Cell<Option<usize>> = const { Cell::new(None) };
}

/// Runs `evaluate` on `inner`, the expression of `template`, a one-expression template and a
/// text `inner` is a slice of.
pub(in crate::template) fn as_template<T>(
    template: &str,
    inner: &str,
    evaluate: impl FnOnce() -> T,
) -> T {
    let at = (inner.as_ptr() as usize).wrapping_sub(template.as_ptr() as usize);
    let offset = template.get(..at).map_or(0, |head| head.chars().count());
    let before = TEMPLATE.replace(Some(offset));
    let out = evaluate();
    TEMPLATE.set(before);
    out
}

/// `err` as a bare expression says it. Measured on ansible-core 2.19.12: a filter or a test
/// nobody ships is an expression's syntax error in a condition, and a template's elsewhere.
pub(in crate::template) fn as_expression(err: super::TemplateError) -> super::TemplateError {
    match err.0.strip_prefix("Syntax error in template: No ") {
        Some(rest) => super::TemplateError(format!("Syntax error in expression: No {rest}")),
        None => err,
    }
}

/// What a block that is open waits for, as Jinja's parser lists it.
fn awaited(block: &str) -> Option<&'static str> {
    Some(match block {
        "if" => "'elif' or 'else' or 'endif'",
        "for" => "'endfor' or 'else'",
        "macro" => "'endmacro'",
        "block" => "'endblock'",
        "filter" => "'endfilter'",
        "call" => "'endcall'",
        "with" => "'endwith'",
        "set" => "'endset'",
        "autoescape" => "'endautoescape'",
        _ => return None,
    })
}

/// The innermost block left open in `text`, and whether its `else` was read, for a `for`.
fn innermost(text: &str) -> Option<(String, bool)> {
    let mut open: Vec<(String, bool)> = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("{%") {
        rest = &rest[at + 2..];
        let body = rest.trim_start_matches(['-', '+']).trim_start();
        let name: String = body
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        match name.as_str() {
            "set" => {
                // Only `{% set x %}` opens a block; `{% set x = 1 %}` does not.
                let tag = body.split("%}").next().unwrap_or_default();
                if !tag.contains('=') {
                    open.push((name, false));
                }
            }
            "else" => {
                if let Some(last) = open.last_mut() {
                    last.1 = true;
                }
            }
            n if n.starts_with("end") => {
                open.pop();
            }
            n if awaited(n).is_some() => open.push((name, false)),
            _ => {}
        }
    }
    open.pop()
}

/// `Jinja was looking for ...`, for the block open in `text`.
fn looking_for(text: &str) -> String {
    match innermost(text) {
        Some((block, after_else)) => {
            let tags = if block == "for" && after_else {
                "'endfor'"
            } else {
                awaited(&block).unwrap_or_default()
            };
            format!(
                " Jinja was looking for the following tags: {tags}. The innermost block that needs to be closed is '{block}'."
            )
        }
        None => String::new(),
    }
}

/// The closing bracket of the innermost bracket left open in `expr`.
fn closing(expr: &str) -> char {
    let mut open = Vec::new();
    for c in expr.chars() {
        match c {
            '(' | '[' | '{' => open.push(c),
            ')' | ']' | '}' => {
                open.pop();
            }
            _ => {}
        }
    }
    match open.pop() {
        Some('[') => ']',
        Some('{') => '}',
        _ => ')',
    }
}

/// The token of `source` that starts at byte `at`, as Jinja names it in `got '...'`.
fn token_at(source: &str, at: usize) -> String {
    let rest = source.get(at..).unwrap_or_default().trim_start();
    let word: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if word.is_empty() {
        rest.chars().next().map(String::from).unwrap_or_default()
    } else {
        word
    }
}

/// Jinja's sentence for `err`, raised compiling `source`, with its prefix; `None` when it is no
/// syntax error, or one this module does not word.
pub(in crate::template) fn jinja(err: &minijinja::Error, source: &str) -> Option<String> {
    if err.kind() != ErrorKind::SyntaxError {
        return None;
    }
    let detail = err.detail()?;
    let offset = TEMPLATE.get();
    let template = offset.is_some() || source.contains("{{") || source.contains("{%");
    let start = err.range().map_or(source.len(), |r| r.start);
    let expected = detail.strip_prefix("unexpected end of input, expected ");
    let said = match (detail, expected, offset) {
        // A template that is one expression, evaluated as that expression: its end of input is
        // the `}}` that closes it.
        (_, Some("expression"), Some(_)) => "unexpected 'end of print statement'".to_string(),
        (_, Some("identifier"), Some(_)) => {
            "expected token 'name', got 'end of print statement'".to_string()
        }
        (_, Some(close), Some(_)) if close.starts_with('`') => {
            format!("unexpected '}}', expected '{}'", closing(source))
        }
        ("unexpected input after expression", _, Some(_)) => format!(
            "expected token 'end of print statement', got '{}'",
            token_at(source, start)
        ),
        // A whole template.
        ("unexpected end of variable block", _, None) => {
            "unexpected 'end of print statement'".to_string()
        }
        (_, Some("end of variable block"), None) => {
            "unexpected end of template, expected 'end of print statement'.".to_string()
        }
        // A tag that ends where an expression was due: `{% if %}`.
        ("unexpected end of block", _, None) => {
            "Expected an expression, got 'end of statement block'".to_string()
        }
        (_, Some("end of block"), None) => {
            format!("Unexpected end of template.{}", looking_for(source))
        }
        _ if detail.starts_with("unknown statement ") => format!(
            "Encountered unknown tag '{}'.{}",
            &detail["unknown statement ".len()..],
            looking_for(source.get(..start).unwrap_or(source))
        ),
        // An expression.
        (_, Some("expression"), None) => "unexpected 'end of template'".to_string(),
        (_, Some("identifier"), None) => "unexpected end of template, expected 'name'.".to_string(),
        (_, Some(close), None) if close.starts_with('`') => format!(
            "unexpected end of template, expected '{}'.",
            close.trim_matches('`')
        ),
        ("unexpected input after expression", _, None) => "chunk after expression".to_string(),
        ("unexpected character", _, _) => {
            let at = source.get(..start).map_or(0, |s| s.chars().count()) + offset.unwrap_or(0);
            let c = source.get(start..).and_then(|s| s.chars().next())?;
            format!("unexpected char '{c}' at {at}")
        }
        _ => return None,
    };
    let kind = if template { "template" } else { "expression" };
    Some(format!("Syntax error in {kind}: {said}"))
}
