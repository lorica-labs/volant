// SPDX-License-Identifier: GPL-3.0-or-later
//! The text encodings `str.encode` and `bytes.decode` know: UTF-8, ASCII, Latin-1, UTF-16 and
//! UTF-32, with the error handlers that do not need a table of character names.
//!
//! The errors are worded as CPython 3.13 words them, which ansible-core 2.19 runs on.

use std::fmt::Write as _;

use minijinja::Error;

use super::args::raise;

/// An encoding, by the name Python prints for it in an error.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Codec {
    Utf8,
    Ascii,
    Latin1,
    /// UTF-16 with the byte order, and whether a byte order mark leads the bytes.
    Utf16(Order, bool),
    Utf32(Order, bool),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Order {
    Little,
    Big,
}

impl Codec {
    /// The name in an error: `'ascii' codec can't encode character ...`.
    fn name(self) -> &'static str {
        match self {
            Codec::Utf8 => "utf-8",
            Codec::Ascii => "ascii",
            Codec::Latin1 => "latin-1",
            Codec::Utf16(_, true) => "utf-16",
            Codec::Utf16(Order::Little, false) => "utf-16-le",
            Codec::Utf16(Order::Big, false) => "utf-16-be",
            Codec::Utf32(_, true) => "utf-32",
            Codec::Utf32(Order::Little, false) => "utf-32-le",
            Codec::Utf32(Order::Big, false) => "utf-32-be",
        }
    }
}

/// The codec Python finds for `name`. Python lower-cases the name and reads a run of characters
/// that are neither letters, digits nor a dot as one `_`, so `UTF-8`, `utf 8` and `Utf_8` are one.
/// A name Python knows and this engine does not (`cp1252`, `shift_jis`) is refused by name.
pub(super) fn lookup(name: &str) -> Result<Codec, Error> {
    let mut key = String::new();
    let mut gap = false;
    for c in name.chars() {
        if c.is_alphanumeric() || c == '.' {
            if gap && !key.is_empty() {
                key.push('_');
            }
            gap = false;
            key.extend(c.to_lowercase());
        } else {
            gap = true;
        }
    }
    Ok(match key.as_str() {
        "utf_8" | "utf8" | "u8" | "utf" | "utf8_ucs2" | "utf8_ucs4" | "cp65001" => Codec::Utf8,
        "ascii" | "us_ascii" | "646" | "ansi_x3.4_1968" | "ansi_x3_4_1968" | "cp367" | "ibm367"
        | "iso646_us" | "iso_646.irv_1991" | "iso_ir_6" | "csascii" => Codec::Ascii,
        "latin_1" | "latin1" | "latin" | "l1" | "iso_8859_1" | "iso8859_1" | "8859" | "cp819"
        | "ibm819" | "iso_ir_100" | "csisolatin1" | "iso_8859_1_1987" => Codec::Latin1,
        "utf_16" | "utf16" | "u16" => Codec::Utf16(Order::Little, true),
        "utf_16_le" | "utf_16le" | "utf16le" => Codec::Utf16(Order::Little, false),
        "utf_16_be" | "utf_16be" | "utf16be" => Codec::Utf16(Order::Big, false),
        "utf_32" | "utf32" | "u32" => Codec::Utf32(Order::Little, true),
        "utf_32_le" | "utf_32le" | "utf32le" => Codec::Utf32(Order::Little, false),
        "utf_32_be" | "utf_32be" | "utf32be" => Codec::Utf32(Order::Big, false),
        "cp1252" | "windows_1252" | "latin_9" | "iso8859_15" | "iso_8859_15" | "cp437"
        | "cp850" | "utf_8_sig" | "utf_7" | "idna" | "punycode" | "unicode_escape"
        | "raw_unicode_escape" | "koi8_r" | "shift_jis" | "gbk" | "gb2312" | "big5" | "euc_jp"
        | "euc_kr" | "mbcs" | "base64" | "hex" | "rot13" | "zlib" | "bz2" => {
            return Err(raise(format!(
                "the encoding '{name}' is not supported by this release: it knows utf-8, ascii, \
                 latin-1, utf-16 and utf-32"
            )));
        }
        _ => return Err(raise(format!("unknown encoding: {name}"))),
    })
}

