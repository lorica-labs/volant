// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use serde_json::json;

/// Which result shapes count as changed, measured against ansible-core 2.19.12: `changed:` on
/// the task line and `changed=3` in the recap.
#[test]
fn a_result_changed_by_the_reference_s_own_rule() {
    let cases = [
        (json!({"changed": "yes"}), true),
        (json!({"changed": "false"}), true),
        (json!({"changed": 1}), true),
        (json!({"changed": 0}), false),
        (json!({"changed": null}), false),
        (json!({"changed": ""}), false),
        (json!({"changed": []}), false),
        (json!({}), false),
    ];
    let wrong: Vec<String> = cases
        .into_iter()
        .filter_map(|(shape, changed)| {
            let Value::Object(map) = shape.clone() else {
                unreachable!()
            };
            (TaskResult(map).changed() != changed).then(|| format!("{shape} changed={changed}"))
        })
        .collect();
    assert!(
        wrong.is_empty(),
        "read otherwise than the reference: {wrong:?}"
    );
}

/// Which result shapes count as failed under `ignore_errors`, measured against ansible-core
/// 2.19.12: `fatal` for every `true` and `ok` for every `false`.
#[test]
fn a_result_fails_by_the_reference_s_own_rule() {
    let cases = [
        (json!({"failed": "yes"}), true),
        (json!({"failed": "false"}), true),
        (json!({"failed": 0}), false),
        (json!({"failed": 1}), true),
        (json!({"failed": null}), false),
        (json!({"failed": ""}), false),
        (json!({"failed": []}), false),
        (json!({"rc": "2"}), true),
        (json!({"rc": "0"}), false),
        (json!({"rc": null}), true),
        (json!({"rc": 0.0}), false),
        (json!({"rc": false}), false),
        (json!({"rc": true}), true),
        (json!({"rc": ""}), true),
        (json!({"rc": 1}), true),
        (json!({}), false),
        (json!({"failed": false, "rc": 1}), false),
    ];
    let wrong: Vec<String> = cases
        .into_iter()
        .filter_map(|(shape, fails)| {
            let Value::Object(map) = shape.clone() else {
                unreachable!()
            };
            (TaskResult(map).failed() != fails).then(|| format!("{shape} fails={fails}"))
        })
        .collect();
    assert!(
        wrong.is_empty(),
        "read otherwise than the reference: {wrong:?}"
    );
}

/// `failed_when_result` decides a task when a result carries one (ansible-core 2.19.12): retries
/// that ran out pass under `failed_when: false`, and in a loop only the carriers decide.
#[test]
fn a_task_fails_by_failed_when_result_when_it_has_one() {
    let results = |shapes: &[Value]| -> Vec<TaskResult> {
        shapes
            .iter()
            .map(|shape| TaskResult(shape.as_object().expect("an object").clone()))
            .collect()
    };
    let cases = [
        (
            vec![json!({"failed": true, "failed_when_result": false})],
            false,
        ),
        (
            vec![json!({"failed": false, "failed_when_result": true})],
            true,
        ),
        (
            vec![json!({"failed": true, "failed_when_result": "Error while evaluating"})],
            true,
        ),
        (vec![json!({"failed": true})], true),
        (
            vec![
                json!({"failed": true, "failed_when_result": false}),
                json!({"failed": true}),
            ],
            false,
        ),
        (
            vec![
                json!({"failed": true, "failed_when_result": false}),
                json!({"failed": false, "failed_when_result": true}),
            ],
            true,
        ),
    ];
    for (shapes, fails) in cases {
        let rs = results(&shapes);
        let decided = failed_when_decides(&rs);
        assert_eq!(
            rs.iter().any(|r| r.is_failed_among(decided)),
            fails,
            "{shapes:?}"
        );
    }
}

