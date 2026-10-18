// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::atomic::{AtomicUsize, Ordering};

use rstest::rstest;

use super::*;

/// A stand-in helper: it reads each request and acts as `argv[1]` says.
const STAND_IN: &str = r#"
import json, os, struct, sys, time
mode = sys.argv[1]
out = sys.stdout.buffer
def put(raw):
    out.write(struct.pack(">I", len(raw)) + raw)
    out.flush()
def get():
    head = sys.stdin.buffer.read(4)
    if len(head) < 4:
        sys.exit(0)
    return json.loads(sys.stdin.buffer.read(struct.unpack(">I", head)[0]))
n = 0
while True:
    request = get()
    n += 1
    if "action" in request and mode in ("asks", "ask_wrong_id", "die_mid_ask", "silent", "bad_ask"):
        if mode == "silent":
            time.sleep(600)
        ident = request["id"] + (1 if mode == "ask_wrong_id" else 0)
        questions = [{"op": "command", "cmd": "true", "in_data": "in", "sudoable": True}, {"op": "hostvar", "host": "h2"}]
        if mode == "bad_ask":
            questions = [{"op": "put_file", "local": "/a", "remote": "/b", "mode": 99999}]
        replies = []
        for question in questions:
            put(json.dumps({"id": ident, "ask": question}).encode())
            if mode == "die_mid_ask":
                os._exit(3)
            replies.append(get())
        put(json.dumps({"id": ident, "value": replies}).encode())
        continue
    if mode == "die_second" and n == 2:
        sys.stderr.write("gone\n")
        os._exit(3)
    if mode == "wrong_id":
        put(json.dumps({"id": request["id"] + 1, "value": 1}).encode())
    elif mode == "oversized":
        out.write(struct.pack(">I", 64 * 1024 * 1024 + 1)); out.flush()
    elif mode == "defaults":
        call = request["call"]
        names = call["args"]
        if call["kind"] == "actions":
            value = {n: n != "ansible.legacy.nosuch" for n in names}
        else:
            value = {n: {"missing": True} if n.endswith("nosuch") else {"actions": ["ansible.builtin.ping"]} for n in names}
        put(json.dumps({"id": request["id"], "value": value}).encode())
    elif mode == "bad_items":
        put(json.dumps({"id": request["id"], "value": {n: {"actions": [1]} for n in request["call"]["args"]}}).encode())
    elif mode == "latin1":
        put(b"\xff\xfe")
    else:
        put(json.dumps({"id": request["id"], "value": request["call"]["args"]}).encode())
"#;

fn bridge_with(mode: &'static str, starts: &Arc<AtomicUsize>) -> Arc<Bridge> {
    let starts = Arc::clone(starts);
    Bridge::with_start(Box::new(move || {
        starts.fetch_add(1, Ordering::SeqCst);
        PythonBuilder::spawn("python3", &["-c", STAND_IN, mode], &[])
    }))
}

fn ask(bridge: &Bridge) -> Result<Value, BridgeError> {
    bridge.call(
        PluginKind::Filter,
        "upper",
        &[json!("a")],
        &Map::new(),
        None,
    )
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(ToString::to_string).collect()
}

#[test]
fn the_module_defaults_requests_read_the_helper_s_answers() {
    let bridge = bridge_with("defaults", &Arc::new(AtomicUsize::new(0)));
    let found = bridge
        .actions_resolve(&names(&["ansible.legacy.foo", "ansible.legacy.nosuch"]))
        .expect("an answer");
    assert_eq!(
        found.into_iter().collect::<Vec<_>>(),
        [
            ("ansible.legacy.foo".to_string(), true),
            ("ansible.legacy.nosuch".to_string(), false)
        ]
    );
    let held = bridge
        .action_groups(&names(&["ansible.builtin.g", "my.col.nosuch"]))
        .expect("an answer");
    assert_eq!(
        held["ansible.builtin.g"],
        Some(names(&["ansible.builtin.ping"]))
    );
    assert_eq!(held["my.col.nosuch"], None);
}

