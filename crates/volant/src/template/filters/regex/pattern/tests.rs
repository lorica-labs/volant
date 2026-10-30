// SPDX-License-Identifier: GPL-3.0-or-later
use rstest::rstest;

use super::{Failure, translate};

/// Each pattern compiles under Python's `re`; the escapes it names become the characters they
/// stand for, and a character the `regex` crate reads specially is written as a hex escape.
#[rstest]
#[case(r"\A\Z", r"\A\z")]
#[case(r"\d\W", r"\d\W")]
#[case(r"\x41", "A")]
#[case(r"é", "\u{e9}")]
#[case(r"\U0001F600", "\u{1f600}")]
#[case(r"\n", r"\x{A}")]
#[case(r"\.", r"\x{2E}")]
#[case(r"\-", r"\x{2D}")]
#[case(r"\#", r"\x{23}")]
#[case(r"\\", r"\x{5C}")]
#[case(r"\0", r"\x{0}")]
#[case(r"\012", r"\x{A}")]
#[case(r"\101", "A")]
#[case(r"(a)\1", r"(a)\1")]
#[case("[a-c]", "[a-c]")]
#[case("[a-a]", "[a-a]")]
#[case(r"[^\d]", r"[^\d]")]
#[case("[-a]", r"[\x{2D}a]")]
#[case("[]a]", r"[\x{5D}a]")]
#[case(r"[\w-]", r"[\w\x{2D}]")]
#[case(r"[a\-z]", r"[a\x{2D}z]")]
#[case(r"[\b]", r"[\x{8}]")]
#[case(r"[\101]", "[A]")]
#[case(r"[\7]", r"[\x{7}]")]
#[case(r"[\n]", r"[\x{A}]")]
#[case(r"[\x41]", "[A]")]
#[case("[.]", r"[\x{2E}]")]
#[case("[\u{e9}]", "[\u{e9}]")]
fn an_escape_becomes_the_character_python_reads(#[case] pattern: &str, #[case] rust: &str) {
    let translated = translate(pattern, false).unwrap_or_else(|e| panic!("{pattern}: {e:?}"));
    assert_eq!(translated.strict, rust, "{pattern}");
}

/// The texts are the ones `re.compile` of Python 3.12 raises for the same pattern.
#[rstest]
#[case("\\", r"bad escape (end of pattern) at position 0")]
#[case(r"\q", r"bad escape \q at position 0")]
#[case(r"\x4", r"incomplete escape \x4 at position 0")]
#[case(r"\u12", r"incomplete escape \u12 at position 0")]
#[case(r"\U00110000", r"bad escape \U00110000 at position 0")]
#[case(
    r"\400",
    r"octal escape value \400 outside of range 0-0o377 at position 0"
)]
#[case("[a", "unterminated character set at position 0")]
#[case("[a-", "unterminated character set at position 0")]
#[case(r"[\8]", r"bad escape \8 at position 1")]
#[case(r"[\Z]", r"bad escape \Z at position 1")]
#[case("[\\", r"bad escape (end of pattern) at position 1")]
#[case(
    r"[\777]",
    r"octal escape value \777 outside of range 0-0o377 at position 1"
)]
fn a_bad_escape_or_set_fails_as_python_fails(#[case] pattern: &str, #[case] error: &str) {
    assert_eq!(
        translate(pattern, false).err(),
        Some(Failure::Invalid(error.to_string())),
        "{pattern}"
    );
}

#[test]
fn a_named_character_is_not_run() {
    assert_eq!(
        translate(r"\N{DIGIT ONE}", false).err(),
        Some(Failure::Unsupported(
            r"a named character, '\N{name}'".to_string()
        ))
    );
}
