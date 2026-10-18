// SPDX-License-Identifier: GPL-3.0-or-later
//! `regexp` as the reference's Python `re` reads it, translated to the expression Rust's `regex`
//! runs, or refused when there is no exact counterpart.

use std::fmt::Write as _;

use regex::bytes::{Regex, RegexBuilder};

/// `re.compile(to_bytes(pattern))`, as a Rust expression that matches exactly the lines the
/// Python one would find with `search`, or the reason there is none. The reason names the
/// argument `name` and never quotes the pattern, which can carry a secret.
pub(super) fn compile(name: &str, pattern: &str) -> Result<Regex, String> {
    let rust = translate(pattern).map_err(|why| format!("{name} holds {why}"))?;
    RegexBuilder::new(&rust)
        .unicode(false)
        .build()
        .map_err(|_| format!("{name} is an expression the Rust engine refuses"))
}

/// What the last item of the branch being read is, for Python's repeat rules.
#[derive(PartialEq)]
enum Last {
    /// Nothing yet: the start, `(` or `|`.
    Nothing,
    /// `^`, `$`, `\A`, `\b` or `\B`, which Python refuses to repeat.
    Assertion,
    /// Something a repeat applies to.
    Item,
    /// A repeat, which Python refuses to repeat again.
    Repeat,
}

/// A Python `re` pattern compiled from bytes, rewritten for Rust's `regex` with Unicode off.
///
/// Compiled from bytes, Python's pattern reads every byte as one character: a non-ASCII
/// character of the text is its UTF-8 bytes, one literal each, in a set as well. `\d`, `\w`,
/// `\s` and `\b` are ASCII, and `.` is any byte but `\n`, which is what Rust's are with Unicode
/// off. `$` also matches before a final `\n`, and every line but the last ends with one, so it
/// becomes `(?m:$)`, which matches there and at the end, and nowhere else in a line.
///
/// Every literal is written `\xHH` or as the ASCII letter or digit it is, so no Rust-only
/// syntax (`\<`, `[[:alpha:]]`, `&&`) can creep in through one. What Python reads and Rust has
/// no counterpart for (lookaround, `(?P=name)`, `\Z`, back references, inline flags,
/// possessive repeats), and what Python refuses (a repeat of nothing or of a repeat, an
/// unknown escape), is an `Err`, and the task goes to the Python module.
#[expect(clippy::too_many_lines)]
fn translate(pattern: &str) -> Result<String, String> {
    let bytes = pattern.as_bytes();
    let mut out = String::new();
    let mut last = Last::Nothing;
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        at += 1;
        match byte {
            b'\\' => {
                let (item, next) = escape(bytes, at, false)?;
                at = next;
                last = if matches!(item, Item::Assertion(_)) {
                    Last::Assertion
                } else {
                    Last::Item
                };
                item.write(&mut out);
            }
            b'(' => {
                if bytes.get(at) == Some(&b'?') {
                    let rest = &bytes[at..];
                    if rest.starts_with(b"?:") {
                        out.push_str("(?:");
                        at += 2;
                    } else if rest.starts_with(b"?P<") {
                        let end = rest
                            .iter()
                            .position(|&b| b == b'>')
                            .ok_or("an unterminated group name")?;
                        let name = &rest[3..end];
                        let identifier = name.first().is_some_and(|b| !b.is_ascii_digit())
                            && name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_');
                        if !identifier {
                            return Err("a group name Python refuses".into());
                        }
                        out.push_str("(?P<");
                        out.push_str(std::str::from_utf8(name).unwrap_or_default());
                        out.push('>');
                        at += end + 1;
                    } else {
                        return Err("a (? group other than (?: and (?P<name>".into());
                    }
                } else {
                    out.push('(');
                }
                last = Last::Nothing;
            }
            b')' => {
                out.push(')');
                last = Last::Item;
            }
            b'|' => {
                out.push('|');
                last = Last::Nothing;
            }
            b'^' => {
                out.push('^');
                last = Last::Assertion;
            }
            b'$' => {
                out.push_str("(?m:$)");
                last = Last::Assertion;
            }
            b'.' => {
                out.push('.');
                last = Last::Item;
            }
            b'[' => {
                at = set(bytes, at, &mut out)?;
                last = Last::Item;
            }
            b'*' | b'+' | b'?' | b'{' => {
                let repeat = if byte == b'{' {
                    let Some((repeat, next)) = braces(bytes, at) else {
                        // Not a repeat: Python reads the brace as itself.
                        Item::Byte(byte).write(&mut out);
                        last = Last::Item;
                        continue;
                    };
                    at = next;
                    repeat
                } else {
                    char::from(byte).to_string()
                };
                if last != Last::Item {
                    return Err("a repeat of what Python refuses to repeat".into());
                }
                out.push_str(&repeat);
                match bytes.get(at) {
                    Some(b'?') => {
                        out.push('?');
                        at += 1;
                    }
                    Some(b'+') => return Err("a possessive repeat".into()),
                    _ => {}
                }
                last = Last::Repeat;
            }
            _ => {
                Item::Byte(byte).write(&mut out);
                last = Last::Item;
            }
        }
    }
    Ok(out)
}

