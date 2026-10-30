// SPDX-License-Identifier: GPL-3.0-or-later
use serde_json::json;

use super::literal_eval;

/// Each value of ansible-core's `inventory_ini` integration target, and a few edges, as
/// `ansible-inventory --host` 2.19.12 shows them.
#[test]
fn values_read_as_the_reference_reads_them() {
    let cases = [
        ("True", Some(json!(true))),
        ("1", Some(json!(1))),
        ("1.2", Some(json!(1.2))),
        ("1j", Some(json!("1j"))),
        ("-1j", Some(json!("(-0-1j)"))),
        ("1+2j", Some(json!("(1+2j)"))),
        ("0-1j", Some(json!("-1j"))),
        ("'single'", Some(json!("single"))),
        ("\"double\"", Some(json!("double"))),
        ("b'single'", Some(json!("single"))),
        ("b\"double\"", Some(json!("double"))),
        ("...", Some(json!("..."))),
        ("{\"a\": \"b\"}", Some(json!({"a": "b"}))),
        ("[1, 2]", Some(json!([1, 2]))),
        ("(1, 2)", Some(json!([1, 2]))),
        ("(1,)", Some(json!([1]))),
        ("(1)", Some(json!(1))),
        ("{1, 2}", Some(json!([1, 2]))),
        ("set()", Some(json!([]))),
        ("None", Some(json!(null))),
        (
            "[..., 1j, set(), {1, 2}, b'x']",
            Some(json!(["...", "1j", [], [1, 2], "x"])),
        ),
        ("'a\\nb' 'c'", Some(json!("a\nbc"))),
        ("r'a\\nb'", Some(json!("a\\nb"))),
        ("0x1F", Some(json!(31))),
        ("1_000", Some(json!(1000))),
        ("-5", Some(json!(-5))),
        ("1e3", Some(json!(1000.0))),
        ("{1: 'a', True: 'b'}", Some(json!({"1": "b"}))),
        ("invalid", None),
        ("host-01.internal", None),
        ("01", None),
        ("1abc", None),
        ("f'x'", None),
        ("[1, 2", None),
        ("1 + 2", None),
    ];
    for (text, want) in cases {
        assert_eq!(literal_eval(text), want, "{text}");
    }
}
