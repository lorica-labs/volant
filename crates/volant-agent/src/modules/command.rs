// SPDX-License-Identifier: GPL-3.0-or-later
//! `command`, `shell` and `raw`: run a program and report rc, stdout and stderr like Ansible.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;

use super::Run;
use crate::clock;

/// Runs one command. `uses_shell` selects `shell` semantics (`sh -c`) over `command`.
pub fn run(args: &Map<String, Value>, uses_shell: bool, cancelled: &dyn Fn() -> bool) -> Run {
    let uses_shell = uses_shell
        || args
            .get("_uses_shell")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let strip_empty_ends = args
        .get("strip_empty_ends")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let chdir = args.get("chdir").and_then(Value::as_str);
    let stdin_data = args.get("stdin").and_then(Value::as_str);

    let raw = args
        .get("_raw_params")
        .or_else(|| args.get("cmd"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let argv: Vec<String> = match args.get("argv").and_then(Value::as_array) {
        Some(list) => list
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        None if uses_shell => vec!["sh".into(), "-c".into(), raw.to_string()],
        None => shlex::split(raw).unwrap_or_default(),
    };
    let display: Value = if uses_shell { json!(raw) } else { json!(argv) };
    if argv.is_empty() || (uses_shell && raw.is_empty()) {
        return Run::Done(TaskResult::failed_with("no command given"));
    }

    if let Some(path) = args.get("creates").and_then(Value::as_str)
        && Path::new(path).exists()
    {
        return Run::Done(skipped(
            display,
            format!("Did not run command since '{path}' exists"),
            format!("skipped, since {path} exists"),
        ));
    }
    if let Some(path) = args.get("removes").and_then(Value::as_str)
        && !Path::new(path).exists()
    {
        return Run::Done(skipped(
            display,
            format!("Did not run command since '{path}' does not exist"),
            format!("skipped, since {path} does not exist"),
        ));
    }

    let start = clock::now();
    let started = Instant::now();
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(if stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = chdir {
        command.current_dir(dir);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return Run::Done(spawn_failure(display, &err)),
    };
    if let (Some(data), Some(mut stdin)) = (stdin_data, child.stdin.take()) {
        let _ = stdin.write_all(data.as_bytes());
    }
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if cancelled() => {
                let _ = child.kill();
                let _ = child.wait();
                return Run::Cancelled;
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(err) => return Run::Done(spawn_failure(display, &err)),
        }
    };
    let rc = status.code().unwrap_or(-1);
    let mut stdout = stdout.join().unwrap_or_default();
    let mut stderr = stderr.join().unwrap_or_default();
    if strip_empty_ends {
        stdout.truncate(stdout.trim_end_matches(['\r', '\n']).len());
        stderr.truncate(stderr.trim_end_matches(['\r', '\n']).len());
    }

    let mut result = Map::new();
    result.insert("cmd".into(), display);
    result.insert("rc".into(), json!(rc));
    result.insert("stdout".into(), json!(stdout));
    result.insert("stderr".into(), json!(stderr));
    result.insert("stdout_lines".into(), lines(&stdout));
    result.insert("stderr_lines".into(), lines(&stderr));
    result.insert("start".into(), json!(start));
    result.insert("end".into(), json!(clock::now()));
    result.insert("delta".into(), json!(clock::delta(started.elapsed())));
    result.insert("changed".into(), json!(true));
    result.insert(
        "msg".into(),
        json!(if rc == 0 { "" } else { "non-zero return code" }),
    );
    if rc != 0 {
        result.insert("failed".into(), json!(true));
    }
    Run::Done(TaskResult(result))
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut pipe) = pipe {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            text = String::from_utf8_lossy(&bytes).into_owned();
        }
        text
    })
}

fn lines(text: &str) -> Value {
    if text.is_empty() {
        return json!([]);
    }
    json!(text.lines().collect::<Vec<_>>())
}

fn skipped(cmd: Value, msg: String, stdout: String) -> TaskResult {
    let mut result = Map::new();
    result.insert("cmd".into(), cmd);
    result.insert("rc".into(), json!(0));
    result.insert("stdout".into(), json!(stdout));
    result.insert("stderr".into(), json!(""));
    result.insert("stdout_lines".into(), lines(&stdout));
    result.insert("stderr_lines".into(), json!([]));
    result.insert("changed".into(), json!(false));
    result.insert("msg".into(), json!(msg));
    TaskResult(result)
}

fn spawn_failure(cmd: Value, err: &std::io::Error) -> TaskResult {
    let (rc, msg) = match err.kind() {
        std::io::ErrorKind::NotFound => (
            2,
            format!(
                "[Errno 2] No such file or directory: {}",
                program_name(&cmd)
            ),
        ),
        _ => (err.raw_os_error().unwrap_or(1), err.to_string()),
    };
    let mut result = Map::new();
    result.insert("cmd".into(), cmd);
    result.insert("rc".into(), json!(rc));
    result.insert("msg".into(), json!(msg));
    result.insert("failed".into(), json!(true));
    TaskResult(result)
}

