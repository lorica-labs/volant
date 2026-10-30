// SPDX-License-Identifier: GPL-3.0-or-later
//! `output_encoding` of `template`: the text of the render turned into the bytes the file holds.
//!
//! The reference calls Python's `str.encode(output_encoding)`. The names and aliases Python
//! knows for each codec are kept here, and so are its error texts. A codec outside this list is
//! refused by name rather than guessed at.

/// A codec this release writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    Utf8,
    Utf8Sig,
    Latin1,
    Ascii,
    Cp1252,
    Latin9,
    Utf16(Order),
    Utf32(Order),
}

/// The byte order of a UTF-16 or UTF-32 codec; `Marked` writes a byte order mark, then the
/// order Python's `utf-16` takes on a little-endian machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    Marked,
    Little,
    Big,
}

/// Python's `encodings.aliases` for the codecs here, after its name normalisation.
const NAMES: &[(&str, Codec)] = &[
    ("utf_8", Codec::Utf8),
    ("cp65001", Codec::Utf8),
    ("u8", Codec::Utf8),
    ("utf", Codec::Utf8),
    ("utf8", Codec::Utf8),
    ("utf8_ucs2", Codec::Utf8),
    ("utf8_ucs4", Codec::Utf8),
    ("utf_8_sig", Codec::Utf8Sig),
    ("latin_1", Codec::Latin1),
    ("8859", Codec::Latin1),
    ("cp819", Codec::Latin1),
    ("csisolatin1", Codec::Latin1),
    ("ibm819", Codec::Latin1),
    ("iso8859", Codec::Latin1),
    ("iso8859_1", Codec::Latin1),
    ("iso_8859_1", Codec::Latin1),
    ("iso_8859_1_1987", Codec::Latin1),
    ("iso_ir_100", Codec::Latin1),
    ("l1", Codec::Latin1),
    ("latin", Codec::Latin1),
    ("latin1", Codec::Latin1),
    ("ascii", Codec::Ascii),
    ("646", Codec::Ascii),
    ("ansi_x3.4_1968", Codec::Ascii),
    ("ansi_x3.4_1986", Codec::Ascii),
    ("ansi_x3_4_1968", Codec::Ascii),
    ("cp367", Codec::Ascii),
    ("csascii", Codec::Ascii),
    ("ibm367", Codec::Ascii),
    ("iso646_us", Codec::Ascii),
    ("iso_646.irv_1991", Codec::Ascii),
    ("iso_ir_6", Codec::Ascii),
    ("us", Codec::Ascii),
    ("us_ascii", Codec::Ascii),
    ("cp1252", Codec::Cp1252),
    ("1252", Codec::Cp1252),
    ("windows_1252", Codec::Cp1252),
    ("iso8859_15", Codec::Latin9),
    ("iso_8859_15", Codec::Latin9),
    ("l9", Codec::Latin9),
    ("latin9", Codec::Latin9),
    ("utf_16", Codec::Utf16(Order::Marked)),
    ("u16", Codec::Utf16(Order::Marked)),
    ("utf16", Codec::Utf16(Order::Marked)),
    ("utf_16_le", Codec::Utf16(Order::Little)),
    ("unicodelittleunmarked", Codec::Utf16(Order::Little)),
    ("utf_16le", Codec::Utf16(Order::Little)),
    ("utf_16_be", Codec::Utf16(Order::Big)),
    ("unicodebigunmarked", Codec::Utf16(Order::Big)),
    ("utf_16be", Codec::Utf16(Order::Big)),
    ("utf_32", Codec::Utf32(Order::Marked)),
    ("u32", Codec::Utf32(Order::Marked)),
    ("utf32", Codec::Utf32(Order::Marked)),
    ("utf_32_le", Codec::Utf32(Order::Little)),
    ("utf_32le", Codec::Utf32(Order::Little)),
    ("utf_32_be", Codec::Utf32(Order::Big)),
    ("utf_32be", Codec::Utf32(Order::Big)),
];

