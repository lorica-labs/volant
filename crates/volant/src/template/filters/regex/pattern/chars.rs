// SPDX-License-Identifier: GPL-3.0-or-later
//! The characters of a pattern: escapes, octal and hex codes, and sets.

use std::fmt::Write as _;

use super::{Failure, Prev, Scanner, invalid, unsupported};

impl Scanner {
    /// The escape after a backslash, outside a set.
    pub(super) fn escape(&mut self) -> Result<(), Failure> {
        let at = self.at - 1;
        let Some(c) = self.peek() else {
            return invalid("bad escape (end of pattern)", at);
        };
        self.at += 1;
        match c {
            'A' | 'b' | 'B' => {
                self.out.push('\\');
                self.out.push(c);
                self.assertion();
            }
            'Z' => {
                self.out.push_str("\\z");
                self.assertion();
            }
            'd' | 'D' | 's' | 'S' | 'w' | 'W' => {
                self.out.push('\\');
                self.out.push(c);
                self.item();
            }
            '0'..='9' => {
                if let Some(ch) = self.numeric(c, at)? {
                    push_literal(&mut self.out, ch);
                } else {
                    // A back reference, `\1` to `\99`, which the engine keeps as it is.
                    self.out.push('\\');
                    self.out.push(c);
                    if let Some(next) = self.peek().filter(char::is_ascii_digit) {
                        self.out.push(next);
                        self.at += 1;
                    }
                }
                self.item();
            }
            _ => {
                let ch = self.character(c, at)?;
                push_literal(&mut self.out, ch);
                self.item();
            }
        }
        Ok(())
    }

    /// An escape that stands for one character, where `c` follows the backslash. Shared by
    /// the pattern and its sets.
    fn character(&mut self, c: char, at: usize) -> Result<char, Failure> {
        Ok(match c {
            'a' => '\u{7}',
            'f' => '\u{c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'v' => '\u{b}',
            '\\' => '\\',
            'x' => self.hex(2, at)?,
            'u' => self.hex(4, at)?,
            'U' => self.hex(8, at)?,
            'N' => return unsupported("a named character, '\\N{name}'"),
            c if c.is_ascii_alphabetic() => {
                return invalid(format!("bad escape \\{c}"), at);
            }
            c => c,
        })
    }

    fn hex(&mut self, digits: usize, at: usize) -> Result<char, Failure> {
        let text: String = self.src.iter().skip(self.at).take(digits).collect();
        let letter = self.src[self.at - 1];
        if text.chars().count() != digits || !text.chars().all(|c| c.is_ascii_hexdigit()) {
            return invalid(format!("incomplete escape \\{letter}{text}"), at);
        }
        self.at += digits;
        let value = u32::from_str_radix(&text, 16).unwrap_or(u32::MAX);
        char::from_u32(value)
            .map_or_else(|| invalid(format!("bad escape \\{letter}{text}"), at), Ok)
    }

    /// A digit after a backslash: an octal escape is a character, and anything else is a back
    /// reference, which is `None`.
    fn numeric(&mut self, first: char, at: usize) -> Result<Option<char>, Failure> {
        let octal = |c: Option<char>| c.is_some_and(|c| ('0'..='7').contains(&c));
        if first == '0' {
            let mut value = 0;
            for _ in 0..2 {
                if !octal(self.peek()) {
                    break;
                }
                value = value * 8 + self.peek().map_or(0, |c| c as u32 - '0' as u32);
                self.at += 1;
            }
            return Ok(char::from_u32(value));
        }
        let second = self.peek();
        let third = self.src.get(self.at + 1).copied();
        if octal(Some(first)) && octal(second) && octal(third) {
            let digits: String = [first, second.unwrap_or('0'), third.unwrap_or('0')]
                .iter()
                .collect();
            self.at += 2;
            let value = u32::from_str_radix(&digits, 8).unwrap_or(0);
            if value > 0o377 {
                return invalid(
                    format!("octal escape value \\{digits} outside of range 0-0o377"),
                    at,
                );
            }
            return Ok(char::from_u32(value));
        }
        Ok(None)
    }

