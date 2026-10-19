// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use serde_json::json;

/// A key written twice keeps its last value instead of being refused.
#[test]
fn a_key_written_twice_keeps_its_last_value() {
    let docs = load(
        "a: 1
b: 2
a: 3
",
        "t.yml",
    )
    .unwrap();
    assert_eq!(to_json(&docs[0]).unwrap(), json!({"a": 3, "b": 2}));
}

#[test]
fn scalars_sequences_and_mappings_convert_to_json() {
    let docs = load("a: 1\nb: [x, 2.5, true, ~]\nc:\n  d: text\n", "t.yml").unwrap();
    let v = to_json(&docs[0]).unwrap();
    assert_eq!(
        v,
        json!({"a": 1, "b": ["x", 2.5, true, null], "c": {"d": "text"}})
    );
}

/// A document of only markers and comments is null, while an empty block scalar and an empty
/// `!!str` stay empty strings, as in PyYAML.
#[test]
fn an_empty_document_is_null_and_an_empty_block_scalar_is_not() {
    for text in ["---\n# only a comment\n", "---\n", "--- # c\n...\n"] {
        let docs = load(text, "t.yml").unwrap();
        assert_eq!(to_json(&docs[0]).unwrap(), Value::Null, "{text:?}");
    }
    for (text, want) in [
        ("a: |\nb: 1\n", json!({"a": "", "b": 1})),
        ("a: |-\n\nb: 2\n", json!({"a": "", "b": 2})),
        ("- |\n- x\n", json!(["", "x"])),
        ("a: !!str\n", json!({"a": ""})),
        ("b: ''\nc: \"\"\n", json!({"b": "", "c": ""})),
    ] {
        let docs = load(text, "t.yml").unwrap();
        assert_eq!(to_json(&docs[0]).unwrap(), want, "{text:?}");
    }
}

#[test]
fn pyyaml_booleans_are_recognised_on_demand_only() {
    let docs = load("a: yes\nb: no\nc: maybe\n", "t.yml").unwrap();
    assert_eq!(as_bool(field(&docs[0], "a").unwrap()), Some(true));
    assert_eq!(as_bool(field(&docs[0], "b").unwrap()), Some(false));
    assert_eq!(as_bool(field(&docs[0], "c").unwrap()), None);
    // Converted as data too, matching PyYAML: unquoted `yes` reads as a variable's value
    // is a boolean, the same as Ansible's own `ignore_errors: yes`.
    assert_eq!(to_json(field(&docs[0], "a").unwrap()).unwrap(), json!(true));
}

#[test]
fn a_quoted_pyyaml_boolean_spelling_stays_a_string() {
    let docs = load("flag: \"yes\"\n", "t.yml").unwrap();
    let node = field(&docs[0], "flag").unwrap();
    assert_eq!(
        as_bool(node),
        Some(true),
        "quoting doesn't stop a keyword field"
    );
    assert_eq!(
        to_json(node).unwrap(),
        json!("yes"),
        "quoting keeps a generic data value a string"
    );
}

/// Every boolean spelling is read the same by the resolver, `as_bool` and `bool_from_str`,
/// which share one list.
#[test]
fn one_list_of_boolean_spellings_serves_every_reader() {
    for (spellings, want) in [(PYYAML_TRUE, true), (PYYAML_FALSE, false)] {
        for spelling in spellings {
            assert_eq!(bool_from_str(spelling), Some(want), "{spelling}");
            let text = format!("flag: {spelling}\n");
            let docs = load(&text, "t.yml").unwrap();
            let node = field(&docs[0], "flag").unwrap();
            assert_eq!(as_bool(node), Some(want), "{spelling} as a keyword");
            assert_eq!(to_json(node).unwrap(), json!(want), "{spelling} as data");
        }
    }
    assert_eq!(bool_from_str("maybe"), None);
}

/// Each spelling and its type as `ansible-core 2.19.12`'s own PyYAML reads it, measured
/// directly.
#[test]
fn integer_and_float_spellings_are_measured_against_the_reference() {
    let cases: &[(&str, Value)] = &[
        ("010", json!(8)),
        ("\"010\"", json!("010")),
        ("0_755", json!(493)),
        ("08", json!("08")),
        ("0o10", json!("0o10")),
        ("0b1010", json!(10)),
        ("0b_1010", json!(10)),
        ("0x1F", json!(31)),
        ("0x_1F", json!(31)),
        ("1_000", json!(1000)),
        ("1000", json!(1000)),
        ("+12", json!(12)),
        ("-07", json!(-7)),
        ("+0b11", json!(3)),
        ("0", json!(0)),
        ("00", json!(0)),
        ("0_0", json!(0)),
        ("1e3", json!("1e3")),
        ("1e+3", json!("1e+3")),
        ("1.0e3", json!("1.0e3")),
        ("1.0e+3", json!(1000.0)),
        ("2.5", json!(2.5)),
        (".5", json!(0.5)),
        ("1_0.5", json!(10.5)),
        ("1e", json!("1e")),
        ("0xg", json!("0xg")),
    ];
    for (raw, want) in cases {
        let text = format!("x: {raw}\n");
        let docs = load(&text, "t.yml").unwrap();
        let got = to_json(field(&docs[0], "x").unwrap()).unwrap();
        assert_eq!(&got, want, "{raw}");
    }
}

/// Loads each text and compares it with the value ansible-core 2.19.12 gave it (`DataLoader().load`),
/// as JSON.
fn assert_loads_as(cases: &[(&str, Value)]) {
    for (text, want) in cases {
        let docs = load(text, "t.yml").unwrap_or_else(|e| panic!("{text:?}: {e:#}"));
        assert_eq!(&to_json(&docs[0]).unwrap(), want, "{text:?}");
    }
}