fn program_name(cmd: &Value) -> String {
    match cmd {
        Value::Array(list) => list
            .first()
            .and_then(Value::as_str)
            .map(|s| format!("b'{s}'"))
            .unwrap_or_default(),
        Value::String(s) => format!("b'{s}'"),
        _ => String::new(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: serde_json::Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn done(run: Run) -> TaskResult {
        match run {
            Run::Done(r) => r,
            Run::Cancelled => panic!("unexpected cancellation"),
        }
    }

    #[test]
    fn free_form_command_reports_stdout_and_rc() {
        let r = done(run(
            &args(json!({"_raw_params": "echo hello world"})),
            false,
            &|| false,
        ));
        assert_eq!(r.0["rc"], 0);
        assert_eq!(r.0["stdout"], "hello world");
        assert_eq!(r.0["stdout_lines"], json!(["hello world"]));
        assert_eq!(r.0["stderr"], "");
        assert_eq!(r.0["cmd"], json!(["echo", "hello", "world"]));
        assert_eq!(r.0["msg"], "");
        assert!(r.changed() && !r.failed());
        assert!(r.0["start"].as_str().unwrap().len() == 26);
        assert!(r.0["delta"].as_str().unwrap().starts_with("0:00:0"));
    }

    #[test]
    fn non_zero_rc_is_a_failure_with_the_ansible_message() {
        let r = done(run(&args(json!({"_raw_params": "false"})), false, &|| {
            false
        }));
        assert_eq!(r.0["rc"], 1);
        assert_eq!(r.0["msg"], "non-zero return code");
        assert!(r.failed());
    }

    #[test]
    fn shell_form_goes_through_sh() {
        let r = done(run(
            &args(json!({"_raw_params": "echo $((6 * 7))"})),
            true,
            &|| false,
        ));
        assert_eq!(r.0["stdout"], "42");
        assert_eq!(r.0["cmd"], "echo $((6 * 7))");
    }

    #[test]
    fn argv_and_cmd_forms_are_accepted() {
        let r = done(run(
            &args(json!({"argv": ["printf", "%s-%s", "a", "b"]})),
            false,
            &|| false,
        ));
        assert_eq!(r.0["stdout"], "a-b");
        let r = done(run(&args(json!({"cmd": "echo cmd-form"})), false, &|| {
            false
        }));
        assert_eq!(r.0["stdout"], "cmd-form");
    }

    #[test]
    fn creates_skips_when_the_path_exists() {
        let r = done(run(
            &args(json!({"_raw_params": "echo never", "creates": "/"})),
            false,
            &|| false,
        ));
        assert_eq!(r.0["rc"], 0);
        assert_eq!(r.0["msg"], "Did not run command since '/' exists");
        assert_eq!(r.0["stdout"], "skipped, since / exists");
        assert!(!r.changed() && !r.failed());
    }

    #[test]
    fn removes_skips_when_the_path_is_absent() {
        let r = done(run(
            &args(json!({"_raw_params": "echo never", "removes": "/definitely/not/here"})),
            false,
            &|| false,
        ));
        assert_eq!(
            r.0["msg"],
            "Did not run command since '/definitely/not/here' does not exist"
        );
        assert!(!r.changed());
    }

    #[test]
    fn chdir_changes_the_working_directory() {
        let dir = std::env::temp_dir().join(format!("volant-chdir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker"), b"").unwrap();
        let r = done(run(
            &args(json!({"_raw_params": "ls", "chdir": dir.to_str().unwrap()})),
            false,
            &|| false,
        ));
        assert_eq!(r.0["stdout"], "marker");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stdin_is_fed_to_the_program() {
        let r = done(run(
            &args(json!({"_raw_params": "cat", "stdin": "from stdin"})),
            false,
            &|| false,
        ));
        assert_eq!(r.0["stdout"], "from stdin");
    }

    #[test]
    fn a_missing_program_reports_errno_2() {
        let r = done(run(
            &args(json!({"_raw_params": "volant-no-such-program"})),
            false,
            &|| false,
        ));
        assert_eq!(r.0["rc"], 2);
        assert!(
            r.0["msg"]
                .as_str()
                .unwrap()
                .starts_with("[Errno 2] No such file or directory")
        );
        assert!(r.failed());
    }

    #[test]
    fn cancellation_kills_the_program() {
        let started = std::time::Instant::now();
        let run = run(&args(json!({"_raw_params": "sleep 30"})), false, &|| true);
        assert!(matches!(run, Run::Cancelled));
        assert!(started.elapsed().as_secs() < 5);
    }

    #[test]
    fn empty_command_is_an_error() {
        let r = done(run(&args(json!({"_raw_params": "   "})), false, &|| false));
        assert!(r.failed());
        assert_eq!(r.0["msg"], "no command given");
    }
}
