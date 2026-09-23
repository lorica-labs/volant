// SPDX-License-Identifier: GPL-3.0-or-later
//! The two encodings both ends of the wire have to agree on byte for byte.
//!
//! Written here rather than pulled in: the agent is uploaded to every managed host, and one
//! codec of forty lines is cheaper than a dependency for two calls. One copy for both crates, so
//! the controller and the agent cannot read the same text two ways.

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
mod tests {
    use super::*;

    /// One decoder for both crates, pinned to literal vectors rather than to itself.
    ///
    /// What would make this red: the `bits == 18` arm (one `=`, the commonest case) dropping
    /// its second byte, which no round-trip test sees because the encoder beside it would
    /// drop it too.
    #[test]
    fn base64_decodes_every_padding_by_known_answer() {
        assert_eq!(b64_decode("UEsDBA==").unwrap(), b"PK\x03\x04");
        assert_eq!(b64_decode("QQ==").unwrap(), b"A");
        assert_eq!(b64_decode("QUI=").unwrap(), b"AB");
        assert_eq!(b64_decode("QUJD").unwrap(), b"ABC");
        assert_eq!(b64_decode("QUJD\r\nREVG").unwrap(), b"ABCDEF");
        assert!(b64_decode("").unwrap().is_empty());
    }

    /// Padding is counted. What would make this red: the lenient decoder this replaces, which
    /// read every `=` as "ignore and remember" and accepted a bare `=`.
    #[test]
    fn base64_refuses_padding_the_bits_do_not_call_for() {
        for bad in [
            "=",
            "====",
            "QQ=",
            "QQ===",
            "QUI==",
            "UEsDBA=========",
            "QQ==QQ==",
        ] {
            assert!(b64_decode(bad).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn base64_encodes_what_it_decodes() {
        for n in 0..=9u8 {
            let bytes: Vec<u8> = (0..n).collect();
            assert_eq!(b64_decode(&b64_encode(&bytes)).unwrap(), bytes);
        }
        assert_eq!(b64_encode(b"abc"), "YWJj");
    }

    /// The SHA-1 `copy` compares, by known answer: `sha1("hello\n")`, which `stat` reported
    /// for the probe file of measure 2 on the reference.
    #[test]
    fn sha1_is_the_checksum_stat_reports() {
        assert_eq!(
            sha1_hex(b"hello\n"),
            "f572d396fae9206628714fb2ce00f72e94f2258f"
        );
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }
}
