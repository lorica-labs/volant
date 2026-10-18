// SPDX-License-Identifier: GPL-3.0-or-later
//! The encodings both ends of the wire have to agree on byte for byte.
//!
//! Base64 is written here rather than pulled in: the agent is uploaded to every managed host, and
//! one codec of forty lines is cheaper than a dependency for two calls. Deflate is not forty
//! lines, so it comes from `miniz_oxide`, pure Rust. One copy for both crates, so the controller
//! and the agent cannot read the same bytes two ways.

/// The most a blob may decode to. The agent refuses a `put_blob` announcing more, reading the
/// frame after it through without holding it.
pub const BLOB_LIMIT: usize = 256 * 1024 * 1024;

/// `bytes` as a zlib stream at level 6: the adler-32 at its end lets [`inflate`] refuse a stream
/// damaged on the way, before the blake3 of the result is even taken.
pub fn deflate(bytes: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(bytes, 6)
}

/// The bytes a [`deflate`] stream holds, refused once they would pass `limit`, and refused when
/// the stream is truncated or its checksum is wrong. Never allocates past `limit`.
pub fn inflate(bytes: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(bytes, limit).map_err(|err| {
        match err.status {
            miniz_oxide::inflate::TINFLStatus::HasMoreOutput => {
                format!("the deflated blob holds more than {limit} bytes")
            }
            status => format!("the deflated blob does not inflate: {status:?}"),
        }
    })
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding, the only form `modify_module` and this engine produce.
pub fn b64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut block = [0u8; 3];
        block[..chunk.len()].copy_from_slice(chunk);
        let bits = u32::from_be_bytes([0, block[0], block[1], block[2]]);
        for slot in 0..4 {
            if slot <= chunk.len() {
                out.push(ALPHABET[(bits >> (18 - 6 * slot)) as usize & 0x3f] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Strict: `\n` and `\r` are skipped, padding is counted and must be exactly what the
/// remaining bits call for, anything else is refused naming the offset.
///
/// A payload half-decoded into something that happens to hash to nothing is not a failure an
/// operator could read, so the refusal says where the text broke.
pub fn b64_decode(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut pads = 0usize;
    let mut padding_at = 0usize;
    for (index, &byte) in text.as_bytes().iter().enumerate() {
        match byte {
            b'\n' | b'\r' => continue,
            b'=' => {
                if pads == 0 {
                    padding_at = index;
                }
                pads += 1;
                continue;
            }
            _ if pads > 0 => {
                return Err(format!("base64 character after padding at offset {index}"));
            }
            _ => {}
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err(format!("invalid base64 character at offset {index}")),
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits == 24 {
            out.extend_from_slice(&[(acc >> 16) as u8, (acc >> 8) as u8, acc as u8]);
            acc = 0;
            bits = 0;
        }
    }
    let wanted = match bits {
        0 => 0,
        12 => {
            out.push((acc >> 4) as u8);
            2
        }
        18 => {
            out.push((acc >> 10) as u8);
            out.push((acc >> 2) as u8);
            1
        }
        _ => {
            return Err(format!(
                "base64 ends in the middle of a byte at offset {}",
                text.len()
            ));
        }
    };
    if pads != wanted {
        return Err(format!(
            "base64 padding at offset {padding_at} is {pads} '=' where the data calls for {wanted}"
        ));
    }
    Ok(out)
}

/// Lowercase hex SHA-1, the checksum `copy` compares against the one `stat` reports.
pub fn sha1_hex(bytes: &[u8]) -> String {
    sha1_smol::Sha1::from(bytes).digest().to_string()
}

#[cfg(test)]
mod tests;