/// An alias is the node its anchor names, whatever the node is.
#[test]
fn an_alias_has_the_value_of_its_anchor() {
    assert_loads_as(&[
        ("a: &n 5\nb: *n\n", json!({"a": 5, "b": 5})),
        ("a: &n 1.5\nb: *n\n", json!({"a": 1.5, "b": 1.5})),
        ("a: &n yes\nb: *n\n", json!({"a": true, "b": true})),
        ("a: &n no\nb: *n\n", json!({"a": false, "b": false})),
        ("a: &n ~\nb: *n\n", json!({"a": null, "b": null})),
        ("a: &n abc\nb: *n\n", json!({"a": "abc", "b": "abc"})),
        ("a: &n \"x y\"\nb: *n\n", json!({"a": "x y", "b": "x y"})),
        ("a: &n \"5\"\nb: *n\n", json!({"a": "5", "b": "5"})),
        (
            "a: &n |\n  line1\n  line2\nb: *n\n",
            json!({"a": "line1\nline2\n", "b": "line1\nline2\n"}),
        ),
        (
            "a: &n >\n  a\n  b\nb: *n\n",
            json!({"a": "a b\n", "b": "a b\n"}),
        ),
        ("a: &n 010\nb: *n\n", json!({"a": 8, "b": 8})),
        ("a: &n 0x1F\nb: *n\n", json!({"a": 31, "b": 31})),
        (
            "a: &k key\nm:\n  *k : 1\n",
            json!({"a": "key", "m": {"key": 1}}),
        ),
        (
            "a: &n 5\nb: [*n, *n, x]\n",
            json!({"a": 5, "b": [5, 5, "x"]}),
        ),
        (
            "a: &n yes\nb: [*n, 1]\n",
            json!({"a": true, "b": [true, 1]}),
        ),
        ("a: &s [1, 2]\nb: *s\n", json!({"a": [1, 2], "b": [1, 2]})),
        (
            "a: &m {x: 1}\nb: *m\n",
            json!({"a": {"x": 1}, "b": {"x": 1}}),
        ),
        ("- &n 5\n- *n\n", json!([5, 5])),
        ("a: &n !!str 5\nb: *n\n", json!({"a": "5", "b": "5"})),
        ("a: &n !!int '5'\nb: *n\n", json!({"a": 5, "b": 5})),
    ]);
}

/// `!unsafe` resolves the text as a plain scalar whatever its quotes are, and the alias of such a
/// scalar is the same.
#[test]
fn an_unsafe_scalar_is_resolved_as_a_plain_one() {
    assert_loads_as(&[
        ("a: &n !unsafe 5\nb: *n\n", json!({"a": 5, "b": 5})),
        ("a: !unsafe 5\n", json!({"a": 5})),
        ("a: !unsafe 1.5\n", json!({"a": 1.5})),
        ("a: !unsafe yes\n", json!({"a": true})),
        ("a: !unsafe ~\n", json!({"a": null})),
        ("a: !unsafe abc\n", json!({"a": "abc"})),
        ("a: !unsafe \"5\"\n", json!({"a": 5})),
        ("a: !unsafe 010\n", json!({"a": 8})),
        ("a: !unsafe 0x1F\n", json!({"a": 31})),
        (
            "a: !unsafe |\n  line1\n  line2\n",
            json!({"a": "line1\nline2\n"}),
        ),
        ("!unsafe 5\n", json!(5)),
        (
            "a: &s !unsafe [yes, 010]\nb: *s\n",
            json!({"a": [true, 8], "b": [true, 8]}),
        ),
        ("a: !unsafe ''\n", json!({"a": null})),
        ("a: !unsafe 'yes'\n", json!({"a": true})),
        ("a: !unsafe .5\n", json!({"a": 0.5})),
        ("a: !unsafe 1:30\n", json!({"a": 90})),
        ("a: !unsafe |\n  5\n", json!({"a": 5})),
    ]);
}

/// An explicit tag of the core schema decides the type of a scalar, quoted or not.
#[test]
fn an_explicit_core_tag_picks_the_type() {
    assert_loads_as(&[
        ("a: !!str 5\n", json!({"a": "5"})),
        ("a: !!str yes\n", json!({"a": "yes"})),
        ("a: !!str ~\n", json!({"a": "~"})),
        ("a: !!int \"5\"\n", json!({"a": 5})),
        ("a: !!int 0x10\n", json!({"a": 16})),
        ("a: !!int 010\n", json!({"a": 8})),
        ("a: !!int 1_000\n", json!({"a": 1000})),
        ("a: !!int 0o10\n", json!({"a": 8})),
        ("a: !!float 5\n", json!({"a": 5.0})),
        ("a: !!float \"1.5\"\n", json!({"a": 1.5})),
        ("a: !!bool yes\n", json!({"a": true})),
        ("a: !!bool \"yes\"\n", json!({"a": true})),
        ("a: !!bool YES\n", json!({"a": true})),
        ("a: !!null x\n", json!({"a": null})),
        ("a: !!null 5\n", json!({"a": null})),
        ("a: !!python/unicode 5\n", json!({"a": "5"})),
        ("a: !!int ' 5 '\n", json!({"a": 5})),
        ("a: !!int --5\n", json!({"a": 5})),
        ("a: !!float --5\n", json!({"a": 5.0})),
    ]);
}

/// Plain scalars the resolver types, with the near misses it leaves as strings. The wide integers and
/// the non-finite floats are not here: the next tests hold them.
#[test]
fn plain_scalars_are_resolved_as_pyyaml_resolves_them() {
    assert_loads_as(&[
        ("a: 1:30\n", json!({"a": 90})),
        ("a: 1:30:00\n", json!({"a": 5400})),
        ("a: 190:20:30\n", json!({"a": 685230})),
        ("a: 1_0:30\n", json!({"a": 630})),
        ("a: 0:30\n", json!({"a": "0:30"})),
        ("a: 1:30.5\n", json!({"a": 90.5})),
        ("a: 190:20:30.15\n", json!({"a": 685230.15})),
        ("a: -190:20:30\n", json!({"a": -685230})),
        ("a: 1.5:30\n", json!({"a": "1.5:30"})),
        ("a: 9:60:60\n", json!({"a": "9:60:60"})),
        ("a: +.5\n", json!({"a": "+.5"})),
        ("a: -.5\n", json!({"a": "-.5"})),
        ("a: .5\n", json!({"a": 0.5})),
        ("a: 5.\n", json!({"a": 5.0})),
        ("a: 1.e+3\n", json!({"a": 1000.0})),
        ("a: 1._5\n", json!({"a": 1.5})),
        ("a: 010\n", json!({"a": 8})),
        ("a: 0o10\n", json!({"a": "0o10"})),
        ("a: 0x1F\n", json!({"a": 31})),
        ("a: 09\n", json!({"a": "09"})),
        ("a: 1e3\n", json!({"a": "1e3"})),
        ("a: 1.0e+3\n", json!({"a": 1000.0})),
        ("a: ~\n", json!({"a": null})),
        ("a: null\n", json!({"a": null})),
        ("a: Yes\n", json!({"a": true})),
        ("a: yEs\n", json!({"a": "yEs"})),
        ("a: 0_8\n", json!({"a": "0_8"})),
        ("a: 00_7\n", json!({"a": 7})),
        ("a: 0B1\n", json!({"a": "0B1"})),
    ]);
}

