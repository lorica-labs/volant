// SPDX-License-Identifier: GPL-3.0-or-later
//! `async_status`: what ansible-core 2.19.12's `async_status` action plugin and module answer
//! together, read from the job's status file under `_async_dir`, which the controller adds the
//! way the action plugin does.

use serde_json::{Map, Value};
use volant_protocol::TaskResult;

use super::{Context, Module, Run};

pub const MODULE: Module = Module {
    spec: &volant_protocol::modules::ASYNC_STATUS,
    run,
};

fn run(args: &Map<String, Value>, _: &Context, _: &dyn Fn() -> bool) -> Run {
    Run::Done(match answer(args) {
        Ok(result) => result,
        Err(msg) => {
            let mut failed = TaskResult::failed_with(msg);
            failed.0.insert("changed".into(), Value::Bool(false));
            failed
        }
    })
}

fn answer(args: &Map<String, Value>) -> Result<TaskResult, String> {
    // The action plugin validates `jid` and `mode` alone; `_async_dir` is its own addition.
    let mut unknown: Vec<&str> = args
        .keys()
        .map(String::as_str)
        .filter(|key| !matches!(*key, "jid" | "mode" | "_async_dir"))
        .collect();
    if !unknown.is_empty() {
        unknown.sort_unstable();
        return Err(format!(
            "Unsupported parameters for (async_status) module: {}. Supported parameters include: jid, mode.",
            unknown.join(", ")
        ));
    }
    let jid = match args.get("jid") {
        Some(Value::String(jid)) => jid.clone(),
        Some(Value::Number(n)) => n.to_string(),
        None | Some(Value::Null) => return Err("missing required arguments: jid".into()),
        Some(other) => {
            return Err(format!(
                "argument 'jid' is of type {} and we were unable to convert to str",
                type_name(other)
            ));
        }
    };
    let mode = match args.get("mode") {
        None | Some(Value::Null) => "status",
        Some(Value::String(mode)) if mode == "status" || mode == "cleanup" => mode.as_str(),
        Some(other) => {
            let got = other
                .as_str()
                .map_or_else(|| other.to_string(), str::to_string);
            return Err(format!(
                "value of mode must be one of: status, cleanup, got: {got}"
            ));
        }
    };
    let Some(Value::String(dir)) = args.get("_async_dir") else {
        return Err("missing required arguments: _async_dir".into());
    };
    // A job id names a file in the directory and nothing else: `cleanup` removes what it names,
    // and a path there would remove whatever file a playbook, or a value a host sent back, chose.
    if jid.is_empty() || jid == "." || jid == ".." || jid.contains('/') || jid.contains('\0') {
        return Err(format!("'{jid}' is not a job id"));
    }
    let path = crate::jobs::expand_dir(dir).join(&jid);
    let shown = Value::String(path.to_string_lossy().into_owned());
    // What the action plugin starts from, before the module's answer is merged over it.
    let mut result = Map::new();
    result.insert("started".into(), Value::Bool(false));
    result.insert("finished".into(), Value::Bool(false));
    for key in ["stdout", "stderr"] {
        result.insert(key.into(), Value::String(String::new()));
        result.insert(format!("{key}_lines"), Value::Array(Vec::new()));
    }
    result.insert("ansible_job_id".into(), Value::String(jid.clone()));
    if mode == "cleanup" {
        result.insert("erased".into(), shown.clone());
    } else {
        result.insert("results_file".into(), shown.clone());
        result.insert("started".into(), Value::Bool(true));
    }
    let module = if !path.exists() {
        let mut found = Map::new();
        found.insert("failed".into(), Value::Bool(true));
        found.insert("msg".into(), Value::String("could not find job".into()));
        found.insert("ansible_job_id".into(), Value::String(jid));
        found.insert("started".into(), Value::Bool(true));
        found.insert("finished".into(), Value::Bool(true));
        found
    } else if mode == "cleanup" {
        std::fs::remove_file(&path)
            .map_err(|err| format!("could not remove {}: {err}", path.display()))?;
        let mut erased = Map::new();
        erased.insert("ansible_job_id".into(), Value::String(jid));
        erased.insert("erased".into(), shown);
        erased
    } else {
        status(&path, &jid, shown)
    };
    result.extend(module);
    // The action plugin's own conversion, for files an older wrapper wrote with 0 and 1.
    for key in ["started", "finished"] {
        let truthy = result.get(key).is_some_and(truthy);
        result.insert(key.into(), Value::Bool(truthy));
    }
    result.entry("changed").or_insert(Value::Bool(false));
    Ok(TaskResult(result))
}

/// The module's status answer. A file that cannot be read or parsed is a job still writing it,
/// as the reference reads it: `data` is still `None` when `json.loads` raises.
fn status(path: &std::path::Path, jid: &str, shown: Value) -> Map<String, Value> {
    let parsed = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let Some(Value::Object(mut data)) = parsed else {
        let mut running = Map::new();
        running.insert("results_file".into(), shown);
        running.insert("ansible_job_id".into(), Value::String(jid.to_string()));
        running.insert("started".into(), Value::Bool(true));
        running.insert("finished".into(), Value::Bool(false));
        return running;
    };
    if !data.contains_key("started") {
        data.insert("finished".into(), Value::Bool(true));
        data.insert("ansible_job_id".into(), Value::String(jid.to_string()));
    } else if !data.contains_key("finished") {
        data.insert("finished".into(), Value::Bool(false));
    }
    data
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Bool(_) => "bool",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
        _ => "str",
    }
}

#[cfg(test)]
mod tests {
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
}