/// What an error handler does with the characters or bytes that a codec cannot convert.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Handler {
    Strict,
    Ignore,
    Replace,
    Backslash,
    XmlCharRef,
}

/// The handler Python finds for `name`. Python looks it up only when a conversion fails, so a
/// name that is wrong goes unnoticed until then. `surrogateescape` and `surrogatepass` act on
/// lone surrogates, which a text here never holds: for any other failure they re-raise it.
fn handler(name: &str) -> Result<Handler, Error> {
    Ok(match name {
        "strict" | "surrogateescape" | "surrogatepass" => Handler::Strict,
        "ignore" => Handler::Ignore,
        "replace" => Handler::Replace,
        "backslashreplace" => Handler::Backslash,
        "xmlcharrefreplace" => Handler::XmlCharRef,
        "namereplace" => {
            return Err(raise(
                "the error handler 'namereplace' is not supported by this release",
            ));
        }
        _ => return Err(raise(format!("unknown error handler name '{name}'"))),
    })
}

/// `\xhh`, `\uhhhh` or `\Uhhhhhhhh`, the shortest of the three that holds `n`.
fn escape(out: &mut String, n: u32) {
    let _ = match n {
        0..=0xff => write!(out, "\\x{n:02x}"),
        0x100..=0xffff => write!(out, "\\u{n:04x}"),
        _ => write!(out, "\\U{n:08x}"),
    };
}

/// `text` as bytes in `codec`; `errors` names the handler for a character the codec lacks.
pub(super) fn encode(text: &str, codec: Codec, errors: &str) -> Result<Vec<u8>, Error> {
    match codec {
        Codec::Utf8 => Ok(text.as_bytes().to_vec()),
        Codec::Ascii => narrow(text, codec, 0x80, errors),
        Codec::Latin1 => narrow(text, codec, 0x100, errors),
        Codec::Utf16(order, bom) => {
            let mut out = Vec::new();
            let mut put = |unit: u16| {
                out.extend(match order {
                    Order::Little => unit.to_le_bytes(),
                    Order::Big => unit.to_be_bytes(),
                });
            };
            if bom {
                put(0xfeff);
            }
            text.encode_utf16().for_each(put);
            Ok(out)
        }
        Codec::Utf32(order, bom) => {
            let mut out = Vec::new();
            let units = bom.then_some('\u{feff}').into_iter().chain(text.chars());
            for c in units {
                out.extend(match order {
                    Order::Little => u32::from(c).to_le_bytes(),
                    Order::Big => u32::from(c).to_be_bytes(),
                });
            }
            Ok(out)
        }
    }
}

/// The one-byte codecs: every character below `limit` is its own byte.
fn narrow(text: &str, codec: Codec, limit: u32, errors: &str) -> Result<Vec<u8>, Error> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::with_capacity(chars.len());
    let mut at = 0;
    while at < chars.len() {
        let c = u32::from(chars[at]);
        if c < limit {
            out.push(u8::try_from(c).unwrap_or(b'?'));
            at += 1;
            continue;
        }
        // Python hands the handler the whole run of characters the codec lacks.
        let end = chars[at..]
            .iter()
            .position(|c| u32::from(*c) < limit)
            .map_or(chars.len(), |n| at + n);
        match handler(errors)? {
            Handler::Strict => return Err(raise(unencodable(&chars[at..end], at, codec, limit))),
            Handler::Ignore => {}
            Handler::Replace => out.extend(std::iter::repeat_n(b'?', end - at)),
            Handler::Backslash => {
                let mut text = String::new();
                chars[at..end]
                    .iter()
                    .for_each(|c| escape(&mut text, u32::from(*c)));
                out.extend(text.into_bytes());
            }
            Handler::XmlCharRef => {
                let mut text = String::new();
                for c in &chars[at..end] {
                    let _ = write!(text, "&#{};", u32::from(*c));
                }
                out.extend(text.into_bytes());
            }
        }
        at = end;
    }
    Ok(out)
}

