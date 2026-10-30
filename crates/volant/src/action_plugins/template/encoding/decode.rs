// SPDX-License-Identifier: GPL-3.0-or-later
//! `bytes.decode(codec)` for the codecs the encoder knows, with Python's error for the first byte
//! a codec cannot read.

use super::{Codec, Order, UNDEFINED, cp1252, latin9};

fn refused(codec: &str, bytes: &[u8], at: usize, reason: &str) -> String {
    format!(
        "'{codec}' codec can't decode byte 0x{:02x} in position {at}: {reason}",
        bytes.get(at).copied().unwrap_or(0)
    )
}

/// One character per byte, as `char_of` reads it.
fn narrow(
    bytes: &[u8],
    codec: &str,
    reason: &str,
    char_of: impl Fn(u8) -> Option<char>,
) -> Result<String, String> {
    bytes
        .iter()
        .enumerate()
        .map(|(at, b)| char_of(*b).ok_or_else(|| refused(codec, bytes, at, reason)))
        .collect()
}

/// The character a charmap codec reads for a byte, by searching what it writes for each one.
fn charmap(byte: u8, write: fn(char) -> Option<u8>) -> Option<char> {
    let direct = char::from(byte);
    if write(direct) == Some(byte) {
        return Some(direct);
    }
    (0x100..0x2200)
        .filter_map(char::from_u32)
        .find(|c| write(*c) == Some(byte))
}

fn utf8(bytes: &[u8]) -> Result<String, String> {
    String::from_utf8(bytes.to_vec()).map_err(|e| {
        let at = e.utf8_error().valid_up_to();
        let reason = if e.utf8_error().error_len().is_none() {
            "unexpected end of data"
        } else if bytes[at] & 0xc0 == 0x80 || bytes[at] >= 0xf8 {
            "invalid start byte"
        } else {
            "invalid continuation byte"
        };
        refused("utf-8", bytes, at, reason)
    })
}

/// UTF-16 or UTF-32: a marked codec reads the byte order from the mark, little-endian without one.
fn wide(bytes: &[u8], order: Order, four: bool) -> Result<String, String> {
    let size = if four { 4 } else { 2 };
    let (mut big, mut body) = (order == Order::Big, bytes);
    if order == Order::Marked {
        let mark: &[u8] = if four {
            &[0xff, 0xfe, 0, 0]
        } else {
            &[0xff, 0xfe]
        };
        let flipped: Vec<u8> = mark.iter().rev().copied().collect();
        if let Some(rest) = body.strip_prefix(mark) {
            body = rest;
        } else if let Some(rest) = body.strip_prefix(flipped.as_slice()) {
            (big, body) = (true, rest);
        }
    }
    let name = match (four, big) {
        (false, false) => "utf-16-le",
        (false, true) => "utf-16-be",
        (true, false) => "utf-32-le",
        (true, true) => "utf-32-be",
    };
    let whole = body.len() / size * size;
    if whole != body.len() {
        return Err(refused(name, body, whole, "truncated data"));
    }
    let units = body.chunks(size).map(|chunk| {
        let mut word = [0u8; 4];
        word[..size].copy_from_slice(chunk);
        if big {
            word[..size].reverse();
        }
        u32::from_le_bytes(word)
    });
    if four {
        return units
            .enumerate()
            .map(|(i, u)| {
                char::from_u32(u)
                    .ok_or_else(|| refused(name, body, i * 4, "code point not in range(0x110000)"))
            })
            .collect();
    }
    let units: Vec<u16> = units.map(|u| u16::try_from(u).unwrap_or(0)).collect();
    char::decode_utf16(units.iter().copied())
        .enumerate()
        .map(|(i, c)| c.map_err(|_| refused(name, body, i * 2, "illegal encoding")))
        .collect()
}

pub(super) fn decode(bytes: &[u8], codec: Codec) -> Result<String, String> {
    match codec {
        Codec::Utf8 => utf8(bytes),
        Codec::Utf8Sig => utf8(bytes.strip_prefix(&[0xef, 0xbb, 0xbf][..]).unwrap_or(bytes)),
        Codec::Latin1 => Ok(bytes.iter().map(|b| char::from(*b)).collect()),
        Codec::Ascii => narrow(bytes, "ascii", "ordinal not in range(128)", |b| {
            b.is_ascii().then(|| char::from(b))
        }),
        Codec::Cp1252 => narrow(bytes, "charmap", UNDEFINED, |b| charmap(b, cp1252)),
        Codec::Latin9 => narrow(bytes, "charmap", UNDEFINED, |b| charmap(b, latin9)),
        Codec::Utf16(order) => wide(bytes, order, false),
        Codec::Utf32(order) => wide(bytes, order, true),
    }
}
