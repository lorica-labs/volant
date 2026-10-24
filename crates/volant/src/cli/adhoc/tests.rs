// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use rstest::rstest;

#[rstest]
#[case::raw_command("command", "false", r#"{"_raw_params": "false"}"#)]
#[case::raw_options_first(
    "command",
    "chdir=/tmp false",
    r#"{"chdir": "/tmp", "_raw_params": "false"}"#
)]
#[case::options("ping", "foo=bar", r#"{"foo": "bar"}"#)]
#[case::json("ping", r#"{"data": "j", "a": 1}"#, r#"{"data": "j", "a": 1}"#)]
#[case::empty_json_is_words("ping", "{}", r#"{"_raw_params": "{}"}"#)]
#[case::nothing("ping", "", "{}")]
// Measured: `-a` is not stripped before the JSON test, so a space in front makes it words.
#[case::spaced_json_is_words(
    "ping",
    r#" {"data": "j"}"#,
    r#"{"_raw_params": "{\"data\": \"j\"}"}"#
)]
#[case::trailing_space_is_words(
    "ping",
    r#"{"data": "j"} "#,
    r#"{"_raw_params": "{\"data\": \"j\"} "}"#
)]
fn module_args_read_as_play_ds_reads_them(
    #[case] module: &str,
    #[case] raw: &str,
    #[case] want: &str,
) {
    let want: Map<String, Value> = serde_json::from_str(want).unwrap();
    let got = module_args(module, raw).unwrap();
    assert_eq!(
        got.iter().collect::<Vec<_>>(),
        want.iter().collect::<Vec<_>>()
    );
}

#[rstest]
#[case::bad_json("{bad}", 250)]
#[case::unbalanced_quote("x=\"a", 4)]
fn module_args_that_read_as_nothing_are_refused(#[case] raw: &str, #[case] code: i32) {
    assert_eq!(module_args("ping", raw).map_err(|(c, _)| c), Err(code));
}

/// Measured with `-m command -a "false <N x>"`: the task is shown whole up to 120 characters,
/// then shortened by words.
#[rstest]
#[case(
    10,
    "{'action': 'command', 'args': {'_raw_params': 'false xxxxxxxxxx'}, 'timeout': 0, 'async_val': 0, 'poll': 15}"
)]
#[case(
    40,
    "{'action': 'command', 'args': {'_raw_params': 'false xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'}, 'timeout': 0, [...]"
)]
#[case(
    58,
    "{'action': 'command', 'args': {'_raw_params': 'false xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'}, [...]"
)]
#[case(59, "{'action': 'command', 'args': {'_raw_params': 'false [...]")]
fn the_task_shown_in_an_error_block_is_shortened(#[case] n: usize, #[case] want: &str) {
    let adhoc = Adhoc::new("h1", "command", &format!("false {}", "x".repeat(n))).unwrap();
    assert_eq!(
        &*adhoc.task_origin().path,
        format!("<adhoc 'command' task>\n\n{want}")
    );
}

#[test]
fn an_include_carries_no_async_keys() {
    let adhoc = Adhoc::new("h1", "include_tasks", "file=t.yml").unwrap();
    assert_eq!(
        &*adhoc.task_origin().path,
        "<adhoc 'include_tasks' task>\n\n{'action': 'include_tasks', 'args': {'file': 't.yml'}, 'timeout': 0}"
    );
}
