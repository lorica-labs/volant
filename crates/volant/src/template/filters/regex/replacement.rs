// SPDX-License-Identifier: GPL-3.0-or-later
//! The replacement string of `re.sub`: `\1`, `\g<name>`, `\g<0>`, octal escapes and the escapes
//! `\n`, `\t` and friends. Python reads it, not the `regex` crate, which would give `$1` a
//! meaning.

use super::pattern::is_identifier;

enum Piece {
    Text(String),
    Group(usize),
}

pub(super) struct Template {
    pieces: Vec<Piece>,
}

impl Template {
    /// The template of `text` for a pattern with `groups` capture groups after the whole match,
    /// and `names` for the named ones. The error is Python's.
    pub(super) fn parse(
        text: &str,
        groups: usize,
        names: &[(String, usize)],
    ) -> Result<Self, String> {
        let mut pieces = Vec::new();
        let mut literal = String::new();
        let chars: Vec<char> = text.chars().collect();
        let mut at = 0;
        let flush = |literal: &mut String, pieces: &mut Vec<Piece>| {
            if !literal.is_empty() {
                pieces.push(Piece::Text(std::mem::take(literal)));
            }
        };
        let group = |index: usize, at: usize| {
            if index > groups {
                Err(format!("invalid group reference {index} at position {at}"))
            } else {
                Ok(Piece::Group(index))
            }
        };
        while at < chars.len() {
            let c = chars[at];
            at += 1;
            if c != '\\' {
                literal.push(c);
                continue;
            }
            let Some(&e) = chars.get(at) else {
                return Err(format!(
                    "bad escape (end of pattern) at position {}",
                    at - 1
                ));
            };
            at += 1;
            let octal = |c: Option<&char>| c.is_some_and(|c| ('0'..='7').contains(c));
            match e {
                'g' => {
                    let (index, start) = named_group(&chars, &mut at, names)?;
                    flush(&mut literal, &mut pieces);
                    pieces.push(group(index, start)?);
                }
                '0' => {
                    let mut value = 0;
                    for _ in 0..2 {
                        let Some(d) = chars.get(at).filter(|c| octal(Some(c))) else {
                            break;
                        };
                        value = value * 8 + (*d as u32 - '0' as u32);
                        at += 1;
                    }
                    literal.push(char::from_u32(value & 0xff).unwrap_or('\0'));
                }
                '1'..='9' => {
                    let start = at - 1;
                    if chars.get(at).is_some_and(char::is_ascii_digit) {
                        if octal(Some(&e)) && octal(chars.get(at)) && octal(chars.get(at + 1)) {
                            let digits: String = [e, chars[at], chars[at + 1]].iter().collect();
                            at += 2;
                            let value = u32::from_str_radix(&digits, 8).unwrap_or(0);
                            if value > 0o377 {
                                return Err(format!(
                                    "octal escape value \\{digits} outside of range 0-0o377 at position {start}"
                                ));
                            }
                            literal.push(char::from_u32(value).unwrap_or('\0'));
                            continue;
                        }
                        let index =
                            (e as usize - '0' as usize) * 10 + (chars[at] as usize - '0' as usize);
                        at += 1;
                        flush(&mut literal, &mut pieces);
                        pieces.push(group(index, start)?);
                    } else {
                        flush(&mut literal, &mut pieces);
                        pieces.push(group(e as usize - '0' as usize, start)?);
                    }
                }
                'a' => literal.push('\u{7}'),
                'b' => literal.push('\u{8}'),
                'f' => literal.push('\u{c}'),
                'n' => literal.push('\n'),
                'r' => literal.push('\r'),
                't' => literal.push('\t'),
                'v' => literal.push('\u{b}'),
                '\\' => literal.push('\\'),
                e if e.is_ascii_alphabetic() => {
                    return Err(format!("bad escape \\{e} at position {}", at - 2));
                }
                e => {
                    literal.push('\\');
                    literal.push(e);
                }
            }
        }
        flush(&mut literal, &mut pieces);
        Ok(Self { pieces })
    }

    /// The expansion for one match, an unmatched group giving nothing.
    pub(super) fn expand(&self, caps: &super::engine::Captures<'_>, out: &mut String) {
        for piece in &self.pieces {
            match piece {
                Piece::Text(text) => out.push_str(text),
                Piece::Group(index) => {
                    if let Some(m) = caps.get(*index) {
                        out.push_str(m.as_str());
                    }
                }
            }
        }
    }

    /// Whether any piece reads a group, so that a match needs its captures.
    pub(super) fn reads_groups(&self) -> bool {
        self.pieces.iter().any(|p| matches!(p, Piece::Group(_)))
    }

    /// The expansion when no piece reads a group.
    pub(super) fn expand_text(&self, out: &mut String) {
        for piece in &self.pieces {
            if let Piece::Text(text) = piece {
                out.push_str(text);
            }
        }
    }
}

/// The group of `\g<name>` or `\g<number>`, with `at` just after the `g`, and where its name
/// starts. `at` ends after the closing `>`.
fn named_group(
    chars: &[char],
    at: &mut usize,
    names: &[(String, usize)],
) -> Result<(usize, usize), String> {
    if chars.get(*at) != Some(&'<') {
        return Err(format!("missing < at position {at}"));
    }
    *at += 1;
    let start = *at;
    while chars.get(*at).is_some_and(|&c| c != '>') {
        *at += 1;
    }
    if *at >= chars.len() {
        return Err(format!("missing >, unterminated name at position {start}"));
    }
    let name: String = chars[start..*at].iter().collect();
    *at += 1;
    if name.is_empty() {
        return Err(format!("missing group name at position {start}"));
    }
    let index = if is_identifier(&name) {
        names
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, i)| *i)
            .ok_or_else(|| format!("unknown group name '{name}'"))?
    } else if name.chars().all(|c| c.is_ascii_digit()) {
        name.parse::<usize>()
            .map_err(|_| format!("invalid group reference {name}"))?
    } else {
        return Err(format!(
            "bad character in group name '{name}' at position {start}"
        ));
    };
    Ok((index, start))
}