fn unencodable(run: &[char], start: usize, codec: Codec, limit: u32) -> String {
    let name = codec.name();
    if let [one] = run {
        let mut shown = String::new();
        escape(&mut shown, u32::from(*one));
        format!(
            "'{name}' codec can't encode character '{shown}' in position {start}: ordinal not in \
             range({limit})"
        )
    } else {
        format!(
            "'{name}' codec can't encode characters in position {start}-{}: ordinal not in \
             range({limit})",
            start + run.len() - 1
        )
    }
}

/// `bytes` as text in `codec`; `errors` names the handler for a byte the codec cannot read.
pub(super) fn decode(bytes: &[u8], codec: Codec, errors: &str) -> Result<String, Error> {
    match codec {
        Codec::Utf8 => utf8(bytes, errors),
        Codec::Ascii => ascii(bytes, errors),
        Codec::Latin1 => Ok(bytes.iter().map(|b| char::from(*b)).collect()),
        _ => Err(raise(format!(
            "decoding with '{}' is not supported by this release: it decodes utf-8, ascii and \
             latin-1",
            codec.name()
        ))),
    }
}

/// What a decode puts in place of the bytes it cannot read, or the error it raises.
fn substitute(
    out: &mut String,
    bad: &[u8],
    errors: &str,
    fail: impl FnOnce() -> String,
) -> Result<(), Error> {
    // Reading a byte as a lone surrogate needs a text that can hold one.
    if matches!(errors, "surrogateescape" | "surrogatepass") {
        return Err(raise(format!(
            "the error handler '{errors}' is not supported by this release when it decodes"
        )));
    }
    match handler(errors)? {
        Handler::Strict => return Err(raise(fail())),
        Handler::Ignore => {}
        Handler::Replace => out.push('\u{fffd}'),
        Handler::Backslash => bad.iter().for_each(|b| escape(out, u32::from(*b))),
        Handler::XmlCharRef => {
            return Err(raise(
                "don't know how to handle UnicodeDecodeError in error callback",
            ));
        }
    }
    Ok(())
}

fn utf8(bytes: &[u8], errors: &str) -> Result<String, Error> {
    let mut out = String::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let rest = &bytes[at..];
        match std::str::from_utf8(rest) {
            Ok(text) => {
                out.push_str(text);
                break;
            }
            Err(e) => {
                let good = e.valid_up_to();
                out.push_str(std::str::from_utf8(&rest[..good]).unwrap_or_default());
                let start = at + good;
                let len = e.error_len().unwrap_or(bytes.len() - start);
                let reason = if e.error_len().is_none() {
                    "unexpected end of data"
                } else if matches!(bytes[start], 0x80..=0xc1 | 0xf5..=0xff) {
                    "invalid start byte"
                } else {
                    "invalid continuation byte"
                };
                substitute(&mut out, &bytes[start..start + len], errors, || {
                    undecodable("utf-8", bytes, start, len, reason)
                })?;
                at = start + len;
            }
        }
    }
    Ok(out)
}

fn ascii(bytes: &[u8], errors: &str) -> Result<String, Error> {
    let mut out = String::with_capacity(bytes.len());
    for (at, b) in bytes.iter().enumerate() {
        if b.is_ascii() {
            out.push(char::from(*b));
        } else {
            substitute(&mut out, &[*b], errors, || {
                undecodable("ascii", bytes, at, 1, "ordinal not in range(128)")
            })?;
        }
    }
    Ok(out)
}

fn undecodable(name: &str, bytes: &[u8], start: usize, len: usize, reason: &str) -> String {
    if len == 1 {
        format!(
            "'{name}' codec can't decode byte 0x{:02x} in position {start}: {reason}",
            bytes[start]
        )
    } else {
        format!(
            "'{name}' codec can't decode bytes in position {start}-{}: {reason}",
            start + len - 1
        )
    }
}
