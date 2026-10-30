// SPDX-License-Identifier: GPL-3.0-or-later
//! Python's string and bytes literals: prefixes, quotes, escapes and joined neighbours.

use super::{Lit, Parser};

impl Parser<'_> {
    /// One string literal, or several written side by side, which Python joins.
    pub(super) fn strings(&mut self) -> Option<Lit> {
        let mut text: Option<String> = None;
        let mut bytes: Option<Vec<u8>> = None;
        loop {
            match self.string()? {
                Lit::Str(s) if bytes.is_none() => text.get_or_insert_with(String::new).push_str(&s),
                Lit::Bytes(b) if text.is_none() => bytes.get_or_insert_with(Vec::new).extend(b),
                _ => return None,
            }
            let mark = self.at;
            self.blank();
            let next = self
                .rest()
                .bytes()
                .take_while(u8::is_ascii_alphabetic)
                .count();
            if !matches!(self.rest().as_bytes().get(next), Some(b'\'' | b'"')) {
                self.at = mark;
                break;
            }
        }
        text.map(Lit::Str).or_else(|| bytes.map(Lit::Bytes))
    }

    fn string(&mut self) -> Option<Lit> {
        let prefix_len = self
            .rest()
            .bytes()
            .take_while(u8::is_ascii_alphabetic)
            .count();
        let prefix = self.rest()[..prefix_len].to_ascii_lowercase();
        let (raw, is_bytes) = match prefix.as_str() {
            "" | "u" => (false, false),
            "r" => (true, false),
            "b" => (false, true),
            "br" | "rb" => (true, true),
            _ => return None,
        };
        self.at += prefix_len;
        let quote = self.peek()?;
        let triple = self.rest().as_bytes().get(..3) == Some(&[quote; 3][..]);
        self.at += if triple { 3 } else { 1 };
        let mut out = String::new();
        loop {
            let c = self.rest().chars().next()?;
            if c == char::from(quote) {
                if !triple {
                    self.at += 1;
                    break;
                }
                if self.rest().as_bytes().get(..3) == Some(&[quote; 3][..]) {
                    self.at += 3;
                    break;
                }
            }
            if (c == '\n' && !triple) || (is_bytes && !c.is_ascii()) {
                return None;
            }
            self.at += c.len_utf8();
            if c != '\\' {
                out.push(c);
                continue;
            }
            let next = self.rest().chars().next()?;
            if raw {
                // A raw string keeps the backslash; it only stops the quote from closing.
                out.push('\\');
                out.push(next);
                self.at += next.len_utf8();
                continue;
            }
            self.escape(next, is_bytes, &mut out)?;
        }
        Some(if is_bytes {
            // Each character of a bytes literal is one byte: `\xff` is 255, not `ÿ`.
            Lit::Bytes(out.chars().map(|c| c as u8).collect())
        } else {
            Lit::Str(out)
        })
    }

    /// The escape `\next`, its backslash already read.
    fn escape(&mut self, next: char, is_bytes: bool, out: &mut String) -> Option<()> {
        self.at += next.len_utf8();
        let simple = match next {
            '\n' => Some(None),
            '\\' | '\'' | '"' => Some(Some(next)),
            'a' => Some(Some('\x07')),
            'b' => Some(Some('\x08')),
            'f' => Some(Some('\x0c')),
            'n' => Some(Some('\n')),
            'r' => Some(Some('\r')),
            't' => Some(Some('\t')),
            'v' => Some(Some('\x0b')),
            _ => None,
        };
        if let Some(c) = simple {
            out.extend(c);
            return Some(());
        }
        let hex = |p: &mut Self, n: usize| -> Option<u32> {
            let digits = p.rest().get(..n)?;
            let value = u32::from_str_radix(digits, 16).ok()?;
            digits
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
                .then_some(())?;
            p.at += n;
            Some(value)
        };
        match next {
            '0'..='7' => {
                let more = self
                    .rest()
                    .bytes()
                    .take(2)
                    .take_while(|b| (b'0'..=b'7').contains(b))
                    .count();
                let digits = format!("{next}{}", &self.rest()[..more]);
                self.at += more;
                let value = u32::from_str_radix(&digits, 8).ok()?;
                if is_bytes {
                    out.push(char::from(u8::try_from(value & 0xff).ok()?));
                } else {
                    out.push(char::from_u32(value)?);
                }
            }
            'x' => out.push(char::from_u32(hex(self, 2)?)?),
            'u' if !is_bytes => out.push(char::from_u32(hex(self, 4)?)?),
            'U' if !is_bytes => out.push(char::from_u32(hex(self, 8)?)?),
            // `\N{...}` needs Unicode's names, which this does not carry: the text stays as it is.
            'N' if !is_bytes => return None,
            // Any other escape keeps its backslash, with a warning the plugin hides.
            other => {
                out.push('\\');
                out.push(other);
            }
        }
        Some(())
    }
}
