// SPDX-License-Identifier: GPL-3.0-or-later
use minijinja::{Environment, Value};
use serde_json::{Map, Value as Json, json};

use super::mutable::{identity, is_literal, mutable, plain_list, plain_map, snapshot};

/// The mutable objects stand in for the plain lists and mappings minijinja builds, found by
/// their Rust type. An upgrade that changes those types fails here, not in a template.
#[test]
fn plain_lists_and_mappings_are_told_from_other_objects() {
    assert!(plain_list(&crate::template::bigint::to_template(&json!([1, 2]))).is_some());
    assert!(plain_map(&crate::template::bigint::to_template(&json!({"a": 1}))).is_some());
    assert!(plain_list(&Value::from(vec![Value::from(1)])).is_some());
    let namespace = Environment::new()
        .compile_expression("namespace()")
        .and_then(|e| e.eval(()))
        .unwrap();
    assert!(mutable(&namespace).is_none());
    assert!(identity(&namespace).is_none());
    assert!(mutable(&Value::from("text")).is_none());
}

#[test]
fn one_plain_value_has_one_identity_and_a_copy_has_another() {
    let list = crate::template::bigint::to_template(&json!([1, 2]));
    let same = list.clone();
    let other = crate::template::bigint::to_template(&json!([1, 2]));
    assert_eq!(identity(&list), identity(&same));
    assert_ne!(identity(&list), identity(&other));
}

#[test]
fn a_snapshot_holds_no_mutable_object() {
    let made = mutable(&crate::template::bigint::to_template(
        &json!({"a": [1], "b": {"c": []}}),
    ))
    .unwrap();
    let inner = made.get_attr("a").unwrap();
    assert!(plain_list(&inner).is_none());
    let copy = snapshot(&made);
    assert!(plain_map(&copy).is_some());
    assert!(plain_list(&copy.get_attr("a").unwrap()).is_some());
    assert_eq!(
        crate::template::bigint::to_json(&copy).unwrap(),
        json!({"a": [1], "b": {"c": []}})
    );
}

#[test]
fn only_a_literal_that_json_carries_back_is_routed() {
    assert!(is_literal(&crate::template::bigint::to_template(
        &json!([1, "a", null, [true], {"k": 1.5}])
    )));
    let int_key = Value::from_pairs([(Value::from(1), Value::from("a"))]);
    assert!(!is_literal(&int_key));
}

/// Runs `f` on its own thread and fails when it has not finished in `secs`, so that a hang shows
/// as a failure and not as a run that never ends.
fn within<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(std::time::Duration::from_secs(secs))
        .expect("the template did not finish")
}

fn render(template: &'static str) -> Result<serde_json::Value, String> {
    within(20, move || {
        crate::testing::templar()
            .render(template, &Map::new())
            .map_err(|e| e.0)
    })
}

/// A list or mapping that is handed to its own method (as the item to remove, or as a key) is
/// compared and hashed while the method holds it. That used to wait on itself for ever.
#[test]
fn a_container_given_to_its_own_method_does_not_wait_on_itself() {
    let err = render("{% set l = [[1]] %}{{ l.remove(l) }}").unwrap_err();
    assert!(err.contains("not in list"), "{err}");
    for template in [
        "{% set d = {} %}{{ d.setdefault(d, 1) }}",
        "{% set d = {} %}{{ d.update([[d, 1]]) }}",
        "{% set d = {} %}{{ d.get(d) }}",
    ] {
        let err = render(template).unwrap_err();
        assert!(err.contains("unhashable type: 'dict'"), "{template}: {err}");
    }
    assert_eq!(
        render(">{% set l = [[1]] %}{% set _ = l.extend(l) %}{% set _ = l[0].append(2) %}{{ l }}"),
        Ok(json!(">[[1, 2], [1]]"))
    );
}

