// SPDX-License-Identifier: GPL-3.0-or-later
use serde_json::json;

use super::*;
use crate::playbook::Origin;
use crate::shadow::recording::parse;

/// A recording made in `/rec` of the events given, one JSON object each.
fn replay(events: &[Value], truncated: bool) -> Replay {
    let header = json!({"event": "header", "format": 1, "ansible_core": "2.19.12", "cli": {},
        "cwd": "/rec", "playbooks": [], "inventory": {"files": []}, "extra_vars_hash": ""});
    let text = std::iter::once(&header)
        .chain(events)
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let mut recording = parse(text.as_bytes(), false).unwrap();
    recording.truncated = truncated;
    Replay::new(&recording, Path::new("/here"))
}

fn task_at(path: &str, line: usize) -> PlayTask {
    PlayTask {
        origin: Some(Origin {
            path: path.into(),
            line,
            col: 7,
        }),
        ..crate::testing::task("command")
    }
}

fn task_event(uuid: &str, path: &str) -> Value {
    json!({"event": "task", "uuid": uuid, "path": path, "name": "t"})
}

fn result(uuid: &str, item: Option<u64>, status: &str, result: Value) -> Value {
    json!({"event": "result", "host": "h", "task": uuid, "item": item, "status": status, "result": result})
}

/// A file included twice gives the same path two occurrences, in the order they ran, and the
/// replay finds each by its own; paths are compared from each run's directory.
#[test]
fn the_same_path_is_told_apart_by_occurrence() {
    let replay = replay(
        &[
            task_event("a", "/rec/inc.yml:1"),
            result("a", None, "ok", json!({"rc": 1})),
            task_event("b", "/rec/inc.yml:1"),
            result("b", None, "ok", json!({"rc": 2})),
        ],
        false,
    );
    // Steps 4 and 9 of the host are the file's two runs; another path between them counts apart.
    let mut seen = Some(Seen::default());
    let task = task_at("/here/inc.yml", 1);
    Seen::mark(&mut seen, 4, &task);
    Seen::mark(&mut seen, 6, &task_at("/here/site.yml", 3));
    Seen::mark(&mut seen, 9, &task);
    for (pos, rc) in [(4, 1), (9, 2)] {
        let n = Seen::of(seen.as_ref(), pos);
        assert_eq!(replay.answer("h", &task, n, 0, 1).unwrap().0["rc"], rc);
    }
    assert_eq!(Seen::of(seen.as_ref(), 6), 0);
    let mut off = None;
    Seen::mark(&mut off, 4, &task);
    assert!(off.is_none());
    let relative = task_at("inc.yml", 1);
    assert_eq!(replay.answer("h", &relative, 1, 0, 1).unwrap().0["rc"], 2);
}

/// What ansible-core added after the module is gone, so Volant decides it again.
#[test]
fn the_answer_is_the_module_s() {
    let mut task = task_at("/here/site.yml", 3);
    task.changed_when = vec!["false".into()];
    let replay = replay(
        &[
            task_event("a", "/rec/site.yml:3"),
            result(
                "a",
                Some(0),
                "ok",
                json!({"rc": 0, "changed": true, "item": "x",
                "ansible_loop_var": "item", "_ansible_no_log": false, "attempts": 1,
                "failed_when_result": false, "invocation": {"module_args": {}}}),
            ),
            result("a", None, "ok", json!({"results": []})),
        ],
        false,
    );
    let answer = replay.answer("h", &task, 0, 0, 1).unwrap().0;
    assert_eq!(
        Value::Object(answer),
        json!({"rc": 0, "invocation": {"module_args": {}}})
    );
}

/// An `until` gets the failed attempts in order, then the final result for every later one.
#[test]
fn attempts_come_in_the_recorded_order() {
    let replay = replay(
        &[
            task_event("a", "/rec/site.yml:3"),
            json!({"event": "retry", "host": "h", "task": "a", "attempt": 1, "result": {"rc": 1}}),
            json!({"event": "retry", "host": "h", "task": "a", "attempt": 2, "result": {"rc": 2}}),
            result("a", None, "ok", json!({"rc": 0})),
        ],
        false,
    );
    let task = task_at("/here/site.yml", 3);
    let rcs: Vec<Value> = (1..=4)
        .map(|n| replay.answer("h", &task, 0, 0, n).unwrap().0["rc"].clone())
        .collect();
    assert_eq!(rcs, [1, 2, 0, 0]);
}

/// Unreachable in the recording is unreachable in the replay; a task the recording does not
/// have fails, unless the recording was cut short, where the replay stops.
#[test]
fn unreachable_and_missing_results() {
    let events = [
        task_event("a", "/rec/site.yml:3"),
        result(
            "a",
            None,
            "unreachable",
            json!({"msg": "no route", "unreachable": true}),
        ),
    ];
    let task = task_at("/here/site.yml", 3);
    let err = replay(&events, false)
        .answer("h", &task, 0, 0, 1)
        .unwrap_err();
    assert_eq!(err, "recorded as unreachable: no route");

    let other = task_at("/here/site.yml", 9);
    let failed = replay(&events, false)
        .answer("h", &other, 0, 0, 1)
        .unwrap()
        .0;
    assert_eq!(
        failed["msg"],
        "the recording has no result for site.yml:9 on h"
    );
    let err = replay(&events, true)
        .answer("h", &other, 0, 0, 1)
        .unwrap_err();
    assert!(err.contains("truncated"), "{err}");
}

/// A `no_log` result answers with its recorded status, the only thing the recording kept.
#[test]
fn a_censored_result_answers_its_status() {
    let replay = replay(
        &[
            task_event("a", "/rec/site.yml:3"),
            result("a", None, "changed", json!({"censored": true})),
        ],
        false,
    );
    let answer = replay
        .answer("h", &task_at("site.yml", 3), 0, 0, 1)
        .unwrap()
        .0;
    assert_eq!(Value::Object(answer), json!({"changed": true}));
}
