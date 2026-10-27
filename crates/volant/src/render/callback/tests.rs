// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::Write;
use std::sync::{Arc, Mutex};

use rstest::rstest;
use serde_json::json;
use volant_protocol::TaskResult;

use super::{LOOP_RESULTS, StdoutCallback};
use crate::render::{Dump, Renderer};
use crate::stats::{Outcome, Stats};

struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What `f` prints at `verbosity`, through `callback` when one is given.
fn printed(
    callback: Option<StdoutCallback>,
    verbosity: u8,
    f: impl FnOnce(&mut Renderer),
) -> String {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut out = Renderer::with_writer(Box::new(Shared(buf.clone())), false, 79, verbosity);
    if let Some(callback) = callback {
        out.set_stdout_callback(callback);
    }
    f(&mut out);
    String::from_utf8(buf.lock().unwrap().clone()).unwrap()
}

fn result(value: serde_json::Value) -> TaskResult {
    TaskResult(value.as_object().unwrap().clone())
}

#[rstest]
#[case("default", Some(StdoutCallback::Default))]
#[case("ansible.builtin.minimal", Some(StdoutCallback::Minimal))]
#[case("oneline", Some(StdoutCallback::Oneline))]
#[case("ansible.builtin.tree", Some(StdoutCallback::Tree))]
#[case("json", Some(StdoutCallback::Json))]
#[case("ansible.posix.json", Some(StdoutCallback::Json))]
#[case("community.general.unixy", None)]
#[case("community.general.yaml", None)]
fn a_callback_is_known_by_its_short_and_its_full_name(
    #[case] name: &str,
    #[case] want: Option<StdoutCallback>,
) {
    assert_eq!(StdoutCallback::named(name), want);
}

/// Banners, the recap, retries, `...ignoring`, includes and loop items are the `default`
/// callback's alone.
#[rstest]
#[case(StdoutCallback::Minimal)]
#[case(StdoutCallback::Oneline)]
#[case(StdoutCallback::Tree)]
fn only_the_default_callback_prints_banners_items_and_the_recap(#[case] callback: StdoutCallback) {
    let item = result(json!({"msg": "a"}));
    let text = printed(Some(callback), 0, |out| {
        out.play("p");
        out.task("t");
        out.set_action("debug");
        out.retrying("h1", "t", 2, &item, false);
        out.result(
            "h1",
            Outcome::Ok,
            &item,
            Some("a"),
            Dump::Debug,
            false,
            None,
        );
        out.ignoring();
        out.included("inc.yml", &["h1".to_string()], None);
        out.recap(&Stats::default(), None);
    });
    assert_eq!(text, "");
}

/// A loop's aggregate carries its items for `minimal` and `oneline`, which show them as the
/// result's `results`; a `command` result takes the command form.
#[rstest]
#[case(
    StdoutCallback::Minimal,
    "h1 | SUCCESS => {\n    \"changed\": false,\n    \"msg\": \"All items completed\",\n    \"results\": [\n        \"a\"\n    ]\n}\nh1 | CHANGED | rc=0 >>\nhi\n"
)]
#[case(
    StdoutCallback::Oneline,
    "h1 | SUCCESS => {\"changed\": false,\"msg\": \"All items completed\",\"results\": [\"a\"]}\nh1 | CHANGED | rc=0 | (stdout) hi\n"
)]
fn the_result_lines_follow_the_task_s_action(#[case] callback: StdoutCallback, #[case] want: &str) {
    let aggregate =
        result(json!({"changed": false, "msg": "All items completed", LOOP_RESULTS: ["a"]}));
    let command = result(json!({"changed": true, "rc": 0, "stdout": "hi", "stderr": ""}));
    let text = printed(Some(callback), 0, |out| {
        out.set_action("ansible.builtin.set_fact");
        out.result("h1", Outcome::Ok, &aggregate, None, Dump::No, false, None);
        out.set_action("command");
        out.result(
            "h1",
            Outcome::Changed,
            &command,
            None,
            Dump::No,
            false,
            None,
        );
    });
    assert_eq!(text, want);
}