/// A copy of a list that holds copies of it shares them as a graph: twenty-eight rounds used to
/// take nine seconds, because each copy walked the structure as a tree.
#[test]
fn copying_a_list_that_holds_copies_of_itself_is_linear() {
    let out = render(
        "{% set l = [1] %}{% for i in range(60) %}{% set _ = l.append(l.copy()) %}{% endfor %}{{ l | length }}",
    );
    assert_eq!(out, Ok(json!(61)));
}

/// A list of mixed types is no total order: sorting it is an error and never a panic, for any
/// length and order of the items.
#[test]
fn sorting_mixed_types_fails_without_a_panic() {
    for template in [
        "{% set l = [3, 'a', 2, 'b', 1, 'c', 0, 'd', 9, 'e', 8, 'f', 7, 'g', 6, 'h', 5] %}{{ l.sort() }}",
        "{% set l = ['b', 1, 'a'] %}{{ l.sort() }}",
        "{% set l = [[1, 'a'], [1, 2], [0, 'b'], [1, 3]] %}{{ l.sort() }}",
    ] {
        let out = render(template);
        assert!(out.is_err(), "{template}: {out:?}");
    }
}

#[test]
fn a_reverse_sort_is_stable_and_orders_the_items() {
    assert_eq!(
        render(
            "{% set l = [[1, 'a'], [2, 'b'], [1, 'c']] %}{% set _ = l.sort(reverse=True) %}{{ l }}"
        ),
        Ok(json!([[2, "b"], [1, "c"], [1, "a"]]))
    );
}

/// `l.extend(l)` in a loop doubles a list; it stops at the limit and does not run out of memory.
#[test]
fn a_list_stops_growing_at_the_limit() {
    let out =
        render("{% set l = [1] %}{% for i in range(60) %}{% set _ = l.extend(l) %}{% endfor %}");
    assert!(out.is_err(), "{out:?}");
}

/// A width, a precision or a `*` of `%` that a host supplies is bounded like the other methods:
/// a task error before any text is built, with Python's own words for a count past an index.
#[test]
fn a_percent_width_or_precision_past_the_limit_is_a_task_error() {
    let v = vars(json!({"w": 10_000_000_000_i64, "p": 2_000_000_000, "f": "%.70000f"}));
    for template in [
        "{{ '%10000000000d' % 1 }}",
        "{{ '%0100000000000d' % 1 }}",
        "{{ '%*d' % (w, 1) }}",
        "{{ '%.*f' % (p, 1.5) }}",
        "{{ '%.10000000000d' % 1 }}",
        "{{ '%-10000000000s' | format('a') }}",
        "{{ f.replace('7', '99999999999') % 1.5 }}",
    ] {
        let err = crate::testing::templar().render(template, &v).unwrap_err();
        assert!(err.0.contains("256 MiB"), "{template}: {err}");
    }
    for (template, want) in [
        ("{{ '%18446744073709551615d' % 1 }}", "width too big"),
        ("{{ '%.18446744073709551615f' % 1.5 }}", "precision too big"),
        ("{{ '%*d' % (100000000000000000000, 1) }}", "C ssize_t"),
    ] {
        let err = crate::testing::templar()
            .render(template, &Map::new())
            .unwrap_err();
        assert!(err.0.contains(want), "{template}: {err}");
    }
    assert_eq!(
        crate::testing::templar()
            .render("{{ (f % 1.5) | length }}", &v)
            .unwrap(),
        json!(70002)
    );
}

/// An integer past 64 bits is an integer to `%` and to the `format` filter.
#[test]
fn percent_reads_integers_past_64_bits() {
    let t = crate::testing::templar();
    let none = Map::new();
    for (template, want) in [
        ("{{ '%d' % 18446744073709551615 }}", "18446744073709551615"),
        (
            "{{ '%x' | format(18446744073709551615) }}",
            "ffffffffffffffff",
        ),
        (
            "{{ '%d' % (0 - 18446744073709551615) }}",
            "-18446744073709551615",
        ),
        (
            "{{ '%f' % 18446744073709551615 }}",
            "18446744073709551616.000000",
        ),
        (
            "{{ '%d' % (1099511627776 * 1099511627776) }}",
            "1208925819614629174706176",
        ),
    ] {
        assert_eq!(
            t.render(template, &none).unwrap(),
            json!(want),
            "{template}"
        );
    }
}

