// SPDX-License-Identifier: GPL-3.0-or-later
use crate::testing::templar;
use serde_json::{Map, Value, json};

fn dump(filter: &str, value: Value) -> String {
    let mut vars = Map::new();
    vars.insert("v".into(), value);
    match templar()
        .render(&format!("{{{{ v | {filter} }}}}"), &vars)
        .unwrap()
    {
        Value::String(s) => s,
        other => panic!("not a string: {other}"),
    }
}

/// The mapping the reference dumped, written here in an order that is not sorted.
fn nested() -> Value {
    json!({
        "u": "é",
        "t": true,
        "s": "1",
        "q": "a: b",
        "m": "line1\nline2",
        "f": 1.5,
        "e": "",
        "b": [1, "two", {"c": null}],
        "a": "yes",
    })
}

/// `to_nice_yaml` prints ansible-core 2.19.12's exact bytes: sorted keys, indent 4, unindented
/// list dashes and its quoting rules.
#[test]
fn to_nice_yaml_prints_the_reference_s_bytes() {
    assert_eq!(
        dump("to_nice_yaml", nested()),
        "a: 'yes'\nb:\n- 1\n- two\n-   c: null\ne: ''\nf: 1.5\nm: 'line1\n\n    line2'\nq: 'a: b'\ns: '1'\nt: true\nu: é\n"
    );
    assert_eq!(
        dump("to_nice_yaml(indent=2)", nested()),
        "a: 'yes'\nb:\n- 1\n- two\n- c: null\ne: ''\nf: 1.5\nm: 'line1\n\n  line2'\nq: 'a: b'\ns: '1'\nt: true\nu: é\n"
    );
}

/// One string per rule that stops PyYAML writing a string plain.
#[test]
fn a_string_is_quoted_exactly_when_pyyaml_quotes_it() {
    let cases: &[(&str, &str)] = &[
        // YAML 1.1 would read these as a boolean, an integer, a float, null, a timestamp,
        // a merge key or a value key.
        ("On", "'On'"),
        ("NO", "'NO'"),
        ("y", "y"),
        ("0x1F", "'0x1F'"),
        ("017", "'017'"),
        ("08", "08"),
        ("1_000", "'1_000'"),
        ("1:30", "'1:30'"),
        ("-1", "'-1'"),
        (".5", "'.5'"),
        ("1.0e+3", "'1.0e+3'"),
        ("1e3", "1e3"),
        (".inf", "'.inf'"),
        ("~", "'~'"),
        ("Null", "'Null'"),
        ("2001-12-14", "'2001-12-14'"),
        ("<<", "'<<'"),
        ("=", "'='"),
        // Indicators: `: ` and ` #` inside, one of the leading indicators, a document mark.
        ("a:b", "a:b"),
        ("a #b", "'a #b'"),
        ("a#b", "a#b"),
        ("- x", "'- x'"),
        ("-x", "-x"),
        ("?x", "?x"),
        ("*x", "'*x'"),
        ("!x", "'!x'"),
        ("%x", "'%x'"),
        ("@x", "'@x'"),
        ("[x", "'[x'"),
        ("{x", "'{x'"),
        ("x,y", "x,y"),
        ("'x", "'''x'"),
        ("\"x", "'\"x'"),
        ("---x", "'---x'"),
        // Leading and trailing space, a tab, a space before a newline, a final newline.
        (" x", "' x'"),
        ("x ", "'x '"),
        ("a\tb", "\"a\\tb\""),
        ("a \nb", "\"a \\nb\""),
        ("a\n", "'a\n\n    '"),
        ("it's", "it's"),
    ];
    for (input, want) in cases {
        assert_eq!(
            dump("to_nice_yaml", json!({ "k": input })),
            format!("k: {want}\n"),
            "{input:?}"
        );
    }
}

/// libyaml folds a plain scalar at a single space once the line has passed 80 columns, and
/// `width` moves that limit; checked against PyYAML 6.0.3's `CSafeDumper`.
#[test]
fn a_long_line_folds_at_the_width_pyyaml_uses() {
    let words = ["aaaa"; 20].join(" ");
    assert_eq!(
        dump("to_nice_yaml", json!({ "k": words })),
        format!(
            "k: {}\n    {}\n",
            ["aaaa"; 16].join(" "),
            ["aaaa"; 4].join(" ")
        )
    );
    assert_eq!(
        dump("to_nice_yaml(width=1000)", json!({ "k": words })),
        format!("k: {words}\n")
    );
}

