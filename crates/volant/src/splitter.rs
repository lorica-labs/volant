// SPDX-License-Identifier: GPL-3.0-or-later
//! `key=value` strings, split the way ansible-core's `parsing/splitter.py` splits them: on spaces,
//! with a quoted word or a `{{ }}`, `{% %}` or `{# #}` block kept whole even when it holds spaces.

use std::sync::LazyLock;

use anyhow::bail;
use regex::Regex;

/// `split_args`: the words of `args`, quotes left in place. A word opened by a quote or a Jinja
/// block runs on to the space that follows its close, so `a={{ b ~ 'c' }}` is one word.
pub fn split_args(args: &str) -> anyhow::Result<Vec<String>> {
    let mut params: Vec<String> = Vec::new();
    if args.is_empty() {
        return Ok(params);
    }
    let items: Vec<&str> = args.split('\n').collect();
    let mut quote = None;
    let mut inside_quotes = false;
    let (mut print, mut block, mut comment) = (0, 0, 0);
    for (item_idx, item) in items.iter().enumerate() {
        let mut line_continuation = false;
        for (idx, token) in item.split(' ').enumerate() {
            if token.is_empty() && idx != 0 {
                last(&mut params).push(' ');
                continue;
            }
            if token == "\\" && !inside_quotes {
                line_continuation = true;
                continue;
            }
            let was_inside_quotes = inside_quotes;
            quote = quote_state(token, quote);
            inside_quotes = quote.is_some();
            let in_block = print + block + comment > 0;
            let mut appended = false;
            if inside_quotes && !was_inside_quotes && !in_block {
                params.push(token.to_string());
                appended = true;
            } else if in_block || inside_quotes || was_inside_quotes {
                let word = last(&mut params);
                if idx > 0 {
                    word.push(' ');
                }
                word.push_str(token);
                appended = true;
            }
            for (depth, open, close) in [
                (&mut print, "{{", "}}"),
                (&mut block, "{%", "%}"),
                (&mut comment, "{#", "#}"),
            ] {
                let before = *depth;
                *depth = count_blocks(token, *depth, open, close);
                if *depth != before && !appended {
                    params.push(token.to_string());
                    appended = true;
                }
            }
            if print + block + comment == 0 && !inside_quotes && !appended && !token.is_empty() {
                params.push(token.to_string());
            }
        }
        if items.len() > 1 && item_idx != items.len() - 1 && !line_continuation {
            last(&mut params).push('\n');
        }
    }
    if print + block + comment > 0 || inside_quotes {
        bail!("failed at splitting arguments, either an unbalanced jinja2 block or quotes: {args}");
    }
    Ok(params)
}

/// The `key=value` pairs of a string, in the order they were written.
pub type Options = Vec<(String, String)>;

/// `parse_kv` without `check_raw`: the `key=value` words, the value unquoted and its escapes
/// decoded, and the other words joined back as `_raw_params` would hold them (`None` when there
/// is none). A key repeated keeps its last value.
pub fn parse_kv(args: &str) -> anyhow::Result<(Options, Option<String>)> {
    let mut options = Options::new();
    let mut raw = Vec::new();
    for orig in split_args(args)? {
        let x = decode_escapes(&orig);
        if !x.contains('=') {
            raw.push(orig);
            continue;
        }
        // The first `=` past the first character that no backslash escapes.
        let pos = x
            .char_indices()
            .skip(1)
            .find(|&(i, c)| c == '=' && !x[..i].ends_with('\\'))
            .map(|(i, _)| i);
        let Some(pos) = pos else {
            raw.push(x.replace("\\=", "="));
            continue;
        };
        let key = x[..pos].trim().to_string();
        let value = unquote(x[pos + 1..].trim()).to_string();
        options.retain(|(k, _)| *k != key);
        options.push((key, value));
    }
    let raw = (!raw.is_empty()).then(|| join_args(&raw));
    Ok((options, raw))
}