fn vars(v: Json) -> Map<String, Json> {
    v.as_object().unwrap().clone()
}

/// A large integer is ordered against a float by value, as Python does, and not after it was
/// rounded to a float: `2**53 + 1` is larger than `2.0**53`.
#[test]
fn sorting_a_large_integer_against_a_float_is_exact() {
    for (template, want) in [
        (
            "{% set l = [9007199254740993, 9007199254740992.0] %}{% set _ = l.sort() %}{{ l | map('string') | join(',') }}",
            "9007199254740992.0,9007199254740993",
        ),
        (
            "{% set l = [9007199254740992.0, 9007199254740993] %}{% set _ = l.sort(reverse=True) %}{{ l | map('string') | join(',') }}",
            "9007199254740993,9007199254740992.0",
        ),
    ] {
        assert_eq!(render(template), Ok(json!(want)), "{template}");
    }
}

/// `inf` is padded with blanks, and the fields of one format string together stay within the limit.
#[test]
fn percent_pads_infinity_with_blanks_and_bounds_the_whole_text() {
    let v = vars(json!({"big": 1e308}));
    let t = crate::testing::templar();
    assert_eq!(
        t.render("{{ '%08f|%+08.1f' % (big * 10, big * 10) }}", &v)
            .unwrap(),
        json!("     inf|    +inf")
    );
    let err = t
        .render("{{ '%200000000s%200000000s' % ('a', 'b') }}", &v)
        .unwrap_err();
    assert!(err.0.contains("256 MiB"), "{err}");
    let err = t
        .render("{{ '%.*f' % (2147483648, 1.5) }}", &v)
        .unwrap_err();
    assert!(err.0.contains("C int"), "{err}");
}

/// `dict.fromkeys` and `{}.fromkeys` refuse a call in CPython's words. The golden rows only
/// record that the reference failed; these are the texts it printed.
#[test]
fn fromkeys_refuses_in_the_words_of_python() {
    for (template, want) in [
        ("{{ dict.fromkeys([[1]]) }}", "unhashable type: 'list'"),
        ("{{ dict.fromkeys([{'a': 1}]) }}", "unhashable type: 'dict'"),
        ("{{ dict.fromkeys(5) }}", "'int' object is not iterable"),
        (
            "{{ {}.fromkeys(none) }}",
            "'NoneType' object is not iterable",
        ),
        (
            "{{ dict.fromkeys() }}",
            "fromkeys expected at least 1 argument, got 0",
        ),
        (
            "{{ dict.fromkeys(1, 2, 3) }}",
            "fromkeys expected at most 2 arguments, got 3",
        ),
        (
            "{{ dict.fromkeys([], value=1) }}",
            "dict.fromkeys() takes no keyword arguments",
        ),
    ] {
        let err = render(template).unwrap_err();
        assert!(err.contains(want), "{template}: {err}");
    }
    assert!(render("{{ dict.fromkeys(nope) }}").is_err());
}

/// The type `dict` and its bound method print as Python prints them inside a text, and are no
/// result: ansible-core refuses to store either, with these words.
#[test]
fn the_dict_type_and_its_method_are_no_result() {
    for (template, kind) in [
        ("{{ dict }}", "type"),
        ("{{ dict.fromkeys }}", "builtin_function_or_method"),
    ] {
        let err = render(template).unwrap_err();
        assert!(
            err.contains(&format!(
                "Error rendering template: Type '{kind}' is unsupported for variable storage."
            )),
            "{template}: {err}"
        );
    }
    assert_eq!(render("a {{ dict }}").unwrap(), json!("a <class 'dict'>"));
    assert_eq!(
        render("a {{ dict.fromkeys }}").unwrap(),
        json!("a <built-in method fromkeys of type object>")
    );
}