#[test]
fn the_default_callback_hides_a_loop_s_items() {
    let aggregate = result(json!({"changed": false, LOOP_RESULTS: ["a"]}));
    let text = printed(None, 1, |out| {
        out.result("h1", Outcome::Ok, &aggregate, None, Dump::No, false, None);
    });
    assert_eq!(text, "ok: [h1] => {\"changed\": false}\n");
}

/// `_dump_results`' indent: four for YAML and for `pretty_results`, none whatever asked for it
/// once `pretty_results` is off; and YAML's `<omitted>` and nesting follow `pretty_results`.
#[rstest]
#[case::json(
    false,
    None,
    false,
    "ok: [h1] => {\"changed\": false, \"stdout\": \"a\", \"stdout_lines\": [\"a\"]}\n"
)]
#[case::json_pretty(
    false,
    Some(true),
    false,
    "ok: [h1] => {\n    \"changed\": false,\n    \"stdout\": \"a\",\n    \"stdout_lines\": [\n        \"a\"\n    ]\n}\n"
)]
#[case::json_debug_not_pretty(false, Some(false), true, "ok: [h1] => {\"msg\": \"a\"}\n")]
#[case::yaml(
    true,
    None,
    false,
    "ok: [h1] => \n    changed: false\n    stdout: a\n    stdout_lines: <omitted>\n"
)]
#[case::yaml_exact(
    true,
    Some(false),
    false,
    "ok: [h1] => \n    changed: false\n    stdout: a\n    stdout_lines:\n    - a\n"
)]
fn the_result_format_and_pretty_results_shape_a_result(
    #[case] yaml: bool,
    #[case] pretty: Option<bool>,
    #[case] debug: bool,
    #[case] want: &str,
) {
    let body = if debug {
        result(json!({"msg": "a", crate::render::VERBOSE_ALWAYS: true}))
    } else {
        result(json!({"changed": false, "stdout": "a", "stdout_lines": ["a"]}))
    };
    let dump = if debug { Dump::Debug } else { Dump::No };
    let text = printed(None, 1, |out| {
        out.set_result_format(yaml, pretty);
        out.result("h1", Outcome::Ok, &body, None, dump, false, None);
    });
    assert_eq!(text, want);
}

/// The `json` callback prints nothing as the run goes, then one document: each play and task with
/// the results by host, `action` and `_ansible_no_log` added, a loop's items back under `results`
/// with their labels, an unreachable host, and every character past ASCII escaped.
#[test]
fn the_json_callback_prints_one_document_at_the_recap() {
    let book = crate::testing::play("- name: p\n  hosts: all\n  tasks: []\n");
    let text = printed(Some(StdoutCallback::Json), 0, |out| {
        out.set_json_indent(0);
        out.play_of(&book.plays[0]);
        out.task_started("t", "debug", None);
        let message = result(json!({"msg": "héllo", "failed": false}));
        out.result("h1", Outcome::Ok, &message, None, Dump::Debug, false, None);
        out.ignoring();
        out.task_started("loop", "debug", None);
        let aggregate = result(json!({"changed": false, LOOP_RESULTS: [{"item": "a"}]}));
        out.result("h1", Outcome::Ok, &aggregate, None, Dump::No, false, None);
        out.unreachable("h2", "gone", false, None);
        out.recap(&Stats::default(), None);
    });
    let escaped = format!("h{}u00e9llo", '\\');
    assert!(text.is_ascii() && text.contains(&escaped), "{text}");
    assert_eq!(text.lines().count(), 1, "{text}");
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    let play = &doc["plays"][0];
    assert_eq!(play["play"]["name"], "p");
    let tasks = play["tasks"].as_array().unwrap();
    assert_eq!(
        tasks[0]["hosts"]["h1"],
        json!({"msg": "héllo", "action": "debug", "_ansible_no_log": false})
    );
    assert_eq!(
        tasks[1]["hosts"]["h1"],
        json!({
            "changed": false,
            "action": "debug",
            "results": [{
                "item": "a",
                "_ansible_item_label": "a",
                "_ansible_no_log": false,
                "_ansible_verbose_always": true
            }]
        })
    );
    assert_eq!(
        tasks[1]["hosts"]["h2"],
        json!({"changed": false, "msg": "gone", "unreachable": true, "action": "debug"})
    );
    assert_eq!(tasks[1]["task"]["name"], "loop");
    assert_eq!(doc["custom_stats"], json!({}));
}
