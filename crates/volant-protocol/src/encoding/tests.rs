// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

/// One decoder for both crates, pinned to literal vectors rather than to itself.
#[test]
fn base64_decodes_every_padding_by_known_answer() {
    assert_eq!(b64_decode("UEsDBA==").unwrap(), b"PK\x03\x04");
    assert_eq!(b64_decode("QQ==").unwrap(), b"A");
    assert_eq!(b64_decode("QUI=").unwrap(), b"AB");
    assert_eq!(b64_decode("QUJD").unwrap(), b"ABC");
    assert_eq!(b64_decode("QUJD\r\nREVG").unwrap(), b"ABCDEF");
    assert!(b64_decode("").unwrap().is_empty());
}

/// Base64 padding is counted, so a bare `=` or padding the bits do not call for is refused.
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

/// Deflate round-trips, and its output is a zlib stream a stock decoder reads: the header
/// bytes below are the ones zlib writes at its default level.
#[test]
fn deflate_round_trips_through_inflate() {
    let bytes = b"PK".repeat(1000);
    let packed = deflate(&bytes);
    assert!(packed.len() < bytes.len() / 10, "{} bytes", packed.len());
    assert_eq!(packed[0], 0x78);
    assert_eq!(inflate(&packed, bytes.len()).unwrap(), bytes);
    assert_eq!(inflate(&deflate(b""), 0).unwrap(), b"");
}

/// `inflate` refuses a stream that inflates past the limit its caller announced.
#[test]
fn inflate_refuses_a_stream_past_its_limit() {
    let bomb = deflate(&vec![0u8; 1024 * 1024]);
    let err = inflate(&bomb, 1000).unwrap_err();
    assert!(err.contains("more than 1000 bytes"), "{err}");
}

/// A stream cut short or with a flipped byte is refused, never returned as the bytes it held
/// so far.
#[test]
fn inflate_refuses_a_truncated_or_damaged_stream() {
    let bytes: Vec<u8> = (0..20_000u32).map(|n| (n * 7 % 251) as u8).collect();
    let packed = deflate(&bytes);
    assert!(inflate(&packed[..packed.len() - 1], bytes.len()).is_err());
    assert!(inflate(&packed[..packed.len() / 2], bytes.len()).is_err());
    let mut flipped = packed.clone();
    let last = flipped.len() - 1;
    flipped[last] ^= 1;
    assert!(inflate(&flipped, bytes.len()).is_err());
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