/// `encode` and the bytes it makes refuse a call in CPython's words, and fail by name for what
/// this engine does not run. The golden rows only record that the reference failed.
#[test]
fn encode_and_bytes_refuse_in_the_words_of_python() {
    for (template, want) in [
        (
            "{{ 'é'.encode('ascii') }}",
            "'ascii' codec can't encode character '\\xe9' in position 0: ordinal not in range(128)",
        ),
        (
            "{{ 'a€€b'.encode('latin-1') }}",
            "'latin-1' codec can't encode characters in position 1-2: ordinal not in range(256)",
        ),
        (
            "{{ 'é'.encode('ascii', 'nope') }}",
            "unknown error handler name 'nope'",
        ),
        ("{{ 'a'.encode('nope') }}", "unknown encoding: nope"),
        (
            "{{ 'a'.encode(1) }}",
            "encode() argument 'encoding' must be str, not int",
        ),
        (
            "{{ 'a'.encode(none) }}",
            "encode() argument 'encoding' must be str, not None",
        ),
        (
            "{{ 'a'.encode('utf-8', 'strict', 3) }}",
            "encode() takes at most 2 arguments (3 given)",
        ),
        (
            "{{ 'a'.encode(foo=1) }}",
            "encode() got an unexpected keyword argument 'foo'",
        ),
        (
            "{{ 'é'.encode().decode('ascii') }}",
            "'ascii' codec can't decode byte 0xc3 in position 0: ordinal not in range(128)",
        ),
        (
            "{{ 'é'.encode('latin-1').decode() }}",
            "'utf-8' codec can't decode byte 0xe9 in position 0: unexpected end of data",
        ),
        (
            "{{ 'é'.encode('utf-16').decode('utf-8') }}",
            "'utf-8' codec can't decode byte 0xff in position 0: invalid start byte",
        ),
        (
            "{{ 'abc'.encode().decode('utf-8', 'x', 3) }}",
            "decode() takes at most 2 arguments (3 given)",
        ),
        ("{{ 'a'.encode().hex('--') }}", "sep must be length 1."),
        (
            "{{ 'a'.encode().nope() }}",
            "object of type 'bytes' has no attribute 'nope'",
        ),
        (
            "{{ 'a'.encode().upper() }}",
            "method upper is not available yet",
        ),
    ] {
        let err = render(template).unwrap_err();
        assert!(err.contains(want), "{template}: {err}");
    }
}

/// What a template can ask for and this engine does not run fails with a message that names it.
#[test]
fn what_bytes_lack_is_refused_by_name() {
    for (template, want) in [
        (
            "{{ 'a'.encode('cp1252') }}",
            "the encoding 'cp1252' is not supported",
        ),
        (
            "{{ 'é'.encode('ascii', 'namereplace') }}",
            "the error handler 'namereplace' is not supported",
        ),
        (
            "{{ 'é'.encode('latin-1').decode('utf-8', 'surrogateescape') }}",
            "the error handler 'surrogateescape' is not supported",
        ),
        (
            "{{ 'a'.encode('utf-16').decode('utf-16') }}",
            "decoding with 'utf-16' is not supported",
        ),
        (
            "{{ 'é'.encode('latin-1') }}",
            "bytes that are not UTF-8 cannot be a result",
        ),
        (
            "x {{ 'é'.encode('latin-1') }}",
            "bytes that are not UTF-8 cannot be a result",
        ),
        (
            "{{ ['é'.encode('latin-1')] }}",
            "bytes that are not UTF-8 cannot be a result",
        ),
        (
            "{{ 'a'.encode() | to_json }}",
            "Object of type 'bytes' is not JSON serializable",
        ),
        (
            "{{ {'k': 'a'.encode()} | to_nice_json }}",
            "Object of type 'bytes' is not JSON serializable",
        ),
        (
            "{{ 'a'.encode() | to_yaml }}",
            "to_yaml does not write bytes yet",
        ),
        (
            "{{ {'k': 'a'.encode()} | dict2items }}",
            "this filter does not take bytes yet",
        ),
    ] {
        let err = render(template).unwrap_err();
        assert!(err.contains(want), "{template}: {err}");
    }
}

