// SPDX-License-Identifier: GPL-3.0-or-later
//! A unary `+`, which Jinja accepts before an operand as Python does and minijinja's parser
//! refuses. The sign is dropped before the template compiles: `+2.5` is `2.5`.

use std::borrow::Cow;

use minijinja::machinery::{Token, tokenize};
use minijinja::syntax::SyntaxConfig;

/// Whether an operand starts after `token`, so that a `+` there is a sign: the start of an
/// expression, an opening bracket, a separator, an operator or a keyword that takes an operand.
fn operand_follows(token: Option<&Token<'_>>) -> bool {
    match token {
        None => true,
        Some(Token::Ident(name)) => {
            matches!(*name, "not" | "and" | "or" | "in" | "if" | "elif" | "else")
        }
        Some(token) => matches!(
            token,
            Token::VariableStart
                | Token::BlockStart
                | Token::ParenOpen
                | Token::BracketOpen
                | Token::BraceOpen
                | Token::Comma
                | Token::Colon
                | Token::Assign
                | Token::Plus
                | Token::Minus
                | Token::Mul
                | Token::Div
                | Token::FloorDiv
                | Token::Pow
                | Token::Mod
                | Token::Tilde
                | Token::Eq
                | Token::Ne
                | Token::Lt
                | Token::Lte
                | Token::Gt
                | Token::Gte
        ),
    }
}

/// `source` without the `+` of each unary plus. `in_expr` says `source` is one expression rather
/// than a template. A source the tokenizer refuses is handed back as it is, for the compiler to
/// report.
pub(in crate::template) fn without_unary_plus(
    source: &str,
    in_expr: bool,
    syntax: SyntaxConfig,
) -> Cow<'_, str> {
    if !source.contains('+') {
        return Cow::Borrowed(source);
    }
    let Ok(tokens) = tokenize(source, in_expr, syntax).collect::<Result<Vec<_>, _>>() else {
        return Cow::Borrowed(source);
    };
    let mut out = String::new();
    let mut from = 0;
    for (i, (token, span)) in tokens.iter().enumerate() {
        let before = i.checked_sub(1).map(|b| &tokens[b].0);
        if !matches!(token, Token::Plus) || !operand_follows(before) {
            continue;
        }
        let (start, end) = (span.start_offset as usize, span.end_offset as usize);
        out.push_str(&source[from..start]);
        from = end;
    }
    if from == 0 {
        return Cow::Borrowed(source);
    }
    out.push_str(&source[from..]);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use minijinja::syntax::SyntaxConfig;
    use rstest::rstest;

    use super::without_unary_plus;

    #[rstest]
    #[case("{{ +2.5 }}", "{{ 2.5 }}")]
    #[case("{{ [-2.5, +2.5, 1 + 2.5] }}", "{{ [-2.5, 2.5, 1 + 2.5] }}")]
    #[case("{{ 1 + +x }}", "{{ 1 + x }}")]
    #[case("{{ a + b }}", "{{ a + b }}")]
    #[case("a + {{ (+1) }}", "a + {{ (1) }}")]
    #[case("{{ '+1' }}", "{{ '+1' }}")]
    fn only_a_sign_is_dropped(#[case] source: &str, #[case] want: &str) {
        assert_eq!(
            without_unary_plus(source, false, SyntaxConfig::default()),
            want
        );
    }

    #[test]
    fn an_expression_may_start_with_a_sign() {
        assert_eq!(
            without_unary_plus("+1 == 1", true, SyntaxConfig::default()),
            "1 == 1"
        );
    }
}