#[rstest]
#[case::echo("echo")]
#[case::items_that_are_not_names("bad_items")]
fn module_defaults_answers_out_of_shape_are_refused(#[case] mode: &'static str) {
    let bridge = bridge_with(mode, &Arc::new(AtomicUsize::new(0)));
    let groups = bridge
        .action_groups(&names(&["ansible.builtin.g"]))
        .unwrap_err();
    assert!(
        groups.to_string().contains("in a shape it should not"),
        "{groups}"
    );
    let actions = bridge.actions_resolve(&names(&["x"])).unwrap_err();
    assert!(
        actions.to_string().contains("in a shape it should not"),
        "{actions}"
    );
}

#[test]
fn module_defaults_are_asked_about_once_and_refused_by_name() {
    use crate::collection::Resolved;
    use crate::playbook::PlayTask;

    let task = |key: &str| {
        let mut task = PlayTask::empty();
        task.module_defaults = vec![json!({ key: {"a": 1} })];
        task
    };
    let bridge = bridge_with("defaults", &Arc::new(AtomicUsize::new(0)));
    let resolved = Resolved::default();
    let group = task("group/g");
    resolved.ensure([&group], Some(&bridge)).expect("a group");
    assert_eq!(
        resolved.groups()["ansible.builtin.g"],
        names(&["ansible.builtin.ping"])
    );
    assert_eq!(
        resolved.ensure([&task("group/nosuch")], Some(&bridge)),
        Err("Error loading module_defaults: could not resolve the module_defaults group ansible.builtin.nosuch".to_string())
    );
    assert_eq!(
        resolved.ensure([&task("nosuch")], Some(&bridge)),
        Err("Could not resolve action ansible.legacy.nosuch in module_defaults".to_string())
    );
    assert_eq!(resolved.ensure([&task("foo")], Some(&bridge)), Ok(()));
    // Without a helper nothing can be checked, and the run says which name.
    assert_eq!(
        Resolved::default().ensure([&group], None),
        Err("Error loading module_defaults: could not resolve the module_defaults group ansible.builtin.g".to_string())
    );
}

#[test]
fn a_healthy_helper_answers_and_starts_once() {
    let starts = Arc::new(AtomicUsize::new(0));
    let bridge = bridge_with("echo", &starts);
    assert_eq!(ask(&bridge), Ok(json!(["a"])));
    assert_eq!(ask(&bridge), Ok(json!(["a"])));
    assert_eq!(starts.load(Ordering::SeqCst), 1);
}

#[rstest]
#[case::wrong_id("wrong_id", "answered request 2 to request 1")]
#[case::oversized_frame("oversized", "exceeds the 67108864 byte limit")]
#[case::not_utf8("latin1", "not UTF-8")]
fn a_helper_that_breaks_the_protocol_ends_the_bridge(
    #[case] mode: &'static str,
    #[case] said: &str,
) {
    let starts = Arc::new(AtomicUsize::new(0));
    let bridge = bridge_with(mode, &starts);
    let first = ask(&bridge).unwrap_err();
    assert!(first.to_string().contains(said), "{first}");
    assert_eq!(ask(&bridge), Err(first));
    assert_eq!(starts.load(Ordering::SeqCst), 1, "the helper was restarted");
}

#[test]
fn a_helper_killed_between_two_calls_fails_every_later_call() {
    let starts = Arc::new(AtomicUsize::new(0));
    let bridge = bridge_with("die_second", &starts);
    assert!(ask(&bridge).is_ok());
    let second = ask(&bridge).unwrap_err();
    assert!(
        second.to_string().starts_with("the Python helper exited"),
        "{second}"
    );
    assert_eq!(ask(&bridge), Err(second));
    assert_eq!(starts.load(Ordering::SeqCst), 1);
}

#[test]
fn a_helper_that_cannot_start_names_why_on_every_call() {
    let bridge = Bridge::with_start(Box::new(|| anyhow::bail!("no ansible-core")));
    let err = ask(&bridge).unwrap_err();
    assert_eq!(err, BridgeError::Start("no ansible-core".into()));
    assert_eq!(
        err.to_string(),
        "the Python helper could not start: no ansible-core"
    );
    assert_eq!(ask(&bridge), Err(err));
}

#[test]
fn a_request_over_the_cap_is_refused_without_ending_the_bridge() {
    let starts = Arc::new(AtomicUsize::new(0));
    let bridge = bridge_with("echo", &starts);
    let huge = json!("x".repeat(MAX_FRAME_LEN));
    let err = bridge
        .call(PluginKind::Filter, "upper", &[huge], &Map::new(), None)
        .unwrap_err();
    assert!(err.to_string().contains("byte limit"), "{err}");
    assert_eq!(ask(&bridge), Ok(json!(["a"])));
}

