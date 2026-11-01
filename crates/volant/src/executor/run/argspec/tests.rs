// SPDX-License-Identifier: GPL-3.0-or-later

use rstest::rstest;
use serde_json::{Value, json};

use super::validate::validate;

const BOOLEANS: &str = "Valid booleans include: 0, 1, '0', 't', 'true', 'n', 'off', 'false', 'f', '1', 'no', 'yes', 'y', 'on'";
const TAGGED: &str = "<class 'ansible.module_utils._internal._datatag._AnsibleTagged";

/// Errors and their order measured on ansible-core 2.19.12 with the `roles_arg_spec` target's
/// `test1` role and specs shaped like it.
#[rstest]
#[case::nested_required_and_unknown(
    json!({"multi_level_option": {"type": "dict", "options": {"second_level": {"type": "dict",
        "options": {"third_level": {"type": "int", "required": true}}}}}}),
    json!({"multi_level_option": {"second_level": {"not_a_supported_suboption": true}}}),
    vec![
        "missing required arguments: third_level found in multi_level_option -> second_level".into(),
        "multi_level_option.second_level.not_a_supported_suboption. Supported parameters include: third_level.".into(),
    ],
)]
#[case::types_choices_and_sub_options(
    json!({
        "tidy": {"type": "list"},
        "count": {"type": "int"},
        "ratio": {"type": "float"},
        "size": {"type": "bits"},
        "pick": {"type": "str", "choices": ["a", "b"]},
        "opts": {"type": "dict", "options": {"flag": {"type": "bool", "default": true}}},
        "more": {"type": "dict", "options": {"x": {"type": "str"}}},
    }),
    json!({"tidy": {"k": "v"}, "count": 37.0, "ratio": "nope", "size": "foo", "pick": "c",
        "opts": {"flag": "not-a-bool"}, "more": "not-a-dict"}),
    vec![
        "dictionary requested, could not parse JSON or key=value".into(),
        format!("argument 'tidy' is of type dict and we were unable to convert to list: {TAGGED}Dict'> cannot be converted to a list"),
        format!("argument 'ratio' is of type str and we were unable to convert to float: {TAGGED}Str'> cannot be converted to a float"),
        format!("argument 'size' is of type str and we were unable to convert to bits: {TAGGED}Str'> cannot be converted to a Bit value"),
        "argument 'more' is of type str and we were unable to convert to dict: dictionary requested, could not parse JSON or key=value".into(),
        "value of pick must be one of: a, b, got: c".into(),
        format!("argument 'flag' is of type str found in 'opts'. and we were unable to convert to bool: The value 'not-a-bool' is not a valid boolean. {BOOLEANS}"),
        "value of 'more' must be of type dict or list of dicts".into(),
    ],
)]
#[case::aliases_and_families_in_a_list_of_dicts(
    json!({"conn": {"type": "list", "elements": "dict",
        "options": {"host": {"type": "str", "aliases": ["h"]}, "port": {"type": "int"}, "sock": {"type": "str"}},
        "mutually_exclusive": [["port", "sock"]], "required_one_of": [["host", "sock"]]}}),
    json!({"conn": [{"h": "x", "port": 1, "sock": "s", "bad": 1}, {"port": 2}]}),
    vec![
        "parameters are mutually exclusive: port|sock found in conn".into(),
        "one of the following is required: host, sock found in conn".into(),
        "conn.bad. Supported parameters include: host, port, sock (h).".into(),
    ],
)]
fn role_spec_errors(#[case] spec: Value, #[case] params: Value, #[case] want: Vec<String>) {
    let (Value::Object(spec), Value::Object(mut params)) = (spec, params) else {
        panic!("spec and params are mappings");
    };
    assert_eq!(validate(&spec, &mut params), want);
}
