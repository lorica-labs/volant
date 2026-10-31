// SPDX-License-Identifier: GPL-3.0-or-later
//! Keeping a comparison of literals from being worked out while the template compiles.
//!
//! minijinja's compiler replaces a comparison whose operands are all literals by its answer, in
//! its own ordering, before the comparison rewrite can route it to Python's rules: `'a' < 1`
//! answers `false` where ansible-core fails with `'<' not supported between instances of 'str'
//! and 'int'`. A literal on the left of a comparison is written as `(x,)[0]` instead, the same
//! value read out of a one-item tuple, which the compiler does not fold.

use std::borrow::Cow;

use minijinja::machinery::{Token, tokenize};
use minijinja::syntax::SyntaxConfig;

/// Whether `token` is a literal the compiler folds.
fn is_literal(token: &Token<'_>) -> bool {
    match token {
        Token::Str(_) | Token::String(_) | Token::Int(_) | Token::Int128(_) | Token::Float(_) => {
            true
        }
        Token::Ident(name) => {
            matches!(*name, "true" | "false" | "none" | "True" | "False" | "None")
        }
        _ => false,
    }
}

fn is_comparison(token: &Token<'_>) -> bool {
    matches!(
        token,
        Token::Eq | Token::Ne | Token::Lt | Token::Lte | Token::Gt | Token::Gte
    )
}

/// Whether a literal after `token` belongs to something that binds tighter than a comparison:
/// an operator, an attribute or filter, or a string literal it is joined to.
fn binds_tighter(token: &Token<'_>) -> bool {
    matches!(
        token,
        Token::Dot
            | Token::Pipe
            | Token::Plus
            | Token::Minus
            | Token::Mul
            | Token::Div
            | Token::FloorDiv
            | Token::Pow
            | Token::Mod
            | Token::Tilde
            | Token::Str(_)
            | Token::String(_)
    )
}

/// `source` with each literal that is the left operand of a comparison written as `(x,)[0]`.
/// `in_expr` says `source` is one expression rather than a template. A source the tokenizer
/// refuses is handed back as it is, for the compiler to report.
pub(in crate::template) fn unfold_comparisons(
    source: &str,
    in_expr: bool,
    syntax: SyntaxConfig,
) -> Cow<'_, str> {
    if !source.contains(['<', '>', '=']) {
        return Cow::Borrowed(source);
    }
    let Ok(tokens) = tokenize(source, in_expr, syntax).collect::<Result<Vec<_>, _>>() else {
        return Cow::Borrowed(source);
    };
    let mut out = String::new();
    let mut from = 0;
    for (i, (token, span)) in tokens.iter().enumerate() {
        let left = is_literal(token)
            && tokens
                .get(i + 1)
                .is_some_and(|(next, _)| is_comparison(next))
            && !(i > 0 && binds_tighter(&tokens[i - 1].0));
        if !left {
            continue;
        }
        let (start, end) = (span.start_offset as usize, span.end_offset as usize);
        out.push_str(&source[from..start]);
        out.push('(');
        out.push_str(&source[start..end]);
        out.push_str(",)[0]");
        from = end;
    }
    if from == 0 {
        return Cow::Borrowed(source);
    }
    out.push_str(&source[from..]);
    Cow::Owned(out)
}
