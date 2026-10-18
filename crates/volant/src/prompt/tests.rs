// SPDX-License-Identifier: GPL-3.0-or-later
use serde_json::json;

use super::*;

fn play(prompt: Value) -> Play {
    let text = format!("- hosts: all\n  vars_prompt: {prompt}\n  tasks: []\n");
    crate::playbook::parse(&text, "p.yml")
        .expect("the play parses")
        .plays
        .remove(0)
}

fn run(
    prompt: Value,
    extra: &Map<String, Value>,
    mut typed: Option<Vec<&str>>,
) -> anyhow::Result<Vec<(String, Value)>> {
    let play = play(prompt);
    let templar = Templar::new(std::path::PathBuf::from("."));
    let terminal = typed.is_some();
    let mut answer = |_: &str, _: bool| {
        Ok(typed
            .as_mut()
            .expect("asked on a terminal")
            .remove(0)
            .to_string())
    };
    let ask: Option<Ask<'_>> = if terminal { Some(&mut answer) } else { None };
    answers(&play, &templar, &Map::new(), extra, ask, &mut false)
}

#[test]
fn a_run_without_a_terminal_takes_the_defaults_and_the_extra_vars_win() {
    let extra = json!({"c": "cli"}).as_object().unwrap().clone();
    let got = run(
        json!([
            {"name": "a", "default": "dflt"},
            {"name": "b"},
            {"name": "c", "default": "never"},
            {"name": "d", "default": 5},
        ]),
        &extra,
        None,
    )
    .unwrap();
    assert_eq!(
        got,
        [
            ("a".to_string(), json!("dflt")),
            ("b".to_string(), json!("None")),
            ("d".to_string(), json!("5")),
        ]
    );
}

#[test]
fn a_terminal_is_asked_with_the_reference_s_prompt_and_an_empty_answer_takes_the_default() {
    let got = run(
        json!([
            {"name": "a", "prompt": "Enter a", "default": "dflt", "private": false},
            {"name": "b", "confirm": true},
            {"name": "c", "prompt": ""},
        ]),
        &Map::new(),
        Some(vec!["", "x", "y", "z", "z", "w"]),
    )
    .unwrap();
    assert_eq!(
        got,
        [
            ("a".to_string(), json!("dflt")),
            ("b".to_string(), json!("z")),
            ("c".to_string(), json!("w")),
        ]
    );
}

#[test]
fn an_unsafe_default_is_kept_as_typed_text() {
    let got = run(
        json!([{"name": "a", "default": "{{ \"{{ q }}\" }}", "unsafe": true}]),
        &Map::new(),
        None,
    )
    .unwrap();
    assert_eq!(
        got[0].1,
        crate::yaml::unsafe_data(json!("{{ q }}")),
        "rendered once, then marked as data"
    );
}

#[test]
fn the_messages_are_the_reference_s() {
    let entry = |value: Value| Entry::read(&value).unwrap().message();
    assert_eq!(
        entry(json!({"name": "a", "prompt": "Enter a", "default": "d"})),
        "Enter a [d]: "
    );
    assert_eq!(
        entry(json!({"name": "a", "prompt": "Enter a"})),
        "Enter a: "
    );
    assert_eq!(entry(json!({"name": "a"})), "a: ");
    assert_eq!(entry(json!({"name": "a", "prompt": ""})), "input for a: ");
}
