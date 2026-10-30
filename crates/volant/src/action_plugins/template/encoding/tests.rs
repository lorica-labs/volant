// SPDX-License-Identifier: GPL-3.0-or-later
use super::{decode_text, encode};

/// Each row was written by `template` of ansible-core 2.19.12 with this `output_encoding`.
#[test]
fn the_bytes_are_the_ones_python_writes() {
    for (name, text, bytes) in [
        ("latin-1", "\u{e9}t\u{e9}\n", &b"\xe9t\xe9\n"[..]),
        ("l1", "\u{e9}t\u{e9}\n", b"\xe9t\xe9\n"),
        ("ISO_8859-1", "\u{e9}t\u{e9}\n", b"\xe9t\xe9\n"),
        ("Latin1", "\u{e9}t\u{e9}\n", b"\xe9t\xe9\n"),
        ("latin 1", "x\n", b"x\n"),
        ("ascii", "cafe\n", b"cafe\n"),
        ("UTF8", "\u{e9}t\u{e9}\n", b"\xc3\xa9t\xc3\xa9\n"),
        (
            "utf-8-sig",
            "\u{e9}t\u{e9}\n",
            b"\xef\xbb\xbf\xc3\xa9t\xc3\xa9\n",
        ),
        ("cp1252", "euro \u{20ac}\n", b"euro \x80\n"),
        (
            "windows-1252",
            "\u{201c}x\u{201d} \u{e9}t\u{e9}\n",
            b"\x93x\x94 \xe9t\xe9\n",
        ),
        (
            "iso-8859-15",
            "\u{e9}t\u{e9} \u{20ac}\n",
            b"\xe9t\xe9 \xa4\n",
        ),
        ("latin9", "\u{152}\n", b"\xbc\n"),
        ("utf-16", "\u{e9}t\u{e9}\n", b"\xff\xfe\xe9\0t\0\xe9\0\n\0"),
        ("U16", "a\n", b"\xff\xfea\0\n\0"),
        ("utf-16", "", b"\xff\xfe"),
        ("utf-16-le", "\u{e9}t\u{e9}\n", b"\xe9\0t\0\xe9\0\n\0"),
        ("UTF_16_BE", "\u{e9}t\u{e9}\n", b"\0\xe9\0t\0\xe9\0\n"),
        ("utf-16-le", "a\u{1f600}\n", b"a\0=\xd8\0\xde\n\0"),
        (
            "utf-32",
            "a\u{e9}\n",
            b"\xff\xfe\0\0a\0\0\0\xe9\0\0\0\n\0\0\0",
        ),
    ] {
        assert_eq!(encode(text, name).unwrap(), bytes, "{name}");
    }
}

/// The error texts of Python, with its position counted in characters, and a run of characters
/// that do not fit named as one range.
#[test]
fn a_character_that_does_not_fit_is_named_as_python_names_it() {
    for (name, text, err) in [
        (
            "latin-1",
            "euro \u{20ac}\n",
            "'latin-1' codec can't encode character '\\u20ac' in position 5: ordinal not in range(256)",
        ),
        (
            "ascii",
            "\u{e9}t\u{e9}\n",
            "'ascii' codec can't encode character '\\xe9' in position 0: ordinal not in range(128)",
        ),
        (
            "US-ASCII",
            "\u{e9}t\u{e9}\n",
            "'ascii' codec can't encode character '\\xe9' in position 0: ordinal not in range(128)",
        ),
        (
            "latin-1",
            "\u{20ac}\u{20ac}\n",
            "'latin-1' codec can't encode characters in position 0-1: ordinal not in range(256)",
        ),
        (
            "latin-1",
            "a\u{1f600}\n",
            "'latin-1' codec can't encode character '\\U0001f600' in position 1: ordinal not in range(256)",
        ),
        (
            "cp1252",
            "\u{20ac}\u{81}x",
            "'charmap' codec can't encode character '\\x81' in position 1: character maps to <undefined>",
        ),
        (
            "cp1252",
            "a\u{20ac}\u{4e2d}\u{4e2d}",
            "'charmap' codec can't encode characters in position 2-3: character maps to <undefined>",
        ),
        (
            "iso-8859-15",
            "a\u{104}",
            "'charmap' codec can't encode character '\\u0104' in position 1: character maps to <undefined>",
        ),
    ] {
        assert_eq!(encode(text, name).unwrap_err(), err, "{name}");
    }
}

/// A codec Python has but this release does not write is named, not guessed at.
#[test]
fn a_codec_not_written_yet_is_refused_by_name() {
    for name in ["bogus", "koi8-r", "shift_jis", "utf-7", "unicode_escape"] {
        let err = encode("x", name).unwrap_err();
        assert!(
            err.starts_with(&format!(
                "output_encoding '{name}' is not supported yet on 'template': only utf-8"
            )),
            "{err}"
        );
    }
    assert_eq!(
        encode("x", "rot13").unwrap_err(),
        "'rot13' is not a text encoding; use codecs.encode() to handle arbitrary codecs"
    );
}