/// Runs one action on `bridge` on a thread of its own, answering every ask with `reply`, and
/// gives back what was asked and how the action ended. A run that hangs fails the test.
fn act(
    bridge: &Arc<Bridge>,
    deadline: Instant,
    reply: fn(&Ask) -> Reply,
) -> (Vec<Ask>, Result<Value, BridgeError>) {
    let bridge = Arc::clone(bridge);
    let (send, done) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut asked = Vec::new();
        let ended = bridge.run_action(
            "my.col.act",
            &json!({"args": {}}),
            &json!({}),
            &json!({}),
            deadline,
            &mut |ask| {
                let answer = reply(&ask);
                asked.push(ask);
                answer
            },
        );
        let _ = send.send((asked, ended));
    });
    done.recv_timeout(std::time::Duration::from_secs(60))
        .expect("the action ended")
}

/// A deadline no test row is meant to reach.
fn later() -> Instant {
    Instant::now() + std::time::Duration::from_secs(60)
}

fn answers(ask: &Ask) -> Reply {
    match ask {
        Ask::HostVar { .. } => Reply::Error("no such host".into()),
        _ => Reply::Value(json!("ok")),
    }
}

#[test]
fn an_action_s_asks_are_answered_in_turn_and_its_helper_kept() {
    let starts = Arc::new(AtomicUsize::new(0));
    let bridge = bridge_with("asks", &starts);
    for _ in 0..2 {
        let (asked, ended) = act(&bridge, later(), answers);
        assert_eq!(
            asked,
            [
                Ask::Command {
                    cmd: "true".into(),
                    stdin: Some(b"in".to_vec()),
                    sudoable: true
                },
                Ask::HostVar { host: "h2".into() }
            ]
        );
        let id = ended.as_ref().expect("a value")[0]["id"].clone();
        assert_eq!(
            ended,
            Ok(json!([
                {"id": id, "reply": {"value": "ok"}},
                {"id": id, "reply": {"error": "no such host"}}
            ]))
        );
    }
    assert_eq!(starts.load(Ordering::SeqCst), 1);
}

#[test]
fn an_answer_over_the_cap_reaches_the_plugin_as_an_error() {
    let bridge = bridge_with("asks", &Arc::new(AtomicUsize::new(0)));
    let (_, ended) = act(&bridge, later(), |_| {
        Reply::Value(json!("x".repeat(MAX_FRAME_LEN)))
    });
    let ended = ended.expect("a value");
    let error = ended[0]["reply"]["error"].as_str().unwrap_or_default();
    assert!(error.contains("over the 67108864 byte limit"), "{ended}");
}

#[rstest]
#[case::ask_for_another_request("ask_wrong_id", "answered request")]
#[case::death_mid_ask("die_mid_ask", "the Python helper exited")]
#[case::oversized_frame("oversized", "exceeds the 67108864 byte limit")]
#[case::ask_out_of_shape("bad_ask", "the mode is not a file mode")]
#[case::still_running_at_the_deadline("silent", "was still running at the task's deadline")]
fn an_action_whose_helper_fails_ends_it_and_the_next_starts_another(
    #[case] mode: &'static str,
    #[case] said: &str,
) {
    let starts = Arc::new(AtomicUsize::new(0));
    let bridge = bridge_with(mode, &starts);
    // Only the silent helper is meant to reach its deadline.
    let wait = if mode == "silent" { 500 } else { 60_000 };
    let deadline = Instant::now() + std::time::Duration::from_millis(wait);
    for start in 1..=2 {
        let (_, ended) = act(&bridge, deadline, answers);
        let err = ended.unwrap_err();
        assert!(err.to_string().contains(said), "{err}");
        assert_eq!(starts.load(Ordering::SeqCst), start);
    }
}

#[test]
fn actions_that_cannot_start_a_helper_name_why_without_retrying() {
    let starts = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&starts);
    let bridge = Bridge::with_start(Box::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("no ansible-core")
    }));
    for _ in 0..2 {
        let (_, ended) = act(&bridge, later(), answers);
        assert_eq!(ended, Err(BridgeError::Start("no ansible-core".into())));
    }
    assert_eq!(starts.load(Ordering::SeqCst), 1);
}