/// `join_args`: the words back into one string, a space between two unless the first ends a line.
fn join_args(words: &[String]) -> String {
    let mut out = String::new();
    for word in words {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

fn last(params: &mut Vec<String>) -> &mut String {
    if params.is_empty() {
        params.push(String::new());
    }
    params.last_mut().expect("pushed above")
}

/// `_get_quote_state`: the quote still open at the end of `token`, given the one open before it.
fn quote_state(token: &str, mut quote: Option<char>) -> Option<char> {
    let mut prev = None;
    for c in token.chars() {
        if matches!(c, '"' | '\'') && prev != Some('\\') {
            match quote {
                Some(q) if q == c => quote = None,
                Some(_) => {}
                None => quote = Some(c),
            }
        }
        prev = Some(c);
    }
    quote
}

/// `_count_jinja2_blocks`: the depth after `token`, never below zero.
fn count_blocks(token: &str, depth: usize, open: &str, close: &str) -> usize {
    (depth + token.matches(open).count()).saturating_sub(token.matches(close).count())
}

/// `quoting.unquote`: one pair of matching outer quotes removed, unless the closing one is escaped.
fn unquote(data: &str) -> &str {
    let b = data.as_bytes();
    if b.len() > 1
        && b[0] == b[b.len() - 1]
        && matches!(b[0], b'"' | b'\'')
        && b[b.len() - 2] != b'\\'
    {
        &data[1..data.len() - 1]
    } else {
        data
    }
}

/// `_decode_escapes`: Python's string escapes decoded. `\N{name}` is left as written: resolving a
/// character by its Unicode name needs a table the engine does not carry.
fn decode_escapes(s: &str) -> String {
    static ESCAPE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"\\U[0-9a-fA-F]{8}|\\u[0-9a-fA-F]{4}|\\x[0-9a-fA-F]{2}|\\[\\'"abfnrtv]"#)
            .expect("valid pattern")
    });
    ESCAPE
        .replace_all(s, |c: &regex::Captures<'_>| {
            let m = &c[0];
            let decoded = match &m[1..2] {
                "U" | "u" | "x" => u32::from_str_radix(&m[2..], 16)
                    .ok()
                    .and_then(char::from_u32),
                "a" => Some('\x07'),
                "b" => Some('\x08'),
                "f" => Some('\x0c'),
                "n" => Some('\n'),
                "r" => Some('\r'),
                "t" => Some('\t'),
                "v" => Some('\x0b'),
                other => other.chars().next(),
            };
            decoded.map_or_else(|| m.to_string(), String::from)
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(args: &str) -> Vec<(String, String)> {
        parse_kv(args).unwrap().0
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Measured on ansible-core 2.19.12 with `set_fact:` written as one string: a block with
    /// spaces inside stays one value, quotes around a whole value go, quotes inside one stay,
    /// `\t` becomes a tab and `\=` stays as written in a value.
    #[test]
    fn a_jinja_block_with_spaces_stays_one_value() {
        assert_eq!(
            kv(
                r#"a={{ base ~ '/x' }} b="p q" c='r s' d={% if true %}yes{% endif %} e=x\ty f=k\=v"#
            ),
            pairs(&[
                ("a", "{{ base ~ '/x' }}"),
                ("b", "p q"),
                ("c", "r s"),
                ("d", "{% if true %}yes{% endif %}"),
                ("e", "x\ty"),
                ("f", r"k\=v"),
            ])
        );
        assert_eq!(
            kv(r#"g={{ dict(k=1) }} h=' t ' i=u"v w""#),
            pairs(&[("g", "{{ dict(k=1) }}"), ("h", " t "), ("i", r#"u"v w""#)])
        );
    }

    #[test]
    fn words_without_a_key_are_joined_back_as_raw_params() {
        assert_eq!(
            parse_kv("echo {{ a b }} x=1 'c d'").unwrap(),
            (
                pairs(&[("x", "1")]),
                Some("echo {{ a b }} 'c d'".to_string())
            )
        );
        assert_eq!(parse_kv("").unwrap(), (Vec::new(), None));
    }

    #[test]
    fn an_unclosed_block_or_quote_fails_the_split() {
        for bad in ["a={{ b", "a='b c", "a={% if x"] {
            let err = parse_kv(bad).unwrap_err().to_string();
            assert!(
                err.starts_with("failed at splitting arguments"),
                "{bad}: {err}"
            );
        }
    }
}
