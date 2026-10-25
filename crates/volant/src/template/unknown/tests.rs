// SPDX-License-Identifier: GPL-3.0-or-later
use serde_json::{Map, Value, json};

use super::*;
use crate::testing::templar;

fn render(text: &str, vars: Value) -> Value {
    let vars: Map<String, Value> = vars.as_object().cloned().unwrap_or_default();
    templar()
        .render(text, &vars)
        .unwrap_or_else(|e| json!(format!("error: {}", e.0)))
}

/// The reason a rendered value is unknown for, when it is.
fn unknown(rendered: &Value) -> Option<String> {
    reason_in_json(rendered)
}

/// Each source that answers otherwise on another run is unknown in a replay, and runs as usual
/// outside one.
#[rstest::rstest]
#[case::pipe("{{ lookup('pipe', 'echo hi') }}", "lookup 'pipe'")]
#[case::env("{{ lookup('env', 'HOME') }}", "lookup 'env'")]
#[case::password("{{ lookup('password', '/dev/null') }}", "lookup 'password'")]
#[case::now("{{ now() }}", "now()")]
#[case::random("{{ [1, 2] | random }}", "filter 'random'")]
#[case::uuid("{{ 99 | random | to_uuid }}", "filter 'random'")]
fn a_source_that_changes_is_unknown_in_a_replay(#[case] text: &str, #[case] reason: &str) {
    let outside = render(text, json!({}));
    assert_eq!(unknown(&outside), None, "{outside}");
    enable_here();
    let rendered = render(text, json!({}));
    assert_eq!(unknown(&rendered).as_deref(), Some(reason), "{rendered}");
}

/// A file outside the directory the replay runs in is unknown; one inside it is read.
#[test]
fn a_file_outside_the_checkout_is_unknown() {
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(outside.path(), "far").unwrap();
    let far = format!("{{{{ lookup('file', '{}') }}}}", outside.path().display());
    let inside = tempfile::NamedTempFile::new_in(".").unwrap();
    std::fs::write(inside.path(), "near").unwrap();
    let inside = std::fs::canonicalize(inside.path()).unwrap();
    let near = format!("{{{{ lookup('file', '{}') }}}}", inside.display());
    assert_eq!(render(&far, json!({})), json!("far"));
    enable_here();
    assert_eq!(
        unknown(&render(&far, json!({}))).as_deref(),
        Some("lookup 'file'")
    );
    assert_eq!(render(&near, json!({})), json!("near"));
}

/// Whatever an expression does with an unknown value, the expression is unknown.
#[rstest::rstest]
#[case::concatenation("{{ 'at ' ~ lookup('env', 'X') }}")]
#[case::text_around("at {{ lookup('env', 'X') }} now")]
#[case::filter_chain("{{ lookup('env', 'X') | upper | replace('A', 'B') | length }}")]
#[case::comparison("{{ lookup('env', 'X') == 'y' }}")]
#[case::arithmetic("{{ lookup('env', 'X') | int + 1 }}")]
#[case::item_of_it("{{ lookup('env', 'X')['key'] }}")]
#[case::dict_holding_it("{{ {'a': lookup('env', 'X')}['a'] }}")]
#[case::condition("{{ 'yes' if lookup('env', 'X') else 'no' }}")]
#[case::default("{{ lookup('env', 'X') | default('d') }}")]
fn an_expression_reading_an_unknown_is_unknown(#[case] text: &str) {
    enable_here();
    let rendered = render(text, json!({}));
    assert_eq!(
        unknown(&rendered).as_deref(),
        Some("lookup 'env'"),
        "{rendered}"
    );
}

/// A variable holding the marker, alone or inside a mapping such as a censored result, reads
/// as the unknown value; a variable that does not stays as it is.
#[test]
fn a_variable_holding_a_marker_reads_as_unknown() {
    enable_here();
    let vars = json!({
        "x": marker("lookup 'pipe'"),
        "reg": {"changed": true, "stdout": marker("no_log"), "rc": marker("no_log")},
        "plain": "text",
    });
    assert_eq!(
        unknown(&render("{{ x | upper }}", vars.clone())).as_deref(),
        Some("lookup 'pipe'")
    );
    assert_eq!(
        unknown(&render("{{ reg.rc == 0 }}", vars.clone())).as_deref(),
        Some("no_log")
    );
    assert_eq!(render("{{ reg.changed }}", vars.clone()), json!(true));
    assert_eq!(render("{{ plain | upper }}", vars), json!("TEXT"));
}

/// The marker names its reason, and is found inside text and nested values.
#[test]
fn the_marker_carries_its_reason() {
    let text = format!("before {} after", marker("lookup 'pipe'"));
    assert_eq!(reason_in(&text), Some("lookup 'pipe'"));
    assert_eq!(reason_in("no marker"), None);
    let nested = json!({"a": [1, {"b": text}]});
    assert_eq!(reason_in_json(&nested).as_deref(), Some("lookup 'pipe'"));
}
