// SPDX-License-Identifier: GPL-3.0-or-later
use serde_json::{Map, Value, json};

use crate::testing::templar;

fn vars(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

/// A width, a count or a precision from a host can ask for more text than a machine holds. The
/// method stops before it builds the text, and the error names the limit.
#[test]
fn a_size_past_the_limit_is_a_task_error_before_any_text_is_built() {
    let big = "b".repeat(20_000_000);
    let v = vars(json!({"w": 10_000_000_000_i64, "t": "a\tb", "big": big}));
    for template in [
        "{{ 'a'.ljust(10000000000) }}",
        "{{ 'a'.rjust(10000000000) }}",
        "{{ 'a'.center(10000000000) }}",
        "{{ 'a'.zfill(10000000000) }}",
        "{{ 'a'.ljust(w) }}",
        "{{ t.expandtabs(2000000000) }}",
        "{{ big.replace('b', big) }}",
        "{{ '' | indent(10000000000) }}",
        "{{ 'x\ny' | indent(w) }}",
        "{{ '{:>10000000000}'.format('a') }}",
        "{{ '{:>{}}'.format('a', w) }}",
        "{{ '{:.10000000000f}'.format(1.5) }}",
        "{{ '{:.10000000000e}'.format(1.5) }}",
        "{{ '{:.10000000000g}'.format(1.5) }}",
        // A fill of four bytes, and many fields that each stay within the limit.
        "{{ 'a'.ljust(100000000, '𝄞') }}",
        "{{ '{:>200000000}{:>200000000}'.format('a', 'a') }}",
        "{{ 'aaaaaaaaaaaaaaaa'.translate({97: big}) }}",
    ] {
        let err = templar().render(template, &v).unwrap_err();
        assert!(err.0.contains("256 MiB"), "{template}: {err}");
    }
}

/// `encode`, `decode` and `hex` check the size of their result from the length alone, so the
/// test asks with a length and builds nothing.
#[test]
fn encode_decode_and_hex_check_their_size_before_building() {
    use super::super::bytes::{check_decode, check_encode, check_hex};
    use super::super::codec::{Codec, Order};
    const HUNDRED_MILLION: usize = 100_000_000;
    for (name, result) in [
        (
            "utf-32",
            check_encode(HUNDRED_MILLION, Codec::Utf32(Order::Little, true), "strict"),
        ),
        (
            "ascii backslashreplace",
            check_encode(HUNDRED_MILLION, Codec::Ascii, "backslashreplace"),
        ),
        ("decode", check_decode(HUNDRED_MILLION, "backslashreplace")),
        ("hex", check_hex(HUNDRED_MILLION)),
    ] {
        let err = result.expect_err(name);
        assert!(err.to_string().contains("256 MiB"), "{name}: {err}");
    }
    assert!(check_encode(HUNDRED_MILLION, Codec::Utf8, "strict").is_ok());
    assert!(check_decode(HUNDRED_MILLION, "strict").is_ok());
    assert!(check_hex(1000).is_ok());
}

/// A count past what an index holds is Python's own error, and a bound past it is a clamp.
#[test]
fn a_count_past_an_index_is_pythons_error() {
    let t = templar();
    let none = Map::new();
    for (template, want) in [
        ("{{ 'a'.rjust(100000000000000000000) }}", "C ssize_t"),
        ("{{ 'a'.expandtabs(10000000000) }}", "C int"),
        (
            "{{ '{:>100000000000000000000}'.format('a') }}",
            "Too many decimal digits",
        ),
    ] {
        let err = t.render(template, &none).unwrap_err();
        assert!(err.0.contains(want), "{template}: {err}");
    }
    assert_eq!(
        t.render("{{ 'abc'.find('b', 100000000000000000000) }}", &none)
            .unwrap(),
        json!(-1)
    );
}

/// A float prints as many digits as it is asked for, past the 65535 that Rust's formatter takes.
#[test]
fn a_float_precision_past_65535_prints_zeros() {
    let t = templar();
    let none = Map::new();
    assert_eq!(
        t.render("{{ '{:.70000f}'.format(1.5) | length }}", &none)
            .unwrap(),
        json!(70002)
    );
    assert_eq!(
        t.render("{{ '{:.70000e}'.format(1.5) | length }}", &none)
            .unwrap(),
        json!(70006)
    );
    assert_eq!(
        t.render("{{ '{:.70000g}'.format(1.5) }}", &none).unwrap(),
        json!("1.5")
    );
}

/// A zero-padded grouped number is built in one pass, however wide the field is.
#[test]
fn a_wide_zero_padded_grouped_number_is_built_at_once() {
    let out = templar()
        .render("{{ ('{:0300000,d}'.format(1)) | length }}", &Map::new())
        .unwrap();
    // Python never starts the field with a separator, so it ends one place past the width.
    assert_eq!(out, json!(300_001));
}
