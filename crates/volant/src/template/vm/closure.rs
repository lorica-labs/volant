// SPDX-License-Identifier: GPL-3.0-or-later
//! The names a file included from a macro reads. Jinja renders the include with the template's
//! own names, an `{% import %}` among them; a minijinja macro keeps only the names its body
//! reads, so the include would see the render's variables in their place. A macro that includes
//! a file gets a block that reads each name assigned before it, which prints nothing and makes
//! the macro keep them.

use std::borrow::Cow;

use minijinja::machinery::{Token, tokenize};
use minijinja::syntax::SyntaxConfig;

/// The names a block assigns at the top level: `import ... as x`, `from ... import a as b, c`,
/// `set x`.
fn assigned<'a>(block: &[&Token<'a>]) -> Vec<&'a str> {
    let name = |t: Option<&&Token<'a>>| match t {
        Some(Token::Ident(n)) => Some(*n),
        _ => None,
    };
    match name(block.first()) {
        Some("import") => block
            .windows(2)
            .filter(|w| matches!(w[0], Token::Ident("as")))
            .filter_map(|w| name(Some(&w[1])))
            .collect(),
        Some("from") => {
            let Some(start) = block
                .iter()
                .position(|t| matches!(t, Token::Ident("import")))
            else {
                return Vec::new();
            };
            let names = &block[start + 1..];
            let end = names
                .iter()
                .position(|t| matches!(t, Token::Ident("with" | "without")))
                .unwrap_or(names.len());
            names[..end]
                .split(|t| matches!(t, Token::Comma))
                .filter_map(|n| name(n.last()))
                .collect()
        }
        Some("set") => name(block.get(1)).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// `source` with a block that reads the names assigned before each top-level macro whose body
/// includes a file. A source the tokenizer refuses is handed back as it is.
pub(in crate::template) fn with_includes_enclosed(
    source: &str,
    syntax: SyntaxConfig,
) -> Cow<'_, str> {
    if !(source.contains("macro") && source.contains("include")) {
        return Cow::Borrowed(source);
    }
    let Ok(tokens) = tokenize(source, false, syntax).collect::<Result<Vec<_>, _>>() else {
        return Cow::Borrowed(source);
    };
    let mut names: Vec<&str> = Vec::new();
    let mut depth = 0usize;
    let mut includes = false;
    let mut inserts: Vec<(usize, String)> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if !matches!(tokens[i].0, Token::BlockStart) {
            i += 1;
            continue;
        }
        let end = tokens[i..]
            .iter()
            .position(|(t, _)| matches!(t, Token::BlockEnd))
            .map_or(tokens.len(), |e| i + e);
        let block: Vec<&Token<'_>> = tokens[i + 1..end].iter().map(|(t, _)| t).collect();
        match block.first() {
            Some(Token::Ident("macro")) => depth += 1,
            Some(Token::Ident("include")) if depth > 0 => includes = true,
            Some(Token::Ident("endmacro")) if depth > 0 => {
                depth -= 1;
                if depth == 0 && includes && !names.is_empty() {
                    let span = tokens[i].1;
                    let open = &source[span.start_offset as usize..span.end_offset as usize];
                    let close = tokens.get(end).map_or("%}", |(_, s)| {
                        source[s.start_offset as usize..s.end_offset as usize]
                            .trim_start_matches(['-', '+'])
                    });
                    let plain = open.trim_end_matches(['-', '+']);
                    let test: Vec<String> =
                        names.iter().map(|n| format!("{n} is defined")).collect();
                    let read = format!(
                        "{open} if {} {close}{plain} endif {close}",
                        test.join(" or ")
                    );
                    inserts.push((span.start_offset as usize, read));
                }
                if depth == 0 {
                    includes = false;
                }
            }
            _ if depth == 0 => names.extend(assigned(&block)),
            _ => {}
        }
        i = end + 1;
    }
    if inserts.is_empty() {
        return Cow::Borrowed(source);
    }
    let mut out =
        String::with_capacity(source.len() + inserts.iter().map(|(_, r)| r.len()).sum::<usize>());
    let mut from = 0;
    for (at, read) in inserts {
        out.push_str(&source[from..at]);
        out.push_str(&read);
        from = at;
    }
    out.push_str(&source[from..]);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use minijinja::syntax::SyntaxConfig;
    use rstest::rstest;

    use super::with_includes_enclosed;

    #[rstest]
    #[case(
        "{% import 'v.j2' as vars %}{% macro m() %}{% include 'i.j2' %}{% endmacro %}",
        "{% import 'v.j2' as vars %}{% macro m() %}{% include 'i.j2' %}\
         {% if vars is defined %}{% endif %}{% endmacro %}"
    )]
    #[case(
        "{% from 'v.j2' import a as b, c with context %}{% set d = 1 %}\
         {% macro m() %}{% include 'i.j2' %}{%- endmacro %}",
        "{% from 'v.j2' import a as b, c with context %}{% set d = 1 %}\
         {% macro m() %}{% include 'i.j2' %}\
         {%- if b is defined or c is defined or d is defined %}{% endif %}{%- endmacro %}"
    )]
    #[case(
        "{% import 'v.j2' as vars %}{% macro m() %}{{ x }}{% endmacro %}",
        "{% import 'v.j2' as vars %}{% macro m() %}{{ x }}{% endmacro %}"
    )]
    #[case(
        "{% macro m() %}{% include 'i.j2' %}{% endmacro %}{% set x = 1 %}",
        "{% macro m() %}{% include 'i.j2' %}{% endmacro %}{% set x = 1 %}"
    )]
    fn a_macro_that_includes_reads_the_names_before_it(#[case] source: &str, #[case] want: &str) {
        assert_eq!(
            with_includes_enclosed(source, SyntaxConfig::default()),
            want
        );
    }
}