/// An integer of any width keeps its digits, whatever base it is written in. The expected values
/// are Python's own.
#[test]
fn an_integer_wider_than_a_word_keeps_every_digit() {
    let wide = "1".to_string() + &"0".repeat(400);
    let cases = [
        ("9223372036854775808", "9223372036854775808"),
        ("-9223372036854775809", "-9223372036854775809"),
        (
            "123456789012345678901234567890",
            "123456789012345678901234567890",
        ),
        ("1_000_000_000_000_000_000_000", "1000000000000000000000"),
        (
            "!!int 123456789012345678901234567890",
            "123456789012345678901234567890",
        ),
        (
            "0x1ffffffffffffffffffffffffffffffffffffff",
            "11417981541647679048466287755595961091061972991",
        ),
        (
            "0xffffffffffffffffffffffffffffffffffffffff",
            "1461501637330902918203684832716283019655932542975",
        ),
        (
            "-0xffffffffffffffffffffffffffffffffffffffff",
            "-1461501637330902918203684832716283019655932542975",
        ),
        (
            "0777777777777777777777777777777777777777777777",
            "43556142965880123323311949751266331066367",
        ),
        (
            &format!("0b{}", "1".repeat(130)),
            "1361129467683753853853498429727072845823",
        ),
        (&wide, &wide),
    ];
    let loaded: Vec<(String, Value)> = cases
        .iter()
        .map(|(yaml, int)| {
            let want = serde_json::from_str(&format!(r#"{{"a": {int}}}"#)).unwrap();
            (format!("a: {yaml}\n"), want)
        })
        .collect();
    let borrowed: Vec<(&str, Value)> = loaded
        .iter()
        .map(|(t, v)| (t.as_str(), v.clone()))
        .collect();
    assert_loads_as(&borrowed);
}

/// Python 3.11 and later refuse more than 4300 decimal digits. A hexadecimal number would cost
/// the square of its length to convert, so it stops at the same limit before any work.
#[test]
fn an_integer_past_pythons_digit_limit_is_refused() {
    for text in [
        format!(
            "a: {}
",
            "9".repeat(4301)
        ),
        format!(
            "a: 0x{}
",
            "f".repeat(3700)
        ),
        format!(
            "a: 0x{}
",
            "f".repeat(2_000_000)
        ),
        format!(
            "a: 0b{}
",
            "1".repeat(20_000)
        ),
    ] {
        let err = format!("{:#}", load(&text, "t.yml").unwrap_err());
        assert!(err.contains("Exceeds the limit (4300 digits)"), "{err}");
    }
    assert!(
        load(
            &format!(
                "a: {}
",
                "9".repeat(4300)
            ),
            "t.yml"
        )
        .is_ok()
    );
    assert!(
        load(
            &format!(
                "a: 0x{}
",
                "f".repeat(3500)
            ),
            "t.yml"
        )
        .is_ok()
    );
}

/// A date and a datetime have no counterpart in the value model: they load as the string that
/// ansible-core prints for them.
#[test]
fn a_date_is_the_text_ansible_core_prints_for_it() {
    assert_loads_as(&[
        ("a: 2020-01-02\n", json!({"a": "2020-01-02"})),
        ("a: 2020-1-2\n", json!({"a": "2020-1-2"})),
        (
            "a: 2020-01-02 10:11:12\n",
            json!({"a": "2020-01-02T10:11:12"}),
        ),
        (
            "a: 2020-01-02T10:11:12Z\n",
            json!({"a": "2020-01-02T10:11:12+00:00"}),
        ),
        (
            "a: 2020-01-02t10:11:12.5+01:00\n",
            json!({"a": "2020-01-02T10:11:12.500000+01:00"}),
        ),
        ("a: !!timestamp 2020-1-2\n", json!({"a": "2020-01-02"})),
        (
            "a: !!timestamp '2020-01-02 10:11:12 -5'\n",
            json!({"a": "2020-01-02T10:11:12-05:00"}),
        ),
        (
            "a: !!timestamp '2020-01-02 10:11:12 +05:30'\n",
            json!({"a": "2020-01-02T10:11:12+05:30"}),
        ),
        (
            "a: !!timestamp 2020-01-02t10:11:12.123456789Z\n",
            json!({"a": "2020-01-02T10:11:12.123456+00:00"}),
        ),
    ]);
}

/// The text of each error is ansible-core 2.19.12's, after `YAML parsing failed: `. Some of it
/// is Python's own: the load stops at the first scalar that cannot be built.
const REFUSED: &[(&str, &str)] = &[
    ("a: !vault 5\n", "The '!vault' tag requires a string value."),
    (
        "a: !vault yes\n",
        "The '!vault' tag requires a string value.",
    ),
    ("a: !vault ~\n", "The '!vault' tag requires a string value."),
    (
        "a: !vault 1.5\n",
        "The '!vault' tag requires a string value.",
    ),
    (
        "a: !vault [1]\n",
        "The '!vault' tag requires a string value.",
    ),
    ("a: !vault\n", "The '!vault' tag requires a string value."),
    (
        "a: !!int abc\n",
        "invalid literal for int() with base 10: 'abc'",
    ),
    (
        "a: !!float abc\n",
        "could not convert string to float: 'abc'",
    ),
    ("a: !!bool 5\n", "'5'"),
    ("a: !!bool maybe\n", "'maybe'"),
    (
        "a: !!timestamp abc\n",
        "'NoneType' object has no attribute 'groupdict'",
    ),
    (
        "a: !!seq 5\n",
        "Expected a sequence node, but found scalar.",
    ),
    ("a: !!map 5\n", "Expected a mapping node, but found scalar."),
    (
        "a: !!merge x\n",
        "Could not determine a constructor for the tag 'tag:yaml.org,2002:merge'.",
    ),
    (
        "a: !!value x\n",
        "Could not determine a constructor for the tag 'tag:yaml.org,2002:value'.",
    ),
    (
        "a: !!yaml x\n",
        "Could not determine a constructor for the tag 'tag:yaml.org,2002:yaml'.",
    ),
    (
        "a: !!python/str x\n",
        "Could not determine a constructor for the tag 'tag:yaml.org,2002:python/str'.",
    ),
    (
        "a: !!python/object:os.system x\n",
        "Could not determine a constructor for the tag 'tag:yaml.org,2002:python/object:os.system'.",
    ),
    (
        "a: !!unknown x\n",
        "Could not determine a constructor for the tag 'tag:yaml.org,2002:unknown'.",
    ),
    (
        "a: !custom x\n",
        "Could not determine a constructor for the tag '!custom'.",
    ),
    (
        "a: !custom {a: 1}\n",
        "Could not determine a constructor for the tag '!custom'.",
    ),
    (
        "a: !custom [1]\n",
        "Could not determine a constructor for the tag '!custom'.",
    ),
    (
        "a: !<tag:example.com,2000:x> y\n",
        "Could not determine a constructor for the tag 'tag:example.com,2000:x'.",
    ),
    ("a: !!int\n", "string index out of range"),
    ("a: !!bool\n", "''"),
    ("a: !!set 5\n", "Expected a mapping node, but found scalar."),
    (
        "a: !!omap 5\n",
        "While constructing an ordered map expected a sequence, but found scalar.",
    ),
    (
        "a: !!pairs 5\n",
        "While constructing pairs expected a sequence, but found scalar.",
    ),
    (
        "a: !!python/dict 5\n",
        "Expected a mapping node, but found scalar.",
    ),
    ("a: !!set x\n", "Expected a mapping node, but found scalar."),
    (
        "a: !!seq ''\n",
        "Expected a sequence node, but found scalar.",
    ),
    (
        "a: !!map ''\n",
        "Expected a mapping node, but found scalar.",
    ),
    ("a: !!int ''\n", "string index out of range"),
    ("a: !!float ''\n", "string index out of range"),
    ("a: !!int 0b\n", "invalid literal for int() with base 2: ''"),
    (
        "a: !!int 0x\n",
        "invalid literal for int() with base 16: ''",
    ),
    (
        "a: !!int 08\n",
        "invalid literal for int() with base 8: '08'",
    ),
    (
        "a: !!int 1:x\n",
        "invalid literal for int() with base 10: 'x'",
    ),
    (
        "a: !!float ABC\n",
        "could not convert string to float: 'abc'",
    ),
    ("a: !!float 1:x\n", "could not convert string to float: 'x'"),
    ("a: !!bool Maybe\n", "'maybe'"),
    ("a: !!bool ''\n", "''"),
    ("a: !!timestamp 2020-13-45\n", "month must be in 1..12"),
    (
        "a: !!timestamp 2020-02-30\n",
        "day is out of range for month",
    ),
    (
        "a: !!timestamp '2020-01-02 25:00:00'\n",
        "hour must be in 0..23",
    ),
    (
        "a: !!timestamp '2020-01-02 10:61:00'\n",
        "minute must be in 0..59",
    ),
    (
        "a: !!timestamp 20-1-2\n",
        "'NoneType' object has no attribute 'groupdict'",
    ),
    (
        "a: !!timestamp '2020-01-02 10:11:12 +24:00'\n",
        "offset must be a timedelta strictly between -timedelta(hours=24) and timedelta(hours=24), not datetime.timedelta(days=1).",
    ),
    ("a: !unsafe |\n  yes\n", "'yes\\n'"),
    (
        "a: !vault \"5\"\n",
        "The '!vault' tag requires a string value.",
    ),
    (
        "a: !vault ''\n",
        "The '!vault' tag requires a string value.",
    ),
    (
        "a: !vault |\n  5\n",
        "The '!vault' tag requires a string value.",
    ),
    ("a: !vault |\n  yes\n", "'yes\\n'"),
];

#[test]
fn a_scalar_pyyaml_cannot_build_stops_the_load_with_its_message() {
    for (text, message) in REFUSED {
        let err = load(text, "t.yml").unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            format!("t.yml: invalid YAML: {message}"),
            "{text:?}"
        );
    }
}

/// The spellings of infinity and not-a-number that PyYAML reads as floats, and the near misses
/// it leaves as strings.
#[test]
fn the_non_finite_floats_load_as_the_reference_loads_them() {
    for (text, want) in [
        (".inf", Some(f64::INFINITY)),
        (".Inf", Some(f64::INFINITY)),
        ("+.inf", Some(f64::INFINITY)),
        ("-.INF", Some(f64::NEG_INFINITY)),
        ("!!float -.nan", Some(f64::NAN)),
        (".NaN", Some(f64::NAN)),
        ("-.nan", None),
        ("inf", None),
        ("nan", None),
    ] {
        let source = format!("x: {text}\n");
        let docs = load(&source, "t.yml").unwrap();
        let node = field(&docs[0], "x").unwrap();
        match want {
            Some(want) => {
                let got = node.as_floating_point().expect(text);
                assert!(got == want || (got.is_nan() && want.is_nan()), "{text}");
            }
            None => assert!(node.as_str().is_some(), "{text}"),
        }
    }
}

#[test]
fn vault_and_unsafe_text_load_as_data() {
    let docs = load(
        "secret: !vault |\n  $ANSIBLE_VAULT;1.1;AES256\n  3132\n",
        "t.yml",
    )
    .unwrap();
    let loaded = to_json(&docs[0]).unwrap();
    let wrapped = loaded["secret"].as_str().unwrap();
    assert_eq!(
        vault_text(wrapped).as_deref(),
        Some("$ANSIBLE_VAULT;1.1;AES256\n3132\n")
    );
    assert_eq!(unsafe_text(wrapped), None);
    let docs = load(
        "raw: !unsafe 'a \"{{ x }}\" \\ b'\nl: [!unsafe '{{ y }}', 3, plain]\nn: !unsafe [{k: '{{ z }}'}, 1]\n",
        "t.yml",
    )
    .unwrap();
    assert_eq!(
        to_json(&docs[0]).unwrap(),
        json!({
            "raw": r#"{{ "a \"{{ x }}\" \\ b" | volant_unsafe }}"#,
            "l": [r#"{{ "{{ y }}" | volant_unsafe }}"#, 3, "plain"],
            "n": [{"k": r#"{{ "{{ z }}" | volant_unsafe }}"#}, 1],
        })
    );
}

#[test]
fn unsafe_text_escapes_and_reads_back_whatever_it_holds() {
    for text in [
        "{{ a }} }} \"q\"",
        "{% if x %}",
        "{# c #}",
        "{{ a }}\\",
        "{{ \u{1f600} }}",
        "{{ a\u{85}b }}\n\r\t",
    ] {
        let wrapped = unsafe_data(Value::String(text.into()));
        assert_eq!(
            unsafe_text(wrapped.as_str().unwrap()).as_deref(),
            Some(text)
        );
    }
    for not_a_wrapper in [
        "{{ \"a\" | volant_unsafe }} {{ b }}",
        "{{ \"a\"b\" | volant_unsafe }}",
        "{{ \"a\\qb\" | volant_unsafe }}",
    ] {
        assert_eq!(unsafe_text(not_a_wrapper), None, "{not_a_wrapper:?}");
    }
}

/// Scalars after multi-byte characters keep the typing their source text gives them.
#[test]
fn scalars_after_multibyte_text_are_typed_from_their_own_source() {
    let docs = load(
        "# é 😀\nnom: «é»\nflag: yes\nmode: 010\nbig: 1_000\n",
        "t.yml",
    )
    .unwrap();
    assert_eq!(
        to_json(&docs[0]).unwrap(),
        json!({"nom": "«é»", "flag": true, "mode": 8, "big": 1000})
    );
}

/// A document of 20000 mappings loads in time that grows with its size. The bound is far above
/// what a linear load takes, and far below what a quadratic one takes.
#[test]
fn a_document_of_twenty_thousand_mappings_loads() {
    let mut text = String::from("---\n");
    for i in 0..20_000 {
        let port = 8000 + i % 1000;
        writeln!(
            text,
            "service_{i}:\n  name: web-{i}\n  port: {port}\n  enabled: yes"
        )
        .unwrap();
        writeln!(text, "  tags: [a, \"b {i}\"]").unwrap();
    }
    let started = std::time::Instant::now();
    let docs = load(&text, "t.yml").unwrap();
    let took = started.elapsed();
    let value = to_json(&docs[0]).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 20_000);
    assert_eq!(value["service_19999"]["enabled"], json!(true));
    assert!(took < std::time::Duration::from_secs(60), "took {took:?}");
}

#[test]
fn syntax_errors_name_the_source() {
    let err = load("a: [1, 2\n", "broken.yml").unwrap_err();
    assert!(format!("{err:#}").contains("broken.yml"));
}

#[test]
fn missing_fields_are_none_not_panics() {
    let docs = load("a: 1\n", "t.yml").unwrap();
    assert!(field(&docs[0], "zzz").is_none());
    assert!(field(&docs[0], "a").is_some());
}

/// A document that nests collections `depth` levels deep, in one of the shapes the reference was
/// measured with. The per-line shapes grow with the square of `depth`, so a table keeps their
/// depth small.
fn nested(shape: &str, depth: usize) -> String {
    let per_line = |item: &dyn Fn(usize) -> String| -> String {
        let mut text = String::new();
        for level in 0..depth {
            text.push_str(&" ".repeat(level));
            text.push_str(&item(level));
            text.push('\n');
        }
        text.push_str(&" ".repeat(depth));
        text.push_str("x\n");
        text
    };
    match shape {
        "block sequence" => format!("{}x\n", "- ".repeat(depth)),
        "block sequence per line" => per_line(&|_| "-".into()),
        "block mapping" => per_line(&|_| "k:".into()),
        "mixed block" => per_line(&|level| if level % 2 == 0 { "-" } else { "k:" }.into()),
        "flow sequence" => format!("{}x{}\n", "[".repeat(depth), "]".repeat(depth)),
        "flow mapping" => format!("{}x{}\n", "{a: ".repeat(depth), "}".repeat(depth)),
        "block then flow" => {
            let flow = depth - depth / 2;
            format!(
                "{}{}x{}\n",
                "- ".repeat(depth / 2),
                "[".repeat(flow),
                "]".repeat(flow)
            )
        }
        other => panic!("no shape {other}"),
    }
}

const SHAPES: [&str; 7] = [
    "block sequence",
    "block sequence per line",
    "block mapping",
    "mixed block",
    "flow sequence",
    "flow mapping",
    "block then flow",
];

/// How many collections deep `value` goes, counted without recursion.
fn json_depth(value: &Value) -> usize {
    let mut deepest = 0;
    let mut stack = vec![(value, 0)];
    while let Some((value, depth)) = stack.pop() {
        match value {
            Value::Array(items) => items.iter().for_each(|v| stack.push((v, depth + 1))),
            Value::Object(map) => map.values().for_each(|v| stack.push((v, depth + 1))),
            _ => deepest = deepest.max(depth),
        }
    }
    deepest
}

/// A document may nest 489 levels, as ansible-core 2.19.12 loads a file; 490 is refused. The
/// scanner stops a flow collection at 255 levels, a limit the reference does not have.
#[test]
fn nesting_deeper_than_the_reference_loads_is_refused_not_a_stack_overflow() {
    for shape in SHAPES {
        // A block document written one level to a line grows with the square of its depth.
        let per_line =
            shape.contains("per line") || matches!(shape, "block mapping" | "mixed block");
        for depth in [200, 255, 256, 313, 314, 315, 488, 489, 490, 1000, 100_000] {
            if per_line && depth > 1000 {
                continue;
            }
            let loads = depth <= 489 && !(shape.starts_with("flow") && depth > 255);
            // A text holding `<<`, even in a comment, goes through the merge-key reader, which
            // must stop at the same limit.
            for text in [
                nested(shape, depth),
                format!("# <<\n{}", nested(shape, depth)),
            ] {
                check_depth(shape, depth, loads, &text);
            }
        }
    }
}

fn check_depth(shape: &str, depth: usize, loads: bool, text: &str) {
    let loaded = load(text, "deep.yml");
    if loads {
        let docs = loaded.unwrap_or_else(|e| panic!("{shape} {depth}: {e:#}"));
        assert_eq!(
            json_depth(&to_json(&docs[0]).unwrap()),
            depth,
            "{shape} {depth}"
        );
    } else {
        let err = loaded
            .err()
            .unwrap_or_else(|| panic!("{shape} {depth} loaded"));
        let err = format!("{err:#}");
        assert!(
            err.contains("deep.yml") && err.contains("recursion"),
            "{shape} {depth}: {err}"
        );
    }
}

/// An alias chain nests as deep as the nodes it names, though the text stays short.
#[test]
fn an_alias_chain_counts_toward_the_nesting_limit() {
    let chain = |levels: usize| {
        let mut text = String::from("a0: &a0 [x]\n");
        for i in 1..levels {
            let _ = writeln!(text, "a{i}: &a{i} [*a{}]", i - 1);
        }
        text
    };
    // The mapping that holds the chain is the first level.
    for (levels, loads) in [(100, true), (488, true), (489, false), (3000, false)] {
        let text = chain(levels);
        let loaded = load(&text, "chain.yml");
        assert_eq!(loaded.is_ok(), loads, "{levels}: {:?}", loaded.err());
        if let Some(err) = loaded.err() {
            assert!(
                format!("{err:#}").contains("maximum recursion depth exceeded"),
                "{err:#}"
            );
        }
    }
}

/// How many scalars `value` holds, counted without recursion.
fn leaf_count(value: &Value) -> usize {
    let (mut leaves, mut stack) = (0, vec![value]);
    while let Some(value) = stack.pop() {
        match value {
            Value::Array(items) => stack.extend(items),
            Value::Object(map) => stack.extend(map.values()),
            _ => leaves += 1,
        }
    }
    leaves
}

/// Each level holds nine aliases of the one before: `levels` of them make a document of a few
/// hundred bytes that stands for `9^levels` nodes.
fn alias_bomb(levels: usize) -> String {
    let mut text = String::from("a0: &a0 [x,x,x,x,x,x,x,x,x]\n");
    for i in 1..levels {
        let aliases = vec![format!("*a{}", i - 1); 9].join(",");
        let _ = writeln!(text, "a{i}: &a{i} [{aliases}]");
    }
    text
}

/// Anchors inside anchors, `levels` deep with `width` scalars at each level. The loader keeps a
/// copy of every anchored node, so the copies add up to about `levels^2 * width / 2` nodes.
fn nested_anchors(levels: usize, width: usize) -> String {
    let row = vec!["x"; width].join(",");
    let mut text = String::new();
    for i in 0..levels {
        let _ = write!(text, "[&a{i} [{row},");
    }
    text.push('x');
    text.push_str(&"]]".repeat(levels));
    text.push('\n');
    text
}

/// Aliases and anchors copy at most 1000000 nodes, so a few hundred bytes cannot stand for
/// gigabytes. A document that copies less loads whole.
#[test]
fn aliases_and_anchors_copy_a_bounded_number_of_nodes() {
    // The 341-byte document that used up 3.6 GB at six levels and was killed at seven.
    let refused = [
        ("billion laughs, 7 levels", alias_bomb(7)),
        ("billion laughs, 9 levels", alias_bomb(9)),
        // Two flow levels to each anchor, so 60 anchors stay under the flow limit of 255.
        ("nested anchors", nested_anchors(60, 1000)),
    ];
    for (what, text) in refused {
        let err = load(&text, "bomb.yml")
            .err()
            .unwrap_or_else(|| panic!("{what} loaded"));
        let err = format!("{err:#}");
        assert!(
            err.contains("bomb.yml") && err.contains("1000000"),
            "{what}: {err}"
        );
    }
    for (what, text, leaves) in [
        (
            "billion laughs, 4 levels",
            alias_bomb(4),
            9 + 81 + 729 + 6561,
        ),
        ("nested anchors", nested_anchors(20, 20), 20 * 20 + 1),
    ] {
        let docs = load(&text, "ok.yml").unwrap_or_else(|e| panic!("{what}: {e:#}"));
        assert_eq!(leaf_count(&to_json(&docs[0]).unwrap()), leaves, "{what}");
    }
}

/// A file that uses aliases the way an inventory does keeps loading, with the values the
/// aliases name.
#[test]
fn a_file_that_reuses_a_few_anchors_loads_whole() {
    let mut text =
        String::from("common: &common {ntp: [a, b, c], dns: {x: 1, y: 2}, tags: &tags [t1, t2]}\n");
    for host in 0..300 {
        let _ = writeln!(text, "h{host}: {{vars: *common, tags: *tags, id: {host}}}");
    }
    let docs = load(&text, "inv.yml").unwrap();
    let json = to_json(&docs[0]).unwrap();
    assert_eq!(json["h299"]["vars"]["dns"], json!({"x": 1, "y": 2}));
    assert_eq!(json["h7"]["tags"], json!(["t1", "t2"]));
}

/// A stream holds one document, as every loader of ansible-core 2.19.12 requires, and the limits
/// count the whole text, so a stream cannot add up to more than one document may.
#[test]
fn a_stream_holds_one_document() {
    for (what, text) in [
        ("two documents", "---\na: 1\n---\nb: 2\n".to_string()),
        ("a trailing marker", "a: 1\n---\n".to_string()),
        ("empty documents", "---\n---\n".to_string()),
        ("many empty documents", "---\n".repeat(100_000)),
        (
            "three alias bombs",
            format!("---\n{0}---\n{0}---\n{0}", alias_bomb(5)),
        ),
    ] {
        let err = load(&text, "stream.yml")
            .err()
            .unwrap_or_else(|| panic!("{what} loaded"));
        let err = format!("{err:#}");
        assert!(
            err.contains("stream.yml") && err.contains("single document"),
            "{what}: {err}"
        );
    }
    for text in [
        "---\na: 1\n",
        "# c\n---\na: 1\n",
        "a: 1\n...\n",
        "---\n",
        "",
    ] {
        assert!(load(text, "one.yml").is_ok(), "{text:?}");
    }
}

/// A scalar that an alias names is copied with its text, so the bytes count too. Ten aliases of
/// a one-megabyte scalar load, and eighty exceed the limit of 64 MiB, in every scalar style.
#[test]
fn aliases_of_a_big_scalar_are_counted_by_their_bytes() {
    let text = "x".repeat(1_000_000);
    let styles = [
        ("plain", format!("a: &a {text}\n")),
        ("single-quoted", format!("a: &a '{text}'\n")),
        ("double-quoted", format!("a: &a \"{text}\\n\"\n")),
        ("literal", format!("a: &a |\n  {text}\n")),
        (
            "multi-line plain",
            format!(
                "a: &a {}y\n",
                format!("{}\n  ", "x".repeat(99)).repeat(10_000)
            ),
        ),
    ];
    for (style, anchor) in styles {
        for (aliases, loads) in [(10, true), (80, false)] {
            let doc = format!("{anchor}b: [{}]\n", vec!["*a"; aliases].join(","));
            let loaded = load(&doc, "big.yml");
            assert_eq!(
                loaded.is_ok(),
                loads,
                "{style} x {aliases}: {:?}",
                loaded.err()
            );
            if let Some(err) = loaded.err() {
                let err = format!("{err:#}");
                assert!(
                    err.contains("big.yml") && err.contains("67108864 bytes"),
                    "{style}: {err}"
                );
            }
        }
    }
}

/// `!unsafe` inside `!unsafe` wraps a string once. Wrapping again at every level doubled the
/// string each time, so a document of a few hundred bytes took gigabytes.
#[test]
fn nested_unsafe_wraps_a_string_once() {
    let once = unsafe_data(Value::String("{{".into()));
    for levels in [1, 2, 3, 32, 254] {
        let text = format!(
            "{}\"{{{{\"{}\n",
            "{a: !unsafe ".repeat(levels),
            "}".repeat(levels)
        );
        let docs = load(&text, "unsafe.yml").unwrap();
        let mut value = to_json(&docs[0]).unwrap();
        for _ in 0..levels {
            value = value["a"].take();
        }
        assert_eq!(value, once, "{levels} levels");
    }
}

/// The deepest document converts on three quarters of a worker's 2 MiB stack in a debug build,
/// for the shapes that cost more than a plain level.
#[test]
fn a_document_at_the_limit_converts_on_a_small_stack() {
    let mut tagged = String::new();
    for level in 0..489 {
        let _ = writeln!(tagged, "{}- !unsafe", " ".repeat(level));
    }
    tagged.push_str(&" ".repeat(489));
    tagged.push_str("x\n");
    let shapes = [
        ("plain", "- ".repeat(489) + "x\n"),
        ("tagged", tagged),
        (
            "mixed",
            "- ".repeat(244) + &"[".repeat(245) + "x" + &"]".repeat(245) + "\n",
        ),
        // Read through the merge-key reader, which builds and plays back a tree of its own.
        ("merge", "# <<\n".to_string() + &"- ".repeat(489) + "x\n"),
        (
            "merge, anchored",
            "# <<\n- &a\n  ".to_string() + &"- ".repeat(488) + "x\n",
        ),
    ];
    for (what, text) in shapes {
        let worker = std::thread::Builder::new()
            .stack_size(1_572_864)
            .spawn(move || {
                let docs = load(&text, "deep.yml").unwrap();
                let json = to_json(&docs[0]).unwrap();
                assert!(json_depth(&json) >= 489, "{what}");
            });
        worker
            .unwrap()
            .join()
            .unwrap_or_else(|_| panic!("{what} failed"));
    }
}

/// Each row is a document and the JSON ansible-core 2.19.12 loads it as, keys in its order: the
/// merged pairs first, an explicit key taking the place of the merged key it overrides.
#[test]
fn a_merge_key_merges_as_pyyaml_does() {
    let rows: &[(&str, &str)] = &[
        (
            "base: &b {a: 1, b: 2}\nd: {<<: *b, c: 3}\n",
            r#"{"base":{"a":1,"b":2},"d":{"a":1,"b":2,"c":3}}"#,
        ),
        (
            "base: &b {a: 1, b: 2}\nd: {c: 3, <<: *b}\n",
            r#"{"base":{"a":1,"b":2},"d":{"a":1,"b":2,"c":3}}"#,
        ),
        (
            "base: &b {a: 1, b: 2}\nd: {a: 9, <<: *b}\n",
            r#"{"base":{"a":1,"b":2},"d":{"a":9,"b":2}}"#,
        ),
        (
            "base: &b {a: 1, b: 2}\nd: {<<: *b, a: 9}\n",
            r#"{"base":{"a":1,"b":2},"d":{"a":9,"b":2}}"#,
        ),
        (
            "x: &x {a: 1, k: x}\ny: &y {a: 2, b: 2, k: y}\nd: {<<: [*x, *y], z: 0}\n",
            r#"{"x":{"a":1,"k":"x"},"y":{"a":2,"b":2,"k":"y"},"d":{"a":1,"b":2,"k":"x","z":0}}"#,
        ),
        (
            "x: &x {a: 1, k: x}\ny: &y {a: 2, b: 2, k: y}\nd: {<<: [*y, *x], z: 0}\n",
            r#"{"x":{"a":1,"k":"x"},"y":{"a":2,"b":2,"k":"y"},"d":{"a":2,"k":"y","b":2,"z":0}}"#,
        ),
        (
            "a: &a {p: 1}\nb: &b {<<: *a, q: 2}\nd: {<<: *b, r: 3}\n",
            r#"{"a":{"p":1},"b":{"p":1,"q":2},"d":{"p":1,"q":2,"r":3}}"#,
        ),
        (
            "x: &x {a: 1}\ny: &y {<<: *x, b: 2}\nd: {<<: [*y], c: 1}\n",
            r#"{"x":{"a":1},"y":{"a":1,"b":2},"d":{"a":1,"b":2,"c":1}}"#,
        ),
        // Two merge keys in one mapping: the later one wins.
        (
            "x: &x {a: 1}\ny: &y {b: 2}\nd:\n  <<: *x\n  <<: *y\n  c: 3\n",
            r#"{"x":{"a":1},"y":{"b":2},"d":{"a":1,"b":2,"c":3}}"#,
        ),
        (
            "x: &x {a: 1, b: 2}\nd: {<<: *x, <<: {b: 3, c: 4}}\n",
            r#"{"x":{"a":1,"b":2},"d":{"a":1,"b":3,"c":4}}"#,
        ),
        // A key written twice keeps its last value, also next to a merge key.
        ("d: {a: 1, a: 2}\n", r#"{"d":{"a":2}}"#),
        (
            "x: &x {a: 1}\nd: {a: 5, a: 6, <<: *x}\n",
            r#"{"x":{"a":1},"d":{"a":6}}"#,
        ),
        // The merge source can be written in place, as a block, or as a tagged mapping.
        (
            "d: {<<: {a: 1, b: 2}, c: 3}\n",
            r#"{"d":{"a":1,"b":2,"c":3}}"#,
        ),
        ("d:\n  <<:\n    a: 1\n  c: 3\n", r#"{"d":{"a":1,"c":3}}"#),
        ("d: {<<: !!map {b: 2}}\n", r#"{"d":{"b":2}}"#),
        (
            "x: &x [{a: 1}, {b: 2}]\nd: {<<: *x, c: 3}\n",
            r#"{"x":[{"a":1},{"b":2}],"d":{"b":2,"a":1,"c":3}}"#,
        ),
        // Nothing to merge, and a merge key at the top of the document.
        ("x: &x {}\nd: {<<: *x}\n", r#"{"x":{},"d":{}}"#),
        ("d: {<<: [], c: 1}\n", r#"{"d":{"c":1}}"#),
        (
            "base: &b {a: 1}\n<<: *b\nz: 1\n",
            r#"{"a":1,"base":{"a":1},"z":1}"#,
        ),
        (
            "- &a {x: 1}\n- {<<: *a, y: 2}\n",
            r#"[{"x":1},{"x":1,"y":2}]"#,
        ),
        // An explicit key, and a tagged one.
        (
            "x: &x {a: 1}\nd:\n  ? <<\n  : *x\n",
            r#"{"x":{"a":1},"d":{"a":1}}"#,
        ),
        (
            "x: &x {a: 1}\nd: {!!merge '<<': *x}\n",
            r#"{"x":{"a":1},"d":{"a":1}}"#,
        ),
        // Quoted, the key is text.
        (
            "x: &x {a: 1}\nd: {'<<': *x}\n",
            r#"{"x":{"a":1},"d":{"<<":{"a":1}}}"#,
        ),
        (
            "x: &x {a: 1}\nd: {\"<<\": *x}\n",
            r#"{"x":{"a":1},"d":{"<<":{"a":1}}}"#,
        ),
        // The characters `<<` elsewhere are no merge key.
        (
            "cmd: |\n  cat <<EOF\n  EOF\nb: {k: 1}\n",
            r#"{"cmd":"cat <<EOF\nEOF\n","b":{"k":1}}"#,
        ),
    ];
    for (text, want) in rows {
        let docs = load(text, "t.yml").unwrap();
        let got = serde_json::to_string(&to_json(&docs[0]).unwrap()).unwrap();
        assert_eq!(&got, want, "{text:?}");
    }
}

/// A merge source marked `!unsafe` stays data in the mapping that merges it: ansible-core 2.19.12
/// does not render `{{ q }}` in `d.a`.
#[test]
fn a_merged_unsafe_mapping_stays_data() {
    let text = "x: &x !unsafe {a: '{{ q }}', n: {m: '{{ q }}'}}\nd: {<<: *x, c: '{{ q }}'}\n";
    let docs = load(text, "t.yml").unwrap();
    let d = to_json(&docs[0]).unwrap()["d"].clone();
    assert_eq!(
        unsafe_text(d["a"].as_str().unwrap()).as_deref(),
        Some("{{ q }}")
    );
    assert_eq!(
        unsafe_text(d["n"]["m"].as_str().unwrap()).as_deref(),
        Some("{{ q }}")
    );
    assert_eq!(d["c"], json!("{{ q }}"));
}

/// A merge key whose value is not a mapping or a list of mappings stops the load with the
/// message of PyYAML's constructor, naming the mapping and the value.
#[test]
fn a_bad_merge_value_is_a_constructor_error() {
    let rows: &[(&str, &str)] = &[
        (
            "d: {<<: 1, c: 2}\n",
            "while constructing a mapping\n  in \"t.yml\", line 1, column 4\nexpected a mapping or list of mappings for merging, but found scalar\n  in \"t.yml\", line 1, column 9",
        ),
        (
            "d:\n  <<: [1]\n",
            "while constructing a mapping\n  in \"t.yml\", line 2, column 3\nexpected a mapping for merging, but found scalar\n  in \"t.yml\", line 2, column 8",
        ),
        (
            "x: &x {a: 1}\nd: {<<: [*x, 2]}\n",
            "while constructing a mapping\n  in \"t.yml\", line 2, column 4\nexpected a mapping for merging, but found scalar\n  in \"t.yml\", line 2, column 14",
        ),
        (
            "d: {<<: [[{a: 1}]]}\n",
            "while constructing a mapping\n  in \"t.yml\", line 1, column 4\nexpected a mapping for merging, but found sequence\n  in \"t.yml\", line 1, column 10",
        ),
        // An alias names the node it points at.
        (
            "a: &s foo\nd: {<<: *s}\n",
            "while constructing a mapping\n  in \"t.yml\", line 2, column 4\nexpected a mapping or list of mappings for merging, but found scalar\n  in \"t.yml\", line 1, column 4",
        ),
        (
            "a: &s\n  - 1\nd: {<<: [*s]}\n",
            "while constructing a mapping\n  in \"t.yml\", line 3, column 4\nexpected a mapping for merging, but found sequence\n  in \"t.yml\", line 1, column 4",
        ),
        // Columns count characters, not bytes.
        (
            "é: 1\na: &s foo\nd: {é: 2, <<: *s}\n",
            "while constructing a mapping\n  in \"t.yml\", line 3, column 4\nexpected a mapping or list of mappings for merging, but found scalar\n  in \"t.yml\", line 2, column 4",
        ),
        (
            "é: 1\nd: {<<: [1, é]}\n",
            "while constructing a mapping\n  in \"t.yml\", line 2, column 4\nexpected a mapping for merging, but found scalar\n  in \"t.yml\", line 2, column 10",
        ),
        // A tag counts as part of the node too.
        (
            "d: {<<: !!str foo}\n",
            "while constructing a mapping\n  in \"t.yml\", line 1, column 4\nexpected a mapping or list of mappings for merging, but found scalar\n  in \"t.yml\", line 1, column 9",
        ),
        (
            "a: !!str &s foo\nd: {<<: *s}\n",
            "while constructing a mapping\n  in \"t.yml\", line 2, column 4\nexpected a mapping or list of mappings for merging, but found scalar\n  in \"t.yml\", line 1, column 4",
        ),
        (
            "<<: 1\n",
            "while constructing a mapping\n  in \"t.yml\", line 1, column 1\nexpected a mapping or list of mappings for merging, but found scalar\n  in \"t.yml\", line 1, column 5",
        ),
    ];
    for (text, want) in rows {
        let err = format!("{:#}", load(text, "t.yml").unwrap_err());
        assert_eq!(err, format!("t.yml: invalid YAML: {want}"), "{text:?}");
    }
}

/// The positions of mappings stay available when a document holds a merge key.
#[test]
fn origins_survive_a_merge_key() {
    let text = "- &a {x: 1}\n- {<<: *a, y: 2}\n";
    let (docs, origins) = load_with_origins(text, "t.yml").unwrap();
    let items = docs[0].as_vec().unwrap();
    let Yaml::Mapping(second) = &items[1] else {
        panic!("a mapping");
    };
    assert_eq!(origins.of(second), Some((2, 3)));
    let value = field(&items[1], "y").unwrap();
    assert_eq!(origins.of_value(value), Some((2, 15)));
}
