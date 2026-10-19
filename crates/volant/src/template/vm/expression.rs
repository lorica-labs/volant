// SPDX-License-Identifier: GPL-3.0-or-later
//! Telling a one-expression template from text with expressions in it.

/// `{{ expr }}` and nothing else: no text around, no second expression, no statement. A `}}`
/// inside is no end when it closes a bracket the expression opened or sits in a string literal:
/// `{'a': {'x': 5}}` closing on `}}}` and `'}}'` are one expression, `1 }}{{ 2` is two. A `-`
/// right after `{{` or right before `}}` is whitespace control, never a minus sign.
pub(in crate::template) fn single_expression(text: &str) -> Option<&str> {
    let t = text.trim();
    let inner = t.strip_prefix("{{")?.strip_suffix("}}")?;
    if !closes_only_what_it_opens(inner) {
        return None;
    }
    let inner = inner.strip_prefix('-').unwrap_or(inner);
    let inner = inner.strip_suffix('-').unwrap_or(inner);
    Some(inner.trim())
}

/// Whether every bracket `expr` closes outside a string literal is one it opened, and no
/// statement or comment opens there. A `{%` inside a string literal is text. minijinja's own
/// parser cannot be asked: its lexer panics on a `}` with nothing open.
fn closes_only_what_it_opens(expr: &str) -> bool {
    let mut depth = 0usize;
    let mut quote = None;
    let mut chars = expr.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(_), '\\') => {
                chars.next();
            }
            (Some(q), c) if c == q => quote = None,
            (None, '\'' | '"') => quote = Some(c),
            (None, '{') if matches!(chars.peek(), Some('%' | '#')) => return false,
            (None, '{' | '[' | '(') => depth += 1,
            (None, '}' | ']' | ')') => match depth.checked_sub(1) {
                Some(d) => depth = d,
                None => return false,
            },
            _ => {}
        }
    }
    true
}
