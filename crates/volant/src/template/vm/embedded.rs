// SPDX-License-Identifier: GPL-3.0-or-later
//! A template written inside a constant string passed to a lookup: `lookup('items', '{{ [a, 42]
//! }}')`. ansible-core 2.19.12 renders such a string, with a deprecation warning, before the
//! lookup reads it. Only the text of the template itself is rendered this way, never a value
//! that reaches the lookup at run time.
//!
//! The string is rewritten into the expression it holds before the template compiles: `'{{ [a,
//! 42] }}'` becomes `([a, 42])` and `'x{{ 1 }}'` becomes `'x' ~ (1)`. A string with a `{%` or
//! `{#` block, or with an escape, is left as it is.

use std::borrow::Cow;

use minijinja::machinery::{Token, tokenize};
use minijinja::syntax::SyntaxConfig;

/// The warning the reference shows for each template that holds such a string.
pub(crate) const WARNING: &str = "Jinja constant strings should not contain embedded templates. This feature will be disabled by default in ansible-core 2.23.";

/// The help line under [`WARNING`].
pub(crate) const HELP: &str = "Use inline expressions, for example: `msg: \"{{ lookup('env', '{{ a_var }}') }}\"` becomes `msg: \"{{ lookup('env', a_var) }}\"`";

/// The expression `text` holds, or `None` when it holds no `{{ }}`, or something this rewrite
/// does not take apart.
fn expression(text: &str) -> Option<String> {
    if !text.contains("{{") || text.contains("{%") || text.contains("{#") {
        return None;
    }
    let mut parts = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("{{") {
        if open > 0 {
            parts.push(quoted(&rest[..open]));
        }
        let close = rest[open..].find("}}")? + open;
        parts.push(format!("({})", rest[open + 2..close].trim()));
        rest = &rest[close + 2..];
    }
    if !rest.is_empty() {
        parts.push(quoted(rest));
    }
    Some(parts.join(" ~ "))
}

fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// `source` with each constant string that is a term of a `lookup`, `query` or `q` call, or an
/// item of a list that is one, written as the expression it holds. `in_expr` says `source` is
/// one expression rather than a template. A source the tokenizer refuses is handed back as it
/// is, for the compiler to report.
pub(in crate::template) fn inline_lookup_terms(
    source: &str,
    in_expr: bool,
    syntax: SyntaxConfig,
) -> Cow<'_, str> {
    if !(source.contains("lookup") || source.contains("query") || source.contains("q("))
        || source.matches("{{").count() < usize::from(!in_expr) + 1
    {
        return Cow::Borrowed(source);
    }
    let Ok(tokens) = tokenize(source, in_expr, syntax).collect::<Result<Vec<_>, _>>() else {
        return Cow::Borrowed(source);
    };
    let mut out = String::new();
    let mut from = 0;
    // The brackets open inside the lookup call being read, its own `(` first; `true` for a `[`.
    let mut open: Vec<bool> = Vec::new();
    let mut named = false;
    for (i, (token, span)) in tokens.iter().enumerate() {
        let before = i.checked_sub(1).map(|b| &tokens[b].0);
        if open.is_empty() {
            if named && matches!(token, Token::ParenOpen) {
                open.push(false);
            }
            named = matches!(token, Token::Ident("lookup" | "query" | "q"))
                && !matches!(before, Some(Token::Dot));
            continue;
        }
        match token {
            Token::ParenOpen | Token::BraceOpen => open.push(false),
            Token::BracketOpen => open.push(true),
            Token::ParenClose | Token::BracketClose | Token::BraceClose => {
                open.pop();
            }
            Token::Str(text) => {
                let term = open.len() == 1 || (open.len() == 2 && open[1]);
                let keyword = matches!(before, Some(Token::Assign));
                if let (true, false, Some(expr)) = (term, keyword, expression(text)) {
                    let (start, end) = (span.start_offset as usize, span.end_offset as usize);
                    out.push_str(&source[from..start]);
                    out.push('(');
                    out.push_str(&expr);
                    out.push(')');
                    from = end;
                }
            }
            _ => {}
        }
    }
    if from == 0 {
        return Cow::Borrowed(source);
    }
    out.push_str(&source[from..]);
    Cow::Owned(out)
}

/// Whether the template `source` passes a lookup a constant string that holds a template.
pub(crate) fn embeds(source: &str) -> bool {
    matches!(
        inline_lookup_terms(source, false, SyntaxConfig::default()),
        Cow::Owned(_)
    )
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// A term written as a constant string holding a template is inlined; a string anywhere else,
    /// or one this rewrite cannot take apart, stays as written.
    #[rstest]
    #[case("lookup('items', '{{ [a, 42] }}')", "lookup('items', (([a, 42])))")]
    #[case("query('items', 'x{{ 1 }}')", "query('items', ('x' ~ (1)))")]
    #[case("lookup('items', ['{{ v }}'])", "lookup('items', [((v))])")]
    #[case(
        "lookup('env', 'X', default='{{ v }}')",
        "lookup('env', 'X', default='{{ v }}')"
    )]
    #[case("lookup('items', f('{{ v }}'))", "lookup('items', f('{{ v }}'))")]
    #[case("'{{ v }}' | upper", "'{{ v }}' | upper")]
    #[case(
        "lookup('items', '{% if 1 %}x{% endif %}')",
        "lookup('items', '{% if 1 %}x{% endif %}')"
    )]
    #[case("x.lookup('{{ v }}')", "x.lookup('{{ v }}')")]
    fn only_a_constant_term_of_a_lookup_is_inlined(#[case] expr: &str, #[case] want: &str) {
        assert_eq!(
            inline_lookup_terms(expr, true, SyntaxConfig::default()),
            want
        );
    }
}