/// The native fields are additive: absent on the wire when empty, defaulted when an older peer
/// omits them.
#[test]
fn the_native_fields_are_absent_on_the_wire_when_empty_and_default_on_the_way_in() {
    let old = r#"{"type":"task_result","batch":1,"index":0,"result":{"changed":false}}"#;
    let FromAgent::TaskResult { ran, .. } = serde_json::from_str(old).unwrap() else {
        panic!("a task result")
    };
    assert_eq!(ran, None);
    let old = r#"{"type":"ready","protocol":5,"version":"0.0.0","arch":"x86_64"}"#;
    let FromAgent::Ready { natives, .. } = serde_json::from_str(old).unwrap() else {
        panic!("a ready")
    };
    assert!(natives.is_empty());
    let task: Task = serde_json::from_str(r#"{"module":"stat"}"#).unwrap();
    assert!(!task.force_python);
    let task = Task {
        module: "stat".into(),
        ..Default::default()
    };
    assert!(
        !serde_json::to_string(&task)
            .unwrap()
            .contains("force_python")
    );
    let forced = Task {
        force_python: true,
        ..task
    };
    assert!(
        serde_json::to_string(&forced)
            .unwrap()
            .contains(r#""force_python":true"#)
    );
    let ran = Ran {
        path: ExecPath::Fallback,
        reason: Some("not implemented".into()),
        micros: 12,
        fork_micros: None,
        import_micros: None,
        module_micros: None,
    };
    let text = serde_json::to_string(&ran).unwrap();
    assert_eq!(
        text,
        r#"{"path":"fallback","reason":"not implemented","micros":12}"#
    );
    assert_eq!(serde_json::from_str::<Ran>(&text).unwrap(), ran);
    assert_eq!(
        PROTOCOL_VERSION, 8,
        "additive fields do not move the version"
    );
}

#[test]
fn hello_has_a_snake_case_type_tag() {
    let text = serde_json::to_string(&ToAgent::Hello {
        protocol: PROTOCOL_VERSION,
    })
    .unwrap();
    assert_eq!(
        text,
        format!(r#"{{"type":"hello","protocol":{PROTOCOL_VERSION}}}"#)
    );
}

/// A host with no Python carries no `interpreters` key at all, and a host with one carries the
/// list in the order the agent found them.
#[test]
fn ready_carries_the_interpreters_only_when_the_host_has_some() {
    let ready = |interpreters: Vec<String>| FromAgent::Ready {
        protocol: PROTOCOL_VERSION,
        version: "0.0.0".into(),
        arch: "x86_64".into(),
        interpreters,
        natives: Vec::new(),
    };
    let empty = serde_json::to_string(&ready(Vec::new())).unwrap();
    assert!(!empty.contains("interpreters"));
    assert_eq!(
        serde_json::from_str::<FromAgent>(&empty).unwrap(),
        ready(Vec::new()),
        "a handshake from a host with no Python has to parse, which is the one the empty \
             list exists for"
    );
    let text = serde_json::to_string(&ready(vec![
        "/usr/bin/python3.12".into(),
        "/usr/local/bin/python3.9".into(),
    ]))
    .unwrap();
    assert!(
        text.contains(r#""interpreters":["/usr/bin/python3.12","/usr/local/bin/python3.9"]"#),
        "{text}"
    );
    let back: FromAgent = serde_json::from_str(&text).unwrap();
    assert_eq!(
        back,
        ready(vec![
            "/usr/bin/python3.12".into(),
            "/usr/local/bin/python3.9".into()
        ])
    );
}

#[test]
fn timeout_is_optional_and_absent_when_unset() {
    let task: Task = serde_json::from_str(r#"{"module":"raw"}"#).unwrap();
    assert_eq!(task.timeout, None);
    assert!(!serde_json::to_string(&task).unwrap().contains("timeout"));
    let task: Task = serde_json::from_str(r#"{"module":"raw","timeout":5}"#).unwrap();
    assert_eq!(task.timeout, Some(5));
}

#[test]
fn run_batch_round_trips() {
    let mut environment = BTreeMap::new();
    environment.insert("PATH".into(), "/opt/bin".into());
    let msg = ToAgent::RunBatch {
        id: 7,
        tasks: vec![Task {
            module: "ansible.builtin.command".into(),
            args: json!({"_raw_params": "echo hi"})
                .as_object()
                .unwrap()
                .clone(),
            ignore_errors: true,
            timeout: None,
            environment,
            payload: None,
            files: Vec::new(),
            force_python: false,
            job: None,
        }],
    };
    let back: ToAgent = serde_json::from_slice(&serde_json::to_vec(&msg).unwrap()).unwrap();
    assert_eq!(back, msg);
}

#[test]
fn task_args_and_ignore_errors_default() {
    let task: Task = serde_json::from_str(r#"{"module":"raw"}"#).unwrap();
    assert!(task.args.is_empty());
    assert!(!task.ignore_errors);
}

/// A task with nothing to add to the environment does not carry the field at all, and one that
/// does carries it as a map of strings.
#[test]
fn the_environment_is_absent_when_empty_and_round_trips_when_set() {
    let task: Task = serde_json::from_str(r#"{"module":"raw"}"#).unwrap();
    assert!(task.environment.is_empty());
    assert!(
        !serde_json::to_string(&task)
            .unwrap()
            .contains("environment")
    );
    let task: Task = serde_json::from_str(r#"{"module":"raw","environment":{"A":"1"}}"#).unwrap();
    assert_eq!(task.environment["A"], "1");
    assert!(serde_json::to_string(&task).unwrap().contains(r#""A":"1""#));
}

/// A Python task carries the reference of its payload and nothing of the payload itself, and a
/// native task carries no payload field at all.
#[test]
fn a_python_task_carries_a_payload_reference_and_a_native_one_carries_none() {
    let native: Task = serde_json::from_str(r#"{"module":"command"}"#).unwrap();
    assert_eq!(native.payload, None);
    assert!(!serde_json::to_string(&native).unwrap().contains("payload"));

    let task: Task = serde_json::from_str(
        r#"{"module":"ping","payload":{"blob":"abc123","module_fqn":"ansible.modules.ping",
            "profile":"legacy","rlimit_nofile":0,"interpreter":"/usr/bin/python3.12"}}"#,
    )
    .unwrap();
    let payload = task.payload.expect("a python task carries one");
    assert_eq!(payload.blob, "abc123");
    assert_eq!(payload.module_fqn, "ansible.modules.ping");
    assert_eq!(payload.profile, "legacy");
    assert_eq!(payload.rlimit_nofile, 0);
    assert!(payload.extensions.is_empty());
    assert_eq!(payload.interpreter, "/usr/bin/python3.12");
}

/// The three blob messages round-trip under the snake_case tag every other message uses.
#[test]
fn the_blob_messages_round_trip() {
    let put = ToAgent::PutBlob {
        hash: "ff".into(),
        len: 4,
        encoding: BlobEncoding::Deflate,
        staged: false,
    };
    let back: ToAgent = serde_json::from_slice(&serde_json::to_vec(&put).unwrap()).unwrap();
    assert_eq!(back, put);
    assert_eq!(
        serde_json::to_string(&put).unwrap(),
        r#"{"type":"put_blob","hash":"ff","len":4,"encoding":"deflate"}"#,
        "a payload carries no staged field, and no bytes"
    );
    let file = ToAgent::PutBlob {
        hash: "ff".into(),
        len: 4,
        encoding: BlobEncoding::Raw,
        staged: true,
    };
    let back: ToAgent = serde_json::from_slice(&serde_json::to_vec(&file).unwrap()).unwrap();
    assert_eq!(back, file);

    let has = ToAgent::HasBlob { hash: "ff".into() };
    let back: ToAgent = serde_json::from_slice(&serde_json::to_vec(&has).unwrap()).unwrap();
    assert_eq!(back, has);

    let state = FromAgent::BlobState {
        hash: "ff".into(),
        present: true,
    };
    let back: FromAgent = serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
    assert_eq!(back, state);

    let put_file = ToAgent::PutFile {
        hash: "ff".into(),
        path: "/etc/x".into(),
        mode: None,
    };
    assert_eq!(
        serde_json::to_string(&put_file).unwrap(),
        r#"{"type":"put_file","hash":"ff","path":"/etc/x"}"#
    );
    let done = FromAgent::PutFileDone {
        hash: "ff".into(),
        changed: true,
        error: None,
    };
    let back: FromAgent = serde_json::from_slice(&serde_json::to_vec(&done).unwrap()).unwrap();
    assert_eq!(back, done);
}

/// A task that stages a file names the blob and the argument, and a task that stages none
/// carries no `files` field at all.
#[test]
fn a_task_names_the_files_it_stages_and_nothing_else() {
    let bare: Task = serde_json::from_str(r#"{"module":"command"}"#).unwrap();
    assert!(bare.files.is_empty());
    assert!(!serde_json::to_string(&bare).unwrap().contains("files"));
    let task: Task =
        serde_json::from_str(r#"{"module":"copy","files":[{"arg":"src","blob":"ab"}]}"#).unwrap();
    assert_eq!(
        task.files,
        vec![StagedFile {
            arg: "src".into(),
            blob: "ab".into()
        }]
    );
}

/// `skipped` reads by truthiness, as `changed` and `failed` do since PR 181. Measured on
/// ansible-core 2.19.12 when this was filed: `skipped: "yes"` shows `skipping:`.
#[test]
fn skipped_is_read_by_truthiness() {
    let yes: TaskResult = serde_json::from_str(r#"{"skipped":"yes"}"#).unwrap();
    assert!(yes.skipped());
    let zero: TaskResult = serde_json::from_str(r#"{"skipped":0}"#).unwrap();
    assert!(!zero.skipped());
}

/// The version moves with the shape.
#[test]
fn the_protocol_version_is_eight() {
    assert_eq!(PROTOCOL_VERSION, 8);
}

#[test]
fn a_non_zero_rc_counts_as_failed() {
    let ok = TaskResult(
        json!({"rc": 0, "changed": true})
            .as_object()
            .unwrap()
            .clone(),
    );
    let bad = TaskResult(json!({"rc": 2}).as_object().unwrap().clone());
    let flagged = TaskResult(json!({"failed": true}).as_object().unwrap().clone());
    assert!(!ok.failed() && ok.changed());
    assert!(bad.failed() && !bad.changed());
    assert!(flagged.failed());
}

#[test]
fn an_explicit_failed_false_rescues_a_non_zero_rc() {
    let rescued = TaskResult(
        json!({"failed": false, "rc": 2})
            .as_object()
            .unwrap()
            .clone(),
    );
    assert!(!rescued.failed());
}

#[test]
fn batch_outcome_carries_the_index() {
    let text = serde_json::to_string(&FromAgent::BatchDone {
        batch: 1,
        outcome: BatchOutcome::Failed { at: 3 },
    })
    .unwrap();
    assert_eq!(
        text,
        r#"{"type":"batch_done","batch":1,"outcome":{"kind":"failed","at":3}}"#
    );
}

#[test]
fn failed_with_builds_the_ansible_shape() {
    let r = TaskResult::failed_with("boom");
    assert!(r.failed());
    assert_eq!(r.0["msg"], "boom");
}

/// A result that holds an integer of any width crosses the wire with every digit.
#[test]
fn a_result_carries_an_integer_of_any_width_exactly() {
    let wide = "9".repeat(1300);
    let line = format!(
        r#"{{"type":"task_result","batch":1,"index":0,"result":{{"changed":false,"n":{wide},"m":1267650600228229401496703205377,"l":[-{wide}]}}}}"#
    );
    let received: FromAgent = serde_json::from_str(&line).unwrap();
    let sent = serde_json::to_value(&received).unwrap();
    let result = &sent["result"];
    assert_eq!(result["n"].to_string(), wide);
    assert_eq!(result["m"].to_string(), "1267650600228229401496703205377");
    assert_eq!(result["l"][0].to_string(), format!("-{wide}"));
    let original: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(*result, original["result"]);
}