/// A bytes value counts, indexes and prints as Python does, and a result that holds one shows
/// its text; the rows of the golden corpus hold the reference's answers to the same templates.
#[test]
fn bytes_inside_a_result_show_their_text() {
    assert_eq!(
        render("{{ ['abc'.encode(), {'k': 'é'.encode()}] }}"),
        Ok(json!(["abc", {"k": "é"}]))
    );
    assert_eq!(render("{{ ['abc'.encode()] ~ '' }}"), Ok(json!("[b'abc']")));
    assert_eq!(
        render("{{ 'a\"b\\'c'.encode() ~ '' }}"),
        Ok(json!("b'a\"b\\'c'"))
    );
}

/// A wide integer sorts against native integers, floats and other wide integers by value, as
/// Python's does, and is an `int` in an error.
#[test]
fn sorting_a_wide_integer_is_exact() {
    let vars = vars(json!({}));
    let t = crate::testing::templar();
    let wide = "340282366920938463463374607431768211457";
    // 2^200 is 1606938044258990275541962092341162602522202993782792835301376.
    let above = "1606938044258990275541962092341162602522202993782792835301377";
    let below = "1606938044258990275541962092341162602522202993782792835301375";
    for (template, want) in [
        (
            format!(
                "{{% set l = ['{wide}' | int, 1, '-{wide}' | int, 2.5, 0] %}}{{% set _ = l.sort() %}}{{{{ l | map('string') | join(',') }}}}"
            ),
            format!("-{wide},0,1,2.5,{wide}"),
        ),
        (
            format!(
                "{{% set l = ['{above}' | int, 2.0 ** 200, 1.0e300, '{below}' | int] %}}{{% set _ = l.sort() %}}{{{{ l | map('type_debug') | join(',') }}}}"
            ),
            "int,float,int,float".to_string(),
        ),
        (
            format!(
                "{{% set l = [1.0e300, 2.0 ** 200, '-{above}' | int, -1.0e300] %}}{{% set _ = l.sort(reverse=True) %}}{{{{ l | map('type_debug') | join(',') }}}}"
            ),
            "float,float,int,float".to_string(),
        ),
        (
            format!(
                "{{% set l = ['{wide}' | int, '-{wide}' | int] %}}{{% set _ = l.sort() %}}{{{{ l | map('string') | join(',') }}}}"
            ),
            format!("-{wide},{wide}"),
        ),
        (
            format!(
                "{{% set l = ['-{wide}' | int, '{wide}' | int, '-{wide}' | int] %}}{{% set _ = l.sort() %}}{{{{ l | map('string') | join(',') }}}}"
            ),
            format!("-{wide},-{wide},{wide}"),
        ),
        (
            format!(
                "{{% set l = ['{wide}' | int, true, '-{wide}' | int, false] %}}{{% set _ = l.sort() %}}{{{{ l | map('type_debug') | join(',') }}}}"
            ),
            "int,bool,bool,int".to_string(),
        ),
        (
            format!(
                "{{% set l = ['inf' | float, '{wide}' | int, '-inf' | float, '-{wide}' | int] %}}{{% set _ = l.sort() %}}{{{{ l | map('string') | join(',') }}}}"
            ),
            format!("-inf,-{wide},{wide},inf"),
        ),
    ] {
        assert_eq!(t.render(&template, &vars), Ok(json!(want)), "{template}");
    }
    let err = t
        .render(
            &format!("{{% set l = ['{wide}' | int, 'a'] %}}{{% set _ = l.sort() %}}"),
            &vars,
        )
        .unwrap_err();
    assert!(err.0.contains("'int'") && err.0.contains("'str'"), "{err}");
}