/// One element of a pattern after a backslash, or of a set.
enum Item {
    Byte(u8),
    /// `d`, `D`, `s`, `S`, `w` or `W`.
    Class(u8),
    /// `A`, `b` or `B` outside a set.
    Assertion(u8),
}

impl Item {
    fn write(&self, out: &mut String) {
        match self {
            Item::Byte(byte) if byte.is_ascii_alphanumeric() => out.push(char::from(*byte)),
            Item::Byte(byte) => {
                let _ = write!(out, "\\x{byte:02X}");
            }
            Item::Class(letter) | Item::Assertion(letter) => {
                out.push('\\');
                out.push(char::from(*letter));
            }
        }
    }
}

/// The escape starting at `bytes[at]`, just after its backslash (`_escape`, or `_class_escape`
/// inside a set), and where it ends.
fn escape(bytes: &[u8], at: usize, in_set: bool) -> Result<(Item, usize), String> {
    let &letter = bytes.get(at).ok_or("a trailing backslash")?;
    let item = match letter {
        b'd' | b'D' | b's' | b'S' | b'w' | b'W' => Item::Class(letter),
        b'b' if in_set => Item::Byte(0x08),
        b'A' | b'b' | b'B' if !in_set => Item::Assertion(letter),
        b'a' => Item::Byte(0x07),
        b'f' => Item::Byte(0x0c),
        b'n' => Item::Byte(b'\n'),
        b'r' => Item::Byte(b'\r'),
        b't' => Item::Byte(b'\t'),
        b'v' => Item::Byte(0x0b),
        b'x' => {
            let hex = bytes
                .get(at + 1..at + 3)
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .filter(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
                .ok_or("an incomplete \\x escape")?;
            let value = u8::from_str_radix(hex, 16).map_err(|err| err.to_string())?;
            return Ok((Item::Byte(value), at + 3));
        }
        // Back references, octal escapes, `\Z`, and letters Python refuses.
        _ if letter.is_ascii_alphanumeric() => {
            return Err(format!(
                "the escape \\{} with no exact counterpart",
                char::from(letter)
            ));
        }
        _ => Item::Byte(letter),
    };
    Ok((item, at + 1))
}

/// `{m,n}` and its shorter forms, starting just after the brace, as Rust writes them, and where
/// it ends; `None` when Python reads the brace as a literal.
fn braces(bytes: &[u8], mut at: usize) -> Option<(String, usize)> {
    if bytes.get(at) == Some(&b'}') {
        return None;
    }
    let digits = |at: &mut usize| {
        let start = *at;
        while bytes.get(*at).is_some_and(u8::is_ascii_digit) {
            *at += 1;
        }
        String::from_utf8_lossy(&bytes[start..*at]).into_owned()
    };
    let low = digits(&mut at);
    let high = if bytes.get(at) == Some(&b',') {
        at += 1;
        Some(digits(&mut at))
    } else {
        None
    };
    if bytes.get(at) != Some(&b'}') {
        return None;
    }
    let low = if low.is_empty() { "0".to_string() } else { low };
    let repeat = match high {
        None => format!("{{{low}}}"),
        Some(high) => format!("{{{low},{high}}}"),
    };
    Some((repeat, at + 1))
}

/// A set, from just after its `[`, written to `out`; returns where it ends.
fn set(bytes: &[u8], mut at: usize, out: &mut String) -> Result<usize, String> {
    out.push('[');
    if bytes.get(at) == Some(&b'^') {
        out.push('^');
        at += 1;
    }
    let start = at;
    loop {
        let &byte = bytes.get(at).ok_or("an unterminated set")?;
        if byte == b']' && at != start {
            out.push(']');
            return Ok(at + 1);
        }
        // Python warns about what may become nested sets and set operations.
        if byte == b'[' || (b"-&~|".contains(&byte) && bytes.get(at + 1) == Some(&byte)) {
            return Err("a set Python warns about".into());
        }
        let (low, next) = element(bytes, at)?;
        at = next;
        if bytes.get(at) == Some(&b'-') && bytes.get(at + 1).is_some_and(|&b| b != b']') {
            let (high, next) = element(bytes, at + 1)?;
            match (low, high) {
                (Item::Byte(low), Item::Byte(high)) if low <= high => {
                    let _ = write!(out, "\\x{low:02X}-\\x{high:02X}");
                }
                _ => return Err("a bad set range".into()),
            }
            at = next;
        } else {
            low.write(out);
        }
    }
}

/// One element of a set, and where it ends.
fn element(bytes: &[u8], at: usize) -> Result<(Item, usize), String> {
    match bytes[at] {
        b'\\' => escape(bytes, at + 1, true),
        byte => Ok((Item::Byte(byte), at + 1)),
    }
}