/// A name is echoed in the error up to 64 characters.
#[test]
fn a_long_name_is_cut_in_the_error() {
    let err = encode("x", &"z".repeat(1_000_000)).unwrap_err();
    assert!(err.starts_with(&format!("output_encoding '{}...' is not", "z".repeat(64))));
    assert!(err.len() < 300, "{}", err.len());
}

/// Each row is what `bytes.decode` of Python 3.12 returns or raises for the same bytes.
#[test]
fn the_bytes_read_back_as_python_reads_them() {
    for (name, bytes, text) in [
        ("utf-8", &b"\xc3\xa9"[..], "\u{e9}"),
        ("utf-8-sig", b"\xef\xbb\xbfa", "a"),
        ("utf-8-sig", b"a", "a"),
        ("latin-1", b"\xe9", "\u{e9}"),
        ("cp1252", b"\x80\x93", "\u{20ac}\u{201c}"),
        ("iso-8859-15", b"\xa4\xbc", "\u{20ac}\u{152}"),
        ("utf-16", b"\xff\xfea\0", "a"),
        ("utf-16", b"\xfe\xff\0a", "a"),
        ("utf-16", b"a\0", "a"),
        ("utf-16-be", b"\0a", "a"),
        ("utf-16-le", b"=\xd8\0\xde", "\u{1f600}"),
        ("utf-32", b"\xff\xfe\0\0a\0\0\0", "a"),
        ("utf-32", b"\0\0\xfe\xff\0\0\0a", "a"),
        ("utf-32-be", b"\0\0\0a", "a"),
    ] {
        assert_eq!(decode_text(bytes, name).unwrap(), text, "{name} {bytes:?}");
    }
}

#[test]
fn a_byte_a_codec_cannot_read_is_named_as_python_names_it() {
    for (name, bytes, err) in [
        (
            "ascii",
            &b"a\xe9"[..],
            "'ascii' codec can't decode byte 0xe9 in position 1: ordinal not in range(128)",
        ),
        (
            "cp1252",
            b"a\x81",
            "'charmap' codec can't decode byte 0x81 in position 1: character maps to <undefined>",
        ),
        (
            "utf-8",
            b"a\xc3",
            "'utf-8' codec can't decode byte 0xc3 in position 1: unexpected end of data",
        ),
        (
            "utf-8",
            b"\x80",
            "'utf-8' codec can't decode byte 0x80 in position 0: invalid start byte",
        ),
        (
            "utf-8",
            b"\xc3a",
            "'utf-8' codec can't decode byte 0xc3 in position 0: invalid continuation byte",
        ),
        (
            "utf-8",
            b"\xff",
            "'utf-8' codec can't decode byte 0xff in position 0: invalid start byte",
        ),
        (
            "utf-16",
            b"a",
            "'utf-16-le' codec can't decode byte 0x61 in position 0: truncated data",
        ),
        (
            "utf-16",
            b"\xff\xfea\0b",
            "'utf-16-le' codec can't decode byte 0x62 in position 4: truncated data",
        ),
        (
            "utf-16-le",
            b"\0\xdc",
            "'utf-16-le' codec can't decode bytes in position 0-1: illegal encoding",
        ),
        (
            "utf-16-le",
            b"=\xd8\0\xde\0\xdc",
            "'utf-16-le' codec can't decode bytes in position 4-5: illegal encoding",
        ),
        (
            "utf-16-le",
            b"\0\xd8a\0",
            "'utf-16-le' codec can't decode bytes in position 0-1: illegal UTF-16 surrogate",
        ),
        (
            "utf-16-le",
            b"\0\xd8",
            "'utf-16-le' codec can't decode bytes in position 0-1: unexpected end of data",
        ),
        (
            "utf-16-le",
            b"a\0\0\xd8\0",
            "'utf-16-le' codec can't decode bytes in position 2-4: unexpected end of data",
        ),
        (
            "utf-32-le",
            b"\0\0\x11\0",
            "'utf-32-le' codec can't decode bytes in position 0-3: code point not in range(0x110000)",
        ),
        (
            "utf-32-be",
            b"\0\x11\0\0",
            "'utf-32-be' codec can't decode bytes in position 0-3: code point not in range(0x110000)",
        ),
        (
            "utf-32-le",
            b"a\0\0\0\0\xd8\0\0",
            "'utf-32-le' codec can't decode bytes in position 4-7: code point in surrogate code point range(0xd800, 0xe000)",
        ),
        (
            "utf-32",
            b"abc",
            "'utf-32-le' codec can't decode bytes in position 0-2: truncated data",
        ),
        (
            "utf-32-le",
            b"a\0\0\0b\0",
            "'utf-32-le' codec can't decode bytes in position 4-5: truncated data",
        ),
    ] {
        assert_eq!(
            decode_text(bytes, name).unwrap_err(),
            err,
            "{name} {bytes:?}"
        );
    }
}
