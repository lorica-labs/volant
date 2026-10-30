// SPDX-License-Identifier: GPL-3.0-or-later
//! `\udc80` and the other lone surrogates a JSON text may escape, which Python's decoder keeps
//! in its string and a Rust string cannot hold.
//!
//! Measured on ansible-core 2.19.12 with `'"hi \udc80 mom"' | from_json`: with
//! `MODULE_STRICT_UTF8_RESPONSE` on, the default, the filter fails, naming the character and its
//! position. Off, the string keeps it, and the display prints `?` in its place with a warning.

use std::borrow::Cow;
use std::sync::Mutex;

/// Where a lone surrogate is kept while the text is parsed: the first of 2048 private-use
/// characters, one per surrogate, which no JSON text writes.
const BASE: u32 = 0xF_0000;

/// `text` with each escaped surrogate that has no partner turned into its private-use stand-in,
/// and whether there was one.
pub(super) fn stand_in(text: &str) -> (Cow<'_, str>, bool) {
    if !text.contains("\\u") {
        return (Cow::Borrowed(text), false);
    }
    let escape = |at: usize| -> Option<u32> {
        let hex = text.get(at..at + 6)?.strip_prefix("\\u")?;
        u32::from_str_radix(hex, 16).ok()
    };
    let mut out = String::with_capacity(text.len());
    let mut found = false;
    let mut chars = text.char_indices();
    while let Some((at, c)) = chars.next() {
        out.push(c);
        if c != '\\' {
            continue;
        }
        match escape(at) {
            Some(0xD800..=0xDBFF) if matches!(escape(at + 6), Some(0xDC00..=0xDFFF)) => {
                // A pair is one character, which the decoder reads itself.
                out.push_str(&text[at + 1..at + 12]);
                chars.nth(10);
            }
            Some(code @ 0xD800..=0xDFFF) => {
                out.pop();
                out.extend(char::from_u32(BASE + code - 0xD800));
                found = true;
                chars.nth(4);
            }
            // Any other escape, `\\` included, is copied whole so its second character does
            // not start one.
            _ => {
                if let Some((_, next)) = chars.next() {
                    out.push(next);
                }
            }
        }
    }
    (Cow::Owned(out), found)
}

/// The surrogate `c` stands in for, if it is a stand-in.
fn surrogate(c: char) -> Option<u32> {
    let code = u32::from(c);
    (BASE..BASE + 0x800)
        .contains(&code)
        .then(|| code - BASE + 0xD800)
}

/// Every string of `value` with its stand-ins resolved: an error under
/// `MODULE_STRICT_UTF8_RESPONSE`, `?` otherwise.
pub(super) fn resolve(value: &mut serde_json::Value) -> Result<(), String> {
    use serde_json::Value;
    match value {
        Value::String(s) => text(s),
        Value::Array(items) => items.iter_mut().try_for_each(resolve),
        Value::Object(map) => {
            let entries = std::mem::take(map);
            for (mut key, mut item) in entries {
                text(&mut key)?;
                resolve(&mut item)?;
                map.insert(key, item);
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn text(s: &mut String) -> Result<(), String> {
    let chars: Vec<char> = s.chars().collect();
    let Some(first) = chars.iter().position(|c| surrogate(*c).is_some()) else {
        return Ok(());
    };
    if crate::config::strict_utf8() {
        // Python's UTF-8 codec reports the run of surrogates the first one starts.
        let run = chars[first..]
            .iter()
            .take_while(|c| surrogate(**c).is_some())
            .count();
        let place = if run == 1 {
            format!(
                "character '\\u{:x}' in position {first}",
                surrogate(chars[first]).unwrap_or_default()
            )
        } else {
            format!("characters in position {first}-{}", first + run - 1)
        };
        return Err(format!(
            "Refusing to deserialize an invalid UTF8 string value: 'utf-8' codec can't encode {place}: surrogates not allowed"
        ));
    }
    *s = chars
        .into_iter()
        .map(|c| if surrogate(c).is_some() { '?' } else { c })
        .collect();
    // Said once, as the reference's display drops a warning it has already shown.
    static SHOWN: Mutex<bool> = Mutex::new(false);
    let mut shown = SHOWN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !*shown {
        *shown = true;
        eprintln!(
            "[WARNING]: Non UTF-8 encoded data replaced with \"?\" while displaying text to stdout/stderr."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{resolve, stand_in};

    /// A pair is left to the decoder; a lone surrogate, high or low, is refused with its place,
    /// in the words of ansible-core 2.19.12's strict check.
    #[test]
    fn a_lone_surrogate_is_refused_where_it_stands() {
        let refusal = |json: &str| {
            let (text, lone) = stand_in(json);
            let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
            (lone, resolve(&mut value).err(), value)
        };
        assert_eq!(
            refusal(r#""\ud83d\ude00 \\udc80""#),
            (false, None, serde_json::json!("😀 \\udc80"))
        );
        let refused =
            "Refusing to deserialize an invalid UTF8 string value: 'utf-8' codec can't encode";
        assert_eq!(
            refusal(r#"["ok", {"k": "hi \udc80 mom"}]"#).1,
            Some(format!(
                "{refused} character '\\udc80' in position 3: surrogates not allowed"
            ))
        );
        assert_eq!(
            refusal(r#""ab\ud800\udbff""#).1,
            Some(format!(
                "{refused} characters in position 2-3: surrogates not allowed"
            ))
        );
    }
}