/// Codecs Python has that turn bytes into bytes, which `str.encode` refuses.
const NOT_TEXT: &[&str] = &[
    "base64",
    "base_64",
    "bz2",
    "bz2_codec",
    "hex",
    "hex_codec",
    "quopri",
    "quotedprintable",
    "quoted_printable",
    "rot13",
    "rot_13",
    "uu",
    "uu_codec",
    "zip",
    "zlib",
    "zlib_codec",
];

/// Python's `encodings.normalize_encoding` after the lookup lower-cases the name: each run of
/// characters that are not letters, digits or a dot becomes one underscore, and none leads or
/// ends the name.
fn normalize(name: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for c in name.to_lowercase().chars() {
        if c.is_alphanumeric() || c == '.' {
            if gap && !out.is_empty() {
                out.push('_');
            }
            out.push(c);
            gap = false;
        } else {
            gap = true;
        }
    }
    out
}

/// The codec `name` stands for, as Python's lookup finds it.
fn codec_of(name: &str) -> Option<Codec> {
    let norm = normalize(name);
    NAMES.iter().find(|(n, _)| *n == norm).map(|(_, c)| *c)
}

/// Python's refusal of a name that is no text codec here.
fn unknown(name: &str) -> String {
    let norm = normalize(name);
    if NOT_TEXT.contains(&norm.as_str()) {
        format!("'{norm}' is not a text encoding; use codecs.encode() to handle arbitrary codecs")
    } else {
        format!("unknown encoding: {name}")
    }
}

/// The bytes of `text` in the codec `name`, or the reference's error for it.
pub(super) fn encode(text: &str, name: &str) -> Result<Vec<u8>, String> {
    let Some(codec) = codec_of(name) else {
        let norm = normalize(name);
        return Err(if NOT_TEXT.contains(&norm.as_str()) {
            unknown(name)
        } else {
            // A name is echoed up to 64 characters, so a long one does not fill the message.
            let shown: String = name.chars().take(64).collect();
            let shown = if shown.len() < name.len() {
                format!("{shown}...")
            } else {
                shown
            };
            format!(
                "output_encoding '{shown}' is not supported yet on 'template': only utf-8, \
                 utf-8-sig, latin-1, ascii, cp1252, iso-8859-15, utf-16 and utf-32 are"
            )
        });
    };
    encode_with(codec, text)
}

/// `text.encode(name)`, Python's error for a name it does not know included.
pub(crate) fn encode_text(text: &str, name: &str) -> Result<Vec<u8>, String> {
    encode_with(codec_of(name).ok_or_else(|| unknown(name))?, text)
}

/// `bytes.decode(name)`.
pub(crate) fn decode_text(bytes: &[u8], name: &str) -> Result<String, String> {
    let codec = codec_of(name).ok_or_else(|| unknown(name))?;
    decode::decode(bytes, codec)
}

fn encode_with(codec: Codec, text: &str) -> Result<Vec<u8>, String> {
    match codec {
        Codec::Utf8 => Ok(text.as_bytes().to_vec()),
        Codec::Utf8Sig => Ok([&[0xef, 0xbb, 0xbf][..], text.as_bytes()].concat()),
        Codec::Latin1 => narrow(text, "latin-1", "ordinal not in range(256)", |c| {
            u8::try_from(u32::from(c)).ok()
        }),
        Codec::Ascii => narrow(text, "ascii", "ordinal not in range(128)", |c| {
            u8::try_from(u32::from(c)).ok().filter(u8::is_ascii)
        }),
        Codec::Cp1252 => narrow(text, "charmap", UNDEFINED, cp1252),
        Codec::Latin9 => narrow(text, "charmap", UNDEFINED, latin9),
        Codec::Utf16(order) => Ok(wide(text, order, false)),
        Codec::Utf32(order) => Ok(wide(text, order, true)),
    }
}

/// Python's reason for a character a charmap codec has no byte for.
const UNDEFINED: &str = "character maps to <undefined>";

