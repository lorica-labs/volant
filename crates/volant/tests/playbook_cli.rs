// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(unix)]
use std::path::Path;
use std::process::{Command, Output};

fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .display()
        .to_string()
}

fn volant(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(args)
        .env("NO_COLOR", "1")
        .env_remove("COLUMNS")
        .output()
        .expect("volant runs")
}

fn settings() -> insta::Settings {
    let mut s = insta::Settings::clone_current();
    s.add_filter(r#""start": "[^"]*""#, r#""start": "[time]""#);
    s.add_filter(r#""end": "[^"]*""#, r#""end": "[time]""#);
    s.add_filter(r#""delta": "[^"]*""#, r#""delta": "[time]""#);
    s
}

#[test]
fn a_playbook_runs_end_to_end_with_ansible_output() {
    let out = volant(&[
        "playbook",
        "-i",
        &fixture("inventory.ini"),
        &fixture("site.yml"),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    settings().bind(|| insta::assert_snapshot!(String::from_utf8(out.stdout).unwrap()));
}

#[test]
fn a_failing_task_stops_the_host_and_exits_2() {
    let out = volant(&["playbook", &fixture("failing.yml")]);
    assert_eq!(out.status.code(), Some(2));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("fatal: [localhost]: FAILED!"), "{text}");
    assert!(
        !text.contains("Never reached"),
        "tasks after a failure must not be displayed: {text}"
    );
    assert!(text.contains("failed=1"), "{text}");
}

#[test]
fn an_agent_that_dies_mid_batch_makes_its_host_unreachable() {
    let out = volant(&["playbook", &fixture("dying-agent.yml")]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("fatal: [localhost]: UNREACHABLE!"), "{text}");
    assert!(
        !text.contains("Never reached"),
        "tasks after the agent died must not be displayed: {text}"
    );
    assert!(text.contains("unreachable=1"), "{text}");
    assert_eq!(out.status.code(), Some(4), "{text}");
}

#[test]
fn the_playbook_alias_takes_the_same_arguments() {
    let alias = Path::new(env!("CARGO_BIN_EXE_volant")).with_file_name("volant-playbook");
    let out = Command::new(alias)
        .args(["-i", &fixture("inventory.ini"), &fixture("site.yml")])
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn a_bad_playbook_exits_1_with_the_error_on_stderr() {
    let out = volant(&["playbook", &fixture("does-not-exist.yml")]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("does-not-exist.yml"));
}
