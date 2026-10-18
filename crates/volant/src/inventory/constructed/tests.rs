// SPDX-License-Identifier: GPL-3.0-or-later
use rstest::rstest;
use serde_json::json;

use super::*;

#[rstest]
#[case::vars_plugins(json!({"use_vars_plugins": true}), "use_vars_plugins is not supported yet")]
#[case::extra_vars(json!({"use_extra_vars": "yes"}), "use_extra_vars is not supported yet")]
#[case::strict_type(
    json!({"strict": [1]}),
    "Invalid type for configuration option strict: expected a boolean, got <class 'list'>"
)]
#[case::compose_type(
    json!({"compose": "x"}),
    "Invalid type for configuration option compose: expected a dict, got <class 'str'>"
)]
fn an_option_it_cannot_honour_is_refused(#[case] config: Value, #[case] message: &str) {
    let Value::Object(config) = config else {
        unreachable!()
    };
    assert_eq!(Options::read(&config).err().as_deref(), Some(message));
}