/// What cp1252 holds from 0x80 to 0x9f; the five bytes it leaves undefined are not here.
const CP1252_HIGH: [(char, u8); 27] = [
    ('\u{20ac}', 0x80),
    ('\u{201a}', 0x82),
    ('\u{192}', 0x83),
    ('\u{201e}', 0x84),
    ('\u{2026}', 0x85),
    ('\u{2020}', 0x86),
    ('\u{2021}', 0x87),
    ('\u{2c6}', 0x88),
    ('\u{2030}', 0x89),
    ('\u{160}', 0x8a),
    ('\u{2039}', 0x8b),
    ('\u{152}', 0x8c),
    ('\u{17d}', 0x8e),
    ('\u{2018}', 0x91),
    ('\u{2019}', 0x92),
    ('\u{201c}', 0x93),
    ('\u{201d}', 0x94),
    ('\u{2022}', 0x95),
    ('\u{2013}', 0x96),
    ('\u{2014}', 0x97),
    ('\u{2dc}', 0x98),
    ('\u{2122}', 0x99),
    ('\u{161}', 0x9a),
    ('\u{203a}', 0x9b),
    ('\u{153}', 0x9c),
    ('\u{17e}', 0x9e),
    ('\u{178}', 0x9f),
];

fn cp1252(c: char) -> Option<u8> {
    match u32::from(c) {
        code @ (0..0x80 | 0xa0..0x100) => u8::try_from(code).ok(),
        _ => CP1252_HIGH.iter().find(|(h, _)| *h == c).map(|(_, b)| *b),
    }
}

/// What ISO 8859-15 puts in place of eight of Latin-1's characters.
const LATIN9_CHANGED: [(char, u8); 8] = [
    ('\u{20ac}', 0xa4),
    ('\u{160}', 0xa6),
    ('\u{161}', 0xa8),
    ('\u{17d}', 0xb4),
    ('\u{17e}', 0xb8),
    ('\u{152}', 0xbc),
    ('\u{153}', 0xbd),
    ('\u{178}', 0xbe),
];

fn latin9(c: char) -> Option<u8> {
    if let Some((_, b)) = LATIN9_CHANGED.iter().find(|(h, _)| *h == c) {
        return Some(*b);
    }
    let byte = u8::try_from(u32::from(c)).ok()?;
    (!LATIN9_CHANGED.iter().any(|(_, b)| *b == byte)).then_some(byte)
}

/// One byte per character, as `byte` gives it. The error is the one Python raises for the first
/// run of characters that have none.
fn narrow(
    text: &str,
    codec: &str,
    reason: &str,
    byte: impl Fn(char) -> Option<u8>,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut at = 0;
    while at < chars.len() {
        if let Some(b) = byte(chars[at]) {
            out.push(b);
            at += 1;
            continue;
        }
        let end = (at..chars.len())
            .find(|&i| byte(chars[i]).is_some())
            .unwrap_or(chars.len());
        let what = if end - at == 1 {
            format!("character {} in position {at}", escape(chars[at]))
        } else {
            format!("characters in position {at}-{}", end - 1)
        };
        return Err(format!("'{codec}' codec can't encode {what}: {reason}"));
    }
    Ok(out)
}

/// Python's `\xNN`, `\uNNNN` or `\UNNNNNNNN` spelling of a character in an error text.
fn escape(c: char) -> String {
    let code = c as u32;
    let escaped = if code < 0x100 {
        format!("\\x{code:02x}")
    } else if code < 0x1_0000 {
        format!("\\u{code:04x}")
    } else {
        format!("\\U{code:08x}")
    };
    format!("'{escaped}'")
}

/// UTF-16 or UTF-32; `Marked` starts with the byte order mark and goes on little-endian.
fn wide(text: &str, order: Order, four: bool) -> Vec<u8> {
    let big = order == Order::Big;
    let mut out = Vec::with_capacity(text.len() * 2 + 4);
    let mut unit = |value: u32, bytes: usize| {
        let le = value.to_le_bytes();
        let part = &le[..bytes];
        if big {
            out.extend(part.iter().rev());
        } else {
            out.extend(part);
        }
    };
    if order == Order::Marked {
        unit(0xfeff, if four { 4 } else { 2 });
    }
    for c in text.chars() {
        if four {
            unit(c as u32, 4);
        } else {
            let mut buf = [0u16; 2];
            for u in c.encode_utf16(&mut buf) {
                unit(u32::from(*u), 2);
            }
        }
    }
    out
}

mod decode;
#[cfg(test)]
mod tests;
