// SPDX-License-Identifier: GPL-3.0-or-later
use serde_json::json;

use super::*;

/// What a module turns the value it was given into is the same value.
#[rstest::rstest]
#[case(json!(true), json!("yes"), true)]
#[case(json!(false), json!("no"), true)]
#[case(json!(true), json!("no"), false)]
#[case(json!(8), json!("8"), true)]
#[case(json!("0644"), json!("0644"), true)]
#[case(json!("0644"), json!("0600"), false)]
#[case(json!(["a", "b"]), json!("a, b"), true)]
#[case(json!(["a", "b"]), json!(["a"]), false)]
#[case(json!({"x": 1, "y": null}), json!({"x": "1"}), true)]
fn a_value_the_module_converted_is_alike(
    #[case] theirs: Value,
    #[case] ours: Value,
    #[case] expected: bool,
) {
    assert_eq!(alike(&theirs, &ours), expected);
}

/// Only the keys Volant sent are compared: the module's defaults, Volant's own keys and the
/// values the module hid are not.
#[test]
fn only_what_volant_sent_is_compared() {
    let recorded = json!({"_raw_params": "echo a", "chdir": null, "creates": "x",
        "password": "VALUE_SPECIFIED_IN_NO_LOG_PARAMETER"});
    let sent = json!({"_raw_params": "echo b", "creates": "x", "password": "s3cret",
        "_volant_tty": true});
    let (ansible, volant) =
        different_args(recorded.as_object().unwrap(), sent.as_object().unwrap());
    assert_eq!(Value::Object(ansible), json!({"_raw_params": "echo a"}));
    assert_eq!(Value::Object(volant), json!({"_raw_params": "echo b"}));
}