    /// A set, from just after its `[`.
    pub(super) fn set(&mut self) -> Result<(), Failure> {
        let open_at = self.at - 1;
        self.out.push('[');
        if self.peek() == Some('^') {
            self.out.push('^');
            self.at += 1;
        }
        let mut first = true;
        let mut prev = Prev::None;
        // Where the element `prev` was read from starts.
        let mut prev_at = 0;
        loop {
            let Some(c) = self.peek() else {
                return invalid("unterminated character set", open_at);
            };
            self.at += 1;
            if c == ']' && !first {
                self.out.push(']');
                self.item();
                return Ok(());
            }
            first = false;
            if let Some(end) = self.peek().filter(|n| c == '-' && *n != ']') {
                if prev == Prev::None {
                    push_set_char(&mut self.out, '-');
                    prev = Prev::Char('-');
                    prev_at = self.at - 1;
                    continue;
                }
                self.out.push('-');
                let end_at = self.at;
                self.at += 1;
                let (high, _) = self.set_element(end, end_at)?;
                let (Prev::Char(low), Some(high)) = (prev, high) else {
                    return self.bad_range(prev_at, end_at);
                };
                if low > high {
                    return self.bad_range(prev_at, end_at);
                }
                push_set_char(&mut self.out, high);
                prev = Prev::None;
                continue;
            }
            prev_at = self.at - 1;
            let (ch, is_class) = self.set_element(c, prev_at)?;
            if let Some(ch) = ch {
                push_set_char(&mut self.out, ch);
                prev = Prev::Char(ch);
            } else {
                debug_assert!(is_class);
                prev = Prev::Class;
            }
        }
    }

    /// A range from the element at `start` to the one at `end` that Python refuses. Python names
    /// each end by the token it read first, `\x` for `\x41`, and counts the position back from
    /// the end of the range by the length of those tokens.
    fn bad_range<T>(&self, start: usize, end: usize) -> Result<T, Failure> {
        let token = |at: usize| -> String {
            let len = if self.src[at] == '\\' { 2 } else { 1 };
            self.src[at..at + len].iter().collect()
        };
        let (this, that) = (token(start), token(end));
        let at = self.at - this.chars().count() - 1 - that.chars().count();
        invalid(format!("bad character range {this}-{that}"), at)
    }

    /// One element of a set that starts with `c`: the character it stands for, or `None` after
    /// writing a class such as `\d`. The flag says a class was read.
    fn set_element(&mut self, c: char, at: usize) -> Result<(Option<char>, bool), Failure> {
        if c != '\\' {
            return Ok((Some(c), false));
        }
        let Some(e) = self.peek() else {
            return invalid("bad escape (end of pattern)", at);
        };
        self.at += 1;
        match e {
            'd' | 'D' | 's' | 'S' | 'w' | 'W' => {
                self.out.push('\\');
                self.out.push(e);
                Ok((None, true))
            }
            'b' => Ok((Some('\u{8}'), false)),
            '0'..='7' => {
                // In a set, one to three octal digits are one character.
                let mut digits = e.to_string();
                while digits.len() < 3 && self.peek().is_some_and(|c| ('0'..='7').contains(&c)) {
                    digits.extend(self.peek());
                    self.at += 1;
                }
                let value = u32::from_str_radix(&digits, 8).unwrap_or(0);
                if value > 0o377 {
                    return invalid(
                        format!("octal escape value \\{digits} outside of range 0-0o377"),
                        at,
                    );
                }
                Ok((char::from_u32(value), false))
            }
            '8' | '9' | 'A' | 'B' | 'Z' => invalid(format!("bad escape \\{e}"), at),
            e => Ok((Some(self.character(e, at)?), false)),
        }
    }
}

/// The characters the `regex` crate reads specially. A literal one is written as a hex escape,
/// so that no syntax of the crate that Python lacks (`\<`, `[[:alpha:]]`) gets through.
const META: &str = "\\.+*?()|[]{}^$#&-~";

/// `c` as a character of the pattern outside a set.
pub(super) fn push_literal(out: &mut String, c: char) {
    if META.contains(c) || c.is_control() {
        let _ = write!(out, "\\x{{{:X}}}", c as u32);
    } else {
        out.push(c);
    }
}

/// `c` as a member of a set.
fn push_set_char(out: &mut String, c: char) {
    if c.is_ascii_alphanumeric() || (!c.is_ascii() && !c.is_control()) {
        out.push(c);
    } else {
        let _ = write!(out, "\\x{{{:X}}}", c as u32);
    }
}
