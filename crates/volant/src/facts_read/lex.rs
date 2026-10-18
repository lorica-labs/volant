// SPDX-License-Identifier: GPL-3.0-or-later
//! Scanning helpers over the text of an expression.

pub(super) fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

pub(super) fn leading_word(s: &str) -> &str {
    let end = s.bytes().position(|c| !is_word(c)).unwrap_or(s.len());
    &s[..end]
}

/// The content of the string literal `s` opens with.
pub(super) fn leading_literal(s: &str) -> Option<&str> {
    let q = *s.as_bytes().first()?;
    if q != b'\'' && q != b'"' {
        return None;
    }
    let end = s[1..].find(q as char)?;
    let lit = &s[1..=end];
    (!lit.contains('\\')).then_some(lit)
}

/// The content of `s` when `s` is one string literal and nothing else.
pub(super) fn literal(s: &str) -> Option<&str> {
    let s = s.trim();
    leading_literal(s).filter(|lit| lit.len() + 2 == s.len())
}

/// The index of the bracket closing the one at `open`, strings skipped.
pub(super) fn close(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0usize;
    let mut quote = None;
    let mut i = open;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            if c == b'\\' {
                i += 1;
            } else if c == q {
                quote = None;
            }
        } else {
            match c {
                b'\'' | b'"' => quote = Some(c),
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}