/// `to_yaml` keeps PyYAML's default flow style: a collection that holds only scalars is written
/// inline, the others as blocks, with an indent of 2.
#[test]
fn to_yaml_writes_a_collection_of_scalars_inline() {
    assert_eq!(
        dump(
            "to_yaml",
            json!({"b": [1, "two"], "a": 1, "c": {"d": "x", "e": [[1], []]}})
        ),
        "a: 1\nb: [1, two]\nc:\n  d: x\n  e:\n  - [1]\n  - []\n"
    );
    assert_eq!(
        dump("to_yaml", json!({"a": "yes", "b": 1})),
        "{a: 'yes', b: 1}\n"
    );
    assert_eq!(dump("to_yaml", json!("abc")), "abc\n");
    assert_eq!(dump("to_yaml", json!(8080)), "8080\n");
    assert_eq!(dump("to_yaml", json!(null)), "null\n");
    assert_eq!(dump("to_yaml", json!("yes")), "'yes'\n");
    assert_eq!(dump("to_yaml", json!([])), "[]\n");
    assert_eq!(dump("to_nice_yaml", json!({})), "{}\n");
    assert_eq!(
        dump("to_nice_yaml", json!([[1, 2], {"a": []}])),
        "-   - 1\n    - 2\n-   a: []\n"
    );
    assert_eq!(
        dump("to_nice_yaml", json!({"k": 1e17, "l": -0.0, "m": 1e-5})),
        "k: 1.0e+17\nl: -0.0\nm: 1.0e-05\n"
    );
}

fn render(text: &str, vars: Value) -> String {
    match templar().render(text, vars.as_object().unwrap()).unwrap() {
        Value::String(s) => s,
        other => panic!("not a string: {other}"),
    }
}

/// Where libyaml's emitter and `emitter.py` part, ansible-core's output is libyaml's, and so is
/// this. Each vector was printed by PyYAML 6.0.3's `CSafeDumper`.
#[test]
fn where_libyaml_and_emitter_py_differ_this_is_libyaml() {
    assert_eq!(
        dump(
            "to_nice_yaml",
            json!({"k": "\u{1F600}\tx", "l": "\u{1F600}"})
        ),
        "k: \"\\U0001F600\\tx\"\nl: \"\\U0001F600\"\n"
    );
    assert_eq!(
        dump(
            "to_nice_yaml",
            json!({"k": ["\u{85}x", "\u{feff}x", "a\u{2028}b", "a\rb", "\u{a0}x"]})
        ),
        "k:\n- \"\\Nx\"\n- \"\\uFEFFx\"\n- 'a\u{2028}    b'\n- \"a\\rb\"\n- \u{a0}x\n"
    );
    assert_eq!(
        dump("to_nice_yaml", json!({"k": "abc\t".repeat(30)})),
        format!("k: \"{}\"\n", "abc\\t".repeat(30))
    );
    assert_eq!(
        dump("to_nice_yaml", json!({"k": "a\tb ".repeat(30)})),
        format!(
            "k: \"{}a\\tb\n    {} \"\n",
            "a\\tb ".repeat(15),
            ["a\\tb"; 14].join(" ")
        )
    );
    assert_eq!(
        dump("to_nice_yaml", json!({"k": "a\tb  ".repeat(20)})),
        format!(
            "k: \"{}a\\tb\n    \\ {}\"\n",
            "a\\tb  ".repeat(13),
            "a\\tb  ".repeat(6)
        )
    );
    assert_eq!(dump("to_nice_yaml", json!({"": 1})), "'': 1\n");
    let key = "k".repeat(128);
    assert_eq!(
        dump(
            "to_nice_yaml",
            Value::Object(Map::from_iter([(key.clone(), json!(1))]))
        ),
        format!("{key}: 1\n")
    );
    let key = "k".repeat(129);
    assert_eq!(
        dump(
            "to_nice_yaml",
            Value::Object(Map::from_iter([(key.clone(), json!(1))]))
        ),
        format!("? {key}\n: 1\n")
    );
    assert_eq!(
        dump("to_nice_yaml", json!({"a\nb": 1})),
        "? 'a\n\n    b'\n: 1\n"
    );
    assert_eq!(
        dump("to_nice_yaml", json!({"k": "a\n\nb\n", "j": "x\ny"})),
        "j: 'x\n\n    y'\nk: 'a\n\n\n    b\n\n    '\n"
    );
    assert_eq!(
        dump("to_yaml", json!({"": 1, "a": "x y"})),
        "{'': 1, a: x y}\n"
    );
}

/// The value is dumped as it is, not through JSON, which has no infinity, no NaN and no
/// integer keys: PyYAML writes `.inf`, `-.inf`, `.nan`, and sorts integer keys as numbers.
#[test]
fn infinities_nan_and_integer_keys_are_pyyaml_s() {
    assert_eq!(
        render(
            "{{ {'k': 'inf' | float, 'l': '-inf' | float, 'm': 'nan' | float} | to_nice_yaml }}",
            json!({})
        ),
        "k: .inf\nl: -.inf\nm: .nan\n"
    );
    assert_eq!(render("{{ 'inf' | float | to_yaml }}", json!({})), ".inf\n");
    assert_eq!(
        render("{{ {1: 'a', 10: 'b', 9: 'c'} | to_nice_yaml }}", json!({})),
        "1: a\n9: c\n10: b\n"
    );
}
