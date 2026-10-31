// SPDX-License-Identifier: GPL-3.0-or-later
//! The name ansible-core gives a string in the refusal to order two values. A string read from
//! a variable the playbook or the inventory wrote carries ansible-core's tags and is a
//! `_AnsibleTaggedStr`; a string built in the template, a result a managed host sent and a magic
//! variable are plain `str`. Measured on ansible-core 2.19.12.
//!
//! The engine's values carry no such tag, so the operands are read back from the template's
//! text: an operand that is a path into a variable, `s`, `l[0]`, `d.k`, `hostvars[h].v`, is
//! tagged when its variable is.

use minijinja::machinery::{Token, tokenize};
use minijinja::syntax::SyntaxConfig;

use super::super::TemplateError;
use super::super::context::Context;

/// The variables ansible-core sets itself, whose strings are not tagged.
const MAGIC: [&str; 18] = [
    "ansible_check_mode",
    "ansible_diff_mode",
    "ansible_forks",
    "ansible_play_batch",
    "ansible_play_hosts",
    "ansible_play_hosts_all",
    "ansible_play_name",
    "ansible_run_tags",
    "ansible_skip_tags",
    "ansible_verbosity",
    "group_names",
    "groups",
    "inventory_dir",
    "inventory_file",
    "inventory_hostname",
    "inventory_hostname_short",
    "play_hosts",
    "playbook_dir",
];

const TAGGED: &str = "'_AnsibleTaggedStr'";

/// `error` with each `'str'` operand of an ordering refusal named as ansible-core names it, when
/// `source` says the operand is a path into a variable the playbook wrote.
pub(in crate::template) fn tagged(
    error: TemplateError,
    source: &str,
    ctx: &minijinja::Value,
) -> TemplateError {
    let text = &error.0;
    let Some((head, operands)) = text.split_once(" not supported between instances of ") else {
        return error;
    };
    let Some((left, right)) = operands.split_once(" and ") else {
        return error;
    };
    if left != "'str'" && right != "'str'" {
        return error;
    }
    let symbol = head.rsplit('\'').nth(1).unwrap_or_default();
    // The comparison that failed is taken to be the first whose `str` side reads a variable: in
    // `1 < 2 < s` that is the second.
    let Some((left_root, right_root)) = operand_roots(source, symbol)
        .into_iter()
        .find(|(l, r)| (left == "'str'" && l.is_some()) || (right == "'str'" && r.is_some()))
    else {
        return error;
    };
    let context = ctx.downcast_object_ref::<Context>();
    let written = |root: Option<String>| {
        root.is_some_and(|name| {
            !MAGIC.contains(&name.as_str()) && !context.is_some_and(|c| c.untrusted.contains(&name))
        })
    };
    let name = |given: &str, root: Option<String>| {
        if given == "'str'" && written(root) {
            TAGGED.to_string()
        } else {
            given.to_string()
        }
    };
    TemplateError(format!(
        "{head} not supported between instances of {} and {}",
        name(left, left_root),
        name(right, right_root)
    ))
}

fn operator(token: &Token<'_>) -> Option<&'static str> {
    Some(match token {
        Token::Lt => "<",
        Token::Lte => "<=",
        Token::Gt => ">",
        Token::Gte => ">=",
        _ => return None,
    })
}

/// Whether a path read across `token` binds tighter than a comparison: a filter, a test, an
/// operator or a call around it makes the operand something else than the variable.
fn breaks_path(token: Option<&Token<'_>>) -> bool {
    matches!(
        token,
        Some(
            Token::Pipe
                | Token::Plus
                | Token::Minus
                | Token::Mul
                | Token::Div
                | Token::FloorDiv
                | Token::Pow
                | Token::Mod
                | Token::Tilde
                | Token::ParenOpen
                | Token::Dot
        )
    )
}

/// The variable at the root of each operand of each comparison by `symbol` in `source`, when
/// that operand is a path into it and nothing more.
fn operand_roots(source: &str, symbol: &str) -> Vec<(Option<String>, Option<String>)> {
    let in_expr = !source.contains("{{") && !source.contains("{%");
    let Ok(tokens) = tokenize(source, in_expr, SyntaxConfig::default())
        .map(|t| t.map(|(token, _)| token))
        .collect::<Result<Vec<_>, _>>()
    else {
        return Vec::new();
    };
    (0..tokens.len())
        .filter(|&at| operator(&tokens[at]) == Some(symbol))
        .map(|at| (root_before(&tokens, at), root_after(&tokens, at)))
        .collect()
}

/// The root of the path that ends just before `at`.
fn root_before(tokens: &[Token<'_>], at: usize) -> Option<String> {
    let mut i = at;
    let mut depth = 0usize;
    // What a lookup answers is tagged too: `lookup('env', 'HOME')` before the operator.
    if matches!(tokens.get(at.checked_sub(1)?), Some(Token::ParenClose)) {
        let mut open = 0usize;
        for j in (0..at).rev() {
            match tokens[j] {
                Token::ParenClose => open += 1,
                Token::ParenOpen => {
                    open -= 1;
                    if open == 0 {
                        let call = j.checked_sub(1).map(|n| &tokens[n]);
                        let before = j.checked_sub(2).map(|n| &tokens[n]);
                        return matches!(call, Some(Token::Ident("lookup" | "query" | "q")))
                            .then(|| "lookup".to_string())
                            .filter(|_| !breaks_path(before));
                    }
                }
                _ => {}
            }
        }
        return None;
    }
    // Walk back over `.name` and `[...]` to the name the path starts with.
    loop {
        i = i.checked_sub(1)?;
        match &tokens[i] {
            Token::BracketClose => depth += 1,
            Token::BracketOpen if depth > 0 => depth -= 1,
            Token::Ident(name) if depth == 0 => {
                let before = i.checked_sub(1).map(|b| &tokens[b]);
                if matches!(before, Some(Token::Dot)) {
                    continue;
                }
                if breaks_path(before) {
                    return None;
                }
                return Some((*name).to_string());
            }
            Token::Dot if depth == 0 => {}
            _ if depth > 0 => {}
            _ => return None,
        }
    }
}

/// The root of the path that starts just after `at`.
fn root_after(tokens: &[Token<'_>], at: usize) -> Option<String> {
    let Some(Token::Ident(name)) = tokens.get(at + 1) else {
        return None;
    };
    let mut i = at + 2;
    let mut depth = 0usize;
    while let Some(token) = tokens.get(i) {
        match token {
            Token::BracketOpen => depth += 1,
            Token::BracketClose if depth > 0 => depth -= 1,
            Token::Dot if depth == 0 => {}
            Token::Ident(_) if depth == 0 && matches!(tokens[i - 1], Token::Dot) => {}
            _ if depth > 0 => {}
            other => {
                if breaks_path(Some(other)) {
                    return None;
                }
                break;
            }
        }
        i += 1;
    }
    Some((*name).to_string())
}
