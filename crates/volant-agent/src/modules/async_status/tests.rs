// SPDX-License-Identifier: GPL-3.0-or-later

use rstest::rstest;
use serde_json::json;

use super::*;

fn ask(dir: &std::path::Path, args: Value) -> Map<String, Value> {
    let mut args = args.as_object().unwrap().clone();
    args.insert("_async_dir".into(), json!(dir.to_str().unwrap()));
    match run(&args, &Context::default(), &|| false) {
        Run::Done(result) => result.0,
        Run::Cancelled => panic!("never cancelled"),
    }
}

/// Each status file shape the reference's wrapper leaves, read as its module reads it.
#[rstest]
#[case::running(Some(r#"{"started": true, "finished": false, "ansible_job_id": "j1.2"}"#), json!({"started": true, "finished": false, "changed": false}))]
#[case::finished(Some(r#"{"rc": 0, "changed": true}"#), json!({"started": true, "finished": true, "changed": true, "rc": 0}))]
#[case::corrupt(Some(r#"{"rc": 0, "chan"#), json!({"started": true, "finished": false, "changed": false}))]
#[case::missing(None, json!({"started": true, "finished": true, "failed": true, "msg": "could not find job"}))]
fn a_status_file_reads_as_the_reference_reads_it(
    #[case] file: Option<&str>,
    #[case] wanted: Value,
) {
    let dir = tempfile::tempdir().unwrap();
    if let Some(text) = file {
        std::fs::write(dir.path().join("j1.2"), text).unwrap();
    }
    let got = ask(dir.path(), json!({"jid": "j1.2"}));
    for (key, value) in wanted.as_object().unwrap() {
        assert_eq!(got.get(key), Some(value), "{key} in {got:?}");
    }
    assert_eq!(got["ansible_job_id"], json!("j1.2"));
    let file = dir.path().join("j1.2");
    assert_eq!(got["results_file"], json!(file.to_str().unwrap()));
}

#[test]
fn cleanup_removes_the_job_s_file_and_only_that() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("j1.2"), "{}").unwrap();
    std::fs::write(dir.path().join("keep"), "{}").unwrap();
    let got = ask(dir.path(), json!({"jid": "j1.2", "mode": "cleanup"}));
    assert_eq!(got["started"], json!(false));
    assert_eq!(got["changed"], json!(false));
    assert!(!dir.path().join("j1.2").exists());
    assert!(dir.path().join("keep").exists());
    for jid in ["../keep", "..", "a/b", ""] {
        let got = ask(dir.path(), json!({"jid": jid, "mode": "cleanup"}));
        assert_eq!(got["failed"], json!(true), "{jid}");
    }
    assert!(dir.path().join("keep").exists());
}

#[rstest]
#[case::no_jid(json!({}), "missing required arguments: jid")]
#[case::bad_mode(json!({"jid": "j1.2", "mode": "kill"}), "value of mode must be one of: status, cleanup, got: kill")]
#[case::unknown(json!({"jid": "j1.2", "pid": 3}), "Unsupported parameters for (async_status) module: pid. Supported parameters include: jid, mode.")]
fn a_bad_argument_fails_with_the_reference_s_words(#[case] args: Value, #[case] msg: &str) {
    let dir = tempfile::tempdir().unwrap();
    let got = ask(dir.path(), args);
    assert_eq!(got["failed"], json!(true));
    assert_eq!(got["msg"], json!(msg));
}
