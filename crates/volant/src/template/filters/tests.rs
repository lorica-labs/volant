// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use crate::testing::templar;

/// `to_json` and `to_nice_json` write what `json.dumps` writes: ASCII escapes, separators, indent
/// and key order.
#[test]
fn python_json_matches_json_dumps() {
    {
        // `json.dumps` with its default `ensure_ascii`: DEL, a character past ASCII and one
        // outside the basic plane (a surrogate pair), and the escapes below space as serde writes
        // them, which are Python's as well.
        let v = serde_json::json!("a\u{7f}é\u{1F600}\u{1}");
        assert_eq!(
            python_json(&v, None, false, true, 0),
            concat!(
                "\"a", "\\u007f", "\\u00e9", "\\ud83d", "\\ude00", "\\u0001", "\""
            ),
        );
        assert_eq!(
            python_json(&v, None, false, false, 0),
            "\"a\u{7f}é\u{1F600}\\u0001\""
        );
    }
    {
        let v = serde_json::json!({"b": 1, "a": [1, 2]});
        assert_eq!(
            python_json(&v, None, false, false, 0),
            r#"{"b": 1, "a": [1, 2]}"#
        );
        assert_eq!(
            python_json(&v, Some(4), true, false, 0),
            "{\n    \"a\": [\n        1,\n        2\n    ],\n    \"b\": 1\n}"
        );
    }
    {
        use serde_json::json;
        let render = |text: &str| render(text, json!({}));
        assert_eq!(
            render("{{ {'é': 1, 'b': 'ü'} | to_json }}"),
            Ok(json!(r#"{"\u00e9": 1, "b": "\u00fc"}"#))
        );
        assert_eq!(
            render("{{ {'é': 1} | to_nice_json }}"),
            Ok(json!(concat!("{\n    \"", r"\u00e9", "\": 1\n}")))
        );
        assert_eq!(
            render("{{ {'é': 1} | to_json(ensure_ascii=False) }}"),
            Ok(json!(r#"{"é": 1}"#))
        );
        assert_eq!(
            render("{{ {'é': 1} | to_nice_json(ensure_ascii=False) }}"),
            Ok(json!("{\n    \"é\": 1\n}"))
        );
    }
}

fn render(text: &str, vars: serde_json::Value) -> Result<serde_json::Value, String> {
    templar()
        .render(text, vars.as_object().unwrap())
        .map_err(|e| e.0)
}

fn text(text: &str) -> String {
    match render(text, serde_json::json!({})).unwrap() {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    }
}

/// Each line is one the reference printed (ansible-core 2.19.12): the text filters, `comment`, the set
/// filters, `flatten`, `from_yaml`, `quote`, `regex_escape`, `default(none)` and the back references.
#[test]
#[expect(clippy::too_many_lines)]
fn the_filters_print_what_the_reference_printed() {
    {
        assert_eq!(
            text("{{ 'YWJj' | b64decode }} {{ 'abc' | b64encode }}"),
            "abc YWJj"
        );
        assert_eq!(
            text(
                "{{ [3,1,2,1] | difference([2]) }} {{ [3,1,2,1] | intersect([1,3]) }} {{ [3,1] | union([1,4]) }}"
            ),
            "[1, 3] [1, 3] [1, 3, 4]"
        );
        assert_eq!(
            text("{{ [1,[2,[3,[4]]]] | flatten }} {{ [1,[2,[3,[4]]]] | flatten(levels=1) }}"),
            "[1, 2, 3, 4] [1, 2, [3, [4]]]"
        );
        assert_eq!(
            text("{{ \"it's a b\" | quote }} {{ '' | quote }} {{ 'plain' | quote }}"),
            r#"'it'"'"'s a b' '' plain"#
        );
        assert_eq!(text("{{ 'a.b*c[d]' | regex_escape }}"), r"a\.b\*c\[d\]");
        assert_eq!(
            text("{{ 'volant' | to_uuid }}"),
            "69744a58-016b-51b4-91fb-2b5aaa59c628"
        );
        assert_eq!(
            text(
                "{{ 'x' | type_debug }} {{ 1 | type_debug }} {{ [1] | type_debug }} {{ {} | type_debug }} {{ none | type_debug }} {{ true | type_debug }} {{ 1.5 | type_debug }}"
            ),
            "str int list dict NoneType bool float"
        );
    }
    {
        assert_eq!(python_replacement(r"A\1-\g<name>"), "A${1}-${name}");
        assert_eq!(python_replacement("plain"), "plain");
    }
    {
        let vars = serde_json::json!({"managed": "Ansible managed", "two": "two\nlines", "x": "x"});
        let r = |t: &str| render(t, vars.clone()).unwrap();
        assert_eq!(r("{{ managed | comment }}"), "#\n# Ansible managed\n#");
        assert_eq!(r("{{ two | comment('c') }}"), "//\n// two\n// lines\n//");
        assert_eq!(r("{{ x | comment(decoration='; ') }}"), ";\n; x\n;");
        // The block styles, and the parameters the port reads, from `core.py` itself.
        assert_eq!(r("{{ x | comment('cblock') }}"), "/*\n *\n * x\n *\n */");
        assert_eq!(r("{{ x | comment('xml') }}"), "<!--\n -\n - x\n -\n-->");
        assert_eq!(r("{{ x | comment('erlang') }}"), "%\n% x\n%");
        assert_eq!(
            r("{{ x | comment(prefix='', postfix_count=2, beginning='>>', end='<<') }}"),
            ">>\n# x\n#\n#\n<<"
        );
        let err = render("{{ x | comment('pascal') }}", vars).unwrap_err();
        assert!(err.contains("Invalid style 'pascal'."), "{err}");
    }
    {
        let r = |t: &str| render(t, serde_json::json!({})).unwrap();
        assert_eq!(
            r("{{ ['b','a','b'] | union(['c','a']) | sort }}"),
            serde_json::json!(["a", "b", "c"])
        );
        assert_eq!(
            r("{{ ['b','a'] | difference(['a']) }}"),
            serde_json::json!(["b"])
        );
        // A list of lists is not hashable, and the reference falls back to the input's order.
        assert_eq!(
            r("{{ [[1], [2]] | intersect([[2]]) }}"),
            serde_json::json!([[2]])
        );
    }
    {
        let r = |t: &str| render(t, serde_json::json!({})).unwrap();
        assert_eq!(
            r("{{ [1, none, 'None', 'null', [2, none]] | flatten }}"),
            serde_json::json!([1, 2])
        );
        assert_eq!(
            r("{{ [1, none, [2]] | flatten(skip_nulls=false) }}"),
            serde_json::json!([1, null, 2])
        );
        assert_eq!(
            r("{{ [1, [2]] | flatten(levels=0) }}"),
            serde_json::json!([1, [2]])
        );
    }
    {
        let vars = serde_json::json!({"doc": "a: 1\nb: [x, y]", "bad": "a: [1"});
        assert_eq!(
            render("{{ doc | from_yaml }}", vars.clone()).unwrap(),
            serde_json::json!({"a": 1, "b": ["x", "y"]})
        );
        assert!(
            render("{{ bad | from_yaml }}", vars)
                .unwrap_err()
                .contains("from_yaml: invalid YAML"),
        );
        assert_eq!(
            render("{{ none | from_yaml }}", serde_json::json!({})).unwrap(),
            serde_json::Value::Null
        );
    }
    {
        assert_eq!(
            text("{{ 'a@b%c+d=e:f,g.h/i-j_k' | quote }}"),
            "a@b%c+d=e:f,g.h/i-j_k"
        );
        assert_eq!(text("{{ none | quote }}"), "''");
        assert_eq!(text("{{ 'é' | quote }}"), "'é'");
        assert_eq!(text("{{ 'a b-c_d' | regex_escape }}"), r"a\ b\-c_d");
        assert_eq!(
            text("{{ 'a.b[c]^$*+' | regex_escape('posix_basic') }}"),
            r"a\.b\[c\]\^\$\*+"
        );
        assert!(
            render(
                "{{ 'x' | regex_escape('posix_extended') }}",
                serde_json::json!({})
            )
            .unwrap_err()
            .contains("Regex type (posix_extended) not yet implemented")
        );
    }
    {
        use serde_json::json;
        let render = |text: &str| render(text, json!({}));
        assert_eq!(
            render("{{ (nope | default(none)) is none }}"),
            Ok(json!(true))
        );
        assert_eq!(render("{{ nope | default(none) }}"), Ok(json!(null)));
        assert_eq!(render("{{ nope | default }}"), Ok(json!("")));
        assert_eq!(render("{{ '' | default('x', true) }}"), Ok(json!("x")));
        assert_eq!(render("{{ '' | default('x') }}"), Ok(json!("")));
    }
}

/// `b64decode`, `to_uuid` and an unknown or wrongly aliased filter fail naming what was wrong.
#[test]
fn a_bad_filter_call_fails_naming_what_was_wrong() {
    {
        let err = render("{{ 'not base64!' | b64decode }}", serde_json::json!({})).unwrap_err();
        assert!(err.contains("b64decode"), "{err}");
        // Whitespace is skipped, as `base64.b64decode` does without `validate=True`.
        assert_eq!(text("{{ 'YWJj ' | b64decode }}"), "abc");
        assert_eq!(text("{{ ' YW\tJj\n' | b64decode }}"), "abc");
        assert_eq!(text("{{ 'YWI_Pg==' | b64decode(urlsafe=true) }}"), "ab?>");
        assert_eq!(text("{{ 'ab?>' | b64encode(urlsafe=true) }}"), "YWI_Pg==");
        let err = render(
            "{{ 'x' | to_uuid(namespace='bogus') }}",
            serde_json::json!({}),
        )
        .unwrap_err();
        assert!(
            err.contains("Invalid value 'bogus' for 'namespace'"),
            "{err}"
        );
        // The namespace in any spelling `uuid.UUID` reads: braces, `urn:uuid:`, no hyphens.
        assert_eq!(
            text("{{ 'volant' | to_uuid(namespace='{361e6d51faec444a9079341386da8e2e}') }}"),
            "69744a58-016b-51b4-91fb-2b5aaa59c628"
        );
    }
    {
        let t = templar();
        let err = t
            .render("{{ 1 | some_unknown_filter }}", &serde_json::Map::new())
            .unwrap_err();
        assert!(err.0.contains("some_unknown_filter"), "{err}");
    }
    {
        let err = render("{{ 'x' | ansible.builtin.ipwrap }}", serde_json::json!({})).unwrap_err();
        assert!(err.contains("unknown filter"), "{err}");
    }
}

/// `ipwrap` brackets an IPv6 address before its prefix and leaves anything else, or a prefix that
/// does not parse, alone.
#[test]
fn ipwrap_brackets_ipv6_and_nothing_else() {
    {
        assert_eq!(
            render(
                "{{ ['192.0.2.1', '2001:db8::1', 'example.org', '192.0.2.0/24', '2001:db8::/64', ''] | ansible.utils.ipwrap }}",
                serde_json::json!({})
            )
            .unwrap(),
            serde_json::json!([
                "192.0.2.1",
                "[2001:db8::1]",
                "example.org",
                "192.0.2.0/24",
                "[2001:db8::]/64",
                ""
            ])
        );
        assert_eq!(
            text("{{ '2001:db8::1' | ansible.utils.ipwrap }}"),
            "[2001:db8::1]"
        );
        assert_eq!(text("{{ 'host1' | ansible.utils.ipwrap }}"), "host1");
        assert_eq!(
            text("{{ '192.0.2.1' | ansible.utils.ipwrap }}"),
            "192.0.2.1"
        );
        let err = render("{{ 42 | ansible.utils.ipwrap }}", serde_json::json!({})).unwrap_err();
        assert!(
            err.contains(
                "The filter plugin 'ansible.utils.ipwrap' failed: Unrecognized type <<class 'int'>> for ipwrap filter <value>"
            ),
            "{err}"
        );
    }
    {
        assert_eq!(
            text("{{ '2001:db8::/129' | ansible.utils.ipwrap }}"),
            "2001:db8::/129"
        );
        assert_eq!(
            text("{{ '2001:db8::/abc' | ansible.utils.ipwrap }}"),
            "2001:db8::/abc"
        );
        // 128 itself, the last valid one, still wraps.
        assert_eq!(
            text("{{ '2001:db8::/128' | ansible.utils.ipwrap }}"),
            "[2001:db8::]/128"
        );
    }
}

/// One expression per name the k3s-ansible roles call, as the reference answered, and the
/// `ansible.builtin` alias of a filter and a test.
#[test]
#[expect(clippy::too_many_lines)]
fn every_name_k3s_ansible_calls_answers_under_its_alias_too() {
    {
        use serde_json::json;
        let rows = [
            (
                "{{ '2001:db8::1' | ansible.utils.ipwrap }}",
                json!("[2001:db8::1]"),
            ),
            ("{{ 'YWJj' | b64decode }}", json!("abc")),
            ("{{ '/a/b/c.tar' | basename }}", json!("c.tar")),
            ("{{ 'yes' | bool }}", json!(true)),
            (
                "{{ {'a': 1} | combine({'b': 2}) }}",
                json!({"a": 1, "b": 2}),
            ),
            ("{{ missing | default('d') }}", json!("d")),
            ("{{ ['x', 'y'] | first }}", json!("x")),
            ("{{ [[1], [1, 2]] | flatten }}", json!([1, 1, 2])),
            ("{{ 'a: 1' | from_yaml }}", json!({"a": 1})),
            ("{{ [1, 2] | length }}", json!(2)),
            ("{{ 'ab' | list }}", json!(["a", "b"])),
            ("{{ [{'a': 1}] | map(attribute='a') | list }}", json!([1])),
            ("{{ '10.0.0.1' | regex_escape }}", json!(r"10\.0\.0\.1")),
            ("{{ 'abc' | regex_replace('b', '') }}", json!("ac")),
            (
                "{{ 'k3s version v1.2.3' | regex_search('v[0-9.]+') }}",
                json!("v1.2.3"),
            ),
            (
                "{{ [{'s': {'e': true}, 'i': 1}, {'s': {'e': false}, 'i': 2}] | selectattr('s.e') | map(attribute='i') | list }}",
                json!([1]),
            ),
            (
                "{{ [{'d': 1}, {}] | selectattr('d', 'defined') | map(attribute='d') | list }}",
                json!([1]),
            ),
            ("{{ 'a,b' | split(',') }}", json!(["a", "b"])),
            ("{{ 1 | string }}", json!("1")),
            ("{{ true | ternary('t', 'f') }}", json!("t")),
            ("{{ {'a': 1} | to_nice_yaml }}", json!("a: 1\n")),
            ("{{ ' x ' | trim }}", json!("x")),
            ("{{ [1, 1, 2] | unique | list }}", json!([1, 2])),
            (
                "{{ ['a', 'b'] | map('extract', {'a': 1, 'b': 2}) | list }}",
                json!([1, 2]),
            ),
            (
                "{{ 'k3s version v1.30.2+k3s1 (abc)'.split(' ')[2] }}",
                json!("v1.30.2+k3s1"),
            ),
            ("{{ 'a b  c'.split() | length }}", json!(3)),
            ("{{ missing is defined }}", json!(false)),
            ("{{ false is false }}", json!(true)),
            ("{{ 'Archlinux' is match('Arch') }}", json!(true)),
            ("{{ 'aarch64' is search('arch') }}", json!(true)),
            ("{{ missing is undefined }}", json!(true)),
            ("{{ '2.19.12' is version('2.15', '>=') }}", json!(true)),
            (
                "{{ '2.19.12' is version_compare('2.15', '>=') }}",
                json!(true),
            ),
            (
                "{{ 'v1.30.2+k3s1' is version('v1.31.0+k3s1', '<') }}",
                json!(true),
            ),
            (
                "{{ 'v1.31.0+k3s1' is version('v1.31.0+k3s1', '<=') }}",
                json!(true),
            ),
            (
                "{{ '6.8.0-45-generic' is version('6.18', '>=') }}",
                json!(false),
            ),
            ("{{ '1.8.7' is version('1.8.5', '<') }}", json!(false)),
        ];
        let failed: Vec<String> = rows
            .iter()
            .filter_map(|(expr, want)| match render(expr, json!({})) {
                Ok(got) if &got == want => None,
                other => Some(format!("{expr} -> {other:?}")),
            })
            .collect();
        assert!(failed.is_empty(), "{failed:#?}");
    }
    {
        assert_eq!(
            render(
                "{{ missing | ansible.builtin.default('y') }}",
                serde_json::json!({})
            )
            .unwrap(),
            serde_json::json!("y")
        );
        assert_eq!(
            render(
                "{{ 'a b' is ansible.builtin.match('a') }}",
                serde_json::json!({})
            )
            .unwrap(),
            serde_json::json!(true)
        );
    }
}

/// `extract`'s vectors, each produced by ansible-core 2.19.12 on localhost.
#[test]
fn extract_reads_container_item_then_each_more_key() {
    use serde_json::json;
    let r = |t: &str| render(t, json!({}));
    assert_eq!(
        r("{{ 'a' | extract({'a': {'x': {'y': 5} } }, ['x', 'y']) }}"),
        Ok(json!(5))
    );
    assert_eq!(
        r("{{ 'a' | extract({'a': {'xy': 7} }, 'xy') }}"),
        Ok(json!(7))
    );
    assert_eq!(r("{{ 'a' | extract({'a': 7}, none) }}"), Ok(json!(7)));
    assert_eq!(
        r("{{ [0, 2] | map('extract', ['p', 'q', 'r']) | list }}"),
        Ok(json!(["p", "r"]))
    );
    assert_eq!(
        r("{{ 'z' | extract({'a': 1}) | default('none') }}"),
        Ok(json!("none"))
    );
    assert_eq!(
        r("{{ 'z' | extract({'a': 1}, ['x', 'y']) | default('none') }}"),
        Ok(json!("none"))
    );
    assert_eq!(
        r("{{ 'a' | extract({'a': {'x': 1} }, ['nope', 'y']) | default('none') }}"),
        Ok(json!("none"))
    );
    assert_eq!(
        r("{{ 'z' | extract({'a': 1}) is defined }}"),
        Ok(json!(false))
    );
    // The reference: `object of type 'dict' has no attribute 'z'`, as an undefined variable.
    let err = r("{{ 'z' | extract({'a': 1}) }}").unwrap_err();
    assert!(err.starts_with(super::super::UNDEFINED), "{err}");
    // An undefined `morekeys` is a key that cannot be read, not an omitted argument: the
    // reference fails `'missing' is undefined` bare, and `default` catches it.
    let err = r("{{ 'a' | extract({'a': 1}, missing) }}").unwrap_err();
    assert!(err.starts_with(super::super::UNDEFINED), "{err}");
    assert_eq!(
        r("{{ 'a' | extract({'a': 1}, missing) | default('d') }}"),
        Ok(json!("d"))
    );
    // The reference: `extract() takes from 3 to 4 positional arguments but 5 were given`.
    let err = r("{{ 'a' | extract({'a': 1}, 'x', 'y') }}").unwrap_err();
    assert!(err.contains("too many arguments"), "{err}");
    assert_eq!(
        r("{{ 'a' | ansible.builtin.extract({'a': 3}) }}"),
        Ok(json!(3))
    );
}
