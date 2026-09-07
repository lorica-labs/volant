// SPDX-License-Identifier: GPL-3.0-or-later
//! `command`, `shell` and `raw`: run a program and report rc, stdout and stderr like Ansible.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;
use volant_protocol::modules::COMMAND;

use super::{Module, Run};
use crate::clock;

pub const MODULE: Module = Module {
    spec: &COMMAND,
    run: run_command,
};

fn run_command(
    args: &Map<String, Value>,
    timeout: Option<Duration>,
    cancelled: &dyn Fn() -> bool,
) -> Run {
    execute(args, false, timeout, cancelled)
}

/// Runs one command. `uses_shell` selects `shell` semantics (`sh -c`) over `command`.
pub(crate) fn execute(
    args: &Map<String, Value>,
    uses_shell: bool,
    timeout: Option<Duration>,
    cancelled: &dyn Fn() -> bool,
) -> Run {
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
    let stdin_data = args
        .get("stdin")
        .and_then(Value::as_str)
        .map(str::to_string);

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
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return Run::Done(spawn_failure(display, chdir, &err)),
    };
    // Drain stdout and stderr on their own threads before writing stdin: the child may
    // start writing output while it is still reading input, and if nobody is reading
    // that output yet, both sides block once a pipe buffer fills up. Writing stdin from
    // its own thread, joined below, keeps that write off the thread that has to reach
    // the cancellation check further down.
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let stdin_writer = child.stdin.take().map(|mut stdin| {
        thread::spawn(move || {
            if let Some(data) = stdin_data {
                let _ = stdin.write_all(data.as_bytes());
            }
        })
    });

    let deadline = timeout.map(|t| Instant::now() + t);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if cancelled() => {
                kill_and_wait(&mut child);
                return Run::Cancelled;
            }
            Ok(None) if deadline.is_some_and(|d| Instant::now() >= d) => {
                kill_and_wait(&mut child);
                let seconds = timeout.map(|t| t.as_secs()).unwrap_or_default();
                return Run::Done(timed_out(seconds));
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(err) => return Run::Done(spawn_failure(display, chdir, &err)),
        }
    };
    let rc = exit_code(&status);
    let mut stdout = stdout.join().unwrap_or_default();
    let mut stderr = stderr.join().unwrap_or_default();
    if let Some(stdin_writer) = stdin_writer {
        let _ = stdin_writer.join();
    }
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

/// Kills the child's process group and waits for it to actually exit, so the caller never
/// races the kernel's own cleanup. Cancellation and the timeout path both need this.
fn kill_and_wait(child: &mut std::process::Child) {
    kill_group(child);
    let _ = child.wait();
}

/// Kills the child's whole process group, so pipelines and backgrounded grandchildren go too.
fn kill_group(child: &std::process::Child) {
    #[cfg(unix)]
    {
        // The child was started with `process_group(0)`, so its pid is its pgid.
        let pgid = child.id() as libc::pid_t;
        // Negative pid targets the group. SIGKILL: the module was already asked to stop.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child;
    }
}

/// Ansible reports a signal death as the negative signal number, like Python's `Popen`.
fn exit_code(status: &std::process::ExitStatus) -> i64 {
    if let Some(code) = status.code() {
        return i64::from(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return -i64::from(signal);
        }
    }
    -1
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
    result.insert("skipped".into(), json!(true));
    result.insert("msg".into(), json!(msg));
    TaskResult(result)
}

/// Matches `ansible-core`'s shape for the `timeout:` task keyword, not the module's own
/// timeout mechanism: no `cmd`, no `rc`/`stdout`/`stderr` (the reference drops those too,
/// even when the command had already produced output), and no `timedout.frame` (an
/// Ansible-internal traceback hint we have nothing to reproduce).
fn timed_out(seconds: u64) -> TaskResult {
    let mut result = Map::new();
    result.insert("changed".into(), json!(false));
    result.insert("failed".into(), json!(true));
    result.insert(
        "msg".into(),
        json!(format!("Task failed: Timed out after {seconds} second(s).")),
    );
    result.insert("timedout".into(), json!({"period": seconds}));
    TaskResult(result)
}

fn spawn_failure(cmd: Value, chdir: Option<&str>, err: &std::io::Error) -> TaskResult {
    let (rc, msg) = match err.kind() {
        std::io::ErrorKind::NotFound => {
            let missing = match chdir {
                Some(dir) if !Path::new(dir).exists() => format!("b'{dir}'"),
                _ => program_name(&cmd),
            };
            (2, format!("[Errno 2] No such file or directory: {missing}"))
        }
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
        let r = done(execute(
            &args(json!({"_raw_params": "echo hello world"})),
            false,
            None,
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
        let r = done(execute(
            &args(json!({"_raw_params": "false"})),
            false,
            None,
            &|| false,
        ));
        assert_eq!(r.0["rc"], 1);
        assert_eq!(r.0["msg"], "non-zero return code");
        assert!(r.failed());
    }

    #[test]
    fn shell_form_goes_through_sh() {
        let r = done(execute(
            &args(json!({"_raw_params": "echo $((6 * 7))"})),
            true,
            None,
            &|| false,
        ));
        assert_eq!(r.0["stdout"], "42");
        assert_eq!(r.0["cmd"], "echo $((6 * 7))");
    }

    #[test]
    fn argv_and_cmd_forms_are_accepted() {
        let r = done(execute(
            &args(json!({"argv": ["printf", "%s-%s", "a", "b"]})),
            false,
            None,
            &|| false,
        ));
        assert_eq!(r.0["stdout"], "a-b");
        let r = done(execute(
            &args(json!({"cmd": "echo cmd-form"})),
            false,
            None,
            &|| false,
        ));
        assert_eq!(r.0["stdout"], "cmd-form");
    }

    #[test]
    fn creates_skips_when_the_path_exists() {
        let r = done(execute(
            &args(json!({"_raw_params": "echo never", "creates": "/"})),
            false,
            None,
            &|| false,
        ));
        assert_eq!(r.0["rc"], 0);
        assert_eq!(r.0["msg"], "Did not run command since '/' exists");
        assert_eq!(r.0["stdout"], "skipped, since / exists");
        assert!(!r.changed() && !r.failed());
        assert!(r.skipped());
    }

    #[test]
    fn removes_skips_when_the_path_is_absent() {
        let r = done(execute(
            &args(json!({"_raw_params": "echo never", "removes": "/definitely/not/here"})),
            false,
            None,
            &|| false,
        ));
        assert_eq!(
            r.0["msg"],
            "Did not run command since '/definitely/not/here' does not exist"
        );
        assert!(!r.changed());
        assert!(r.skipped());
    }

    #[test]
    fn chdir_changes_the_working_directory() {
        let dir = std::env::temp_dir().join(format!("volant-chdir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker"), b"").unwrap();
        let r = done(execute(
            &args(json!({"_raw_params": "ls", "chdir": dir.to_str().unwrap()})),
            false,
            None,
            &|| false,
        ));
        assert_eq!(r.0["stdout"], "marker");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stdin_is_fed_to_the_program() {
        let r = done(execute(
            &args(json!({"_raw_params": "cat", "stdin": "from stdin"})),
            false,
            None,
            &|| false,
        ));
        assert_eq!(r.0["stdout"], "from stdin");
    }

    #[test]
    fn large_stdin_does_not_deadlock() {
        let payload: String = "abcdefghijklmnopqrstuvwxyz0123456789\n"
            .chars()
            .cycle()
            .take(64 * 1_048_576)
            .collect();
        let r = done(execute(
            &args(json!({"_raw_params": "cat", "stdin": payload.clone()})),
            true,
            None,
            &|| false,
        ));
        assert_eq!(r.0["rc"], 0);
        assert_eq!(r.0["stdout"], payload);
    }

    #[test]
    fn a_missing_program_reports_errno_2() {
        let r = done(execute(
            &args(json!({"_raw_params": "volant-no-such-program"})),
            false,
            None,
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
    fn a_missing_chdir_reports_the_directory_not_the_program() {
        let r = done(execute(
            &args(json!({
                "_raw_params": "echo never",
                "chdir": "/definitely/not/here",
            })),
            false,
            None,
            &|| false,
        ));
        assert_eq!(r.0["rc"], 2);
        assert_eq!(
            r.0["msg"],
            "[Errno 2] No such file or directory: b'/definitely/not/here'"
        );
        assert!(r.failed());
    }

    #[test]
    fn cancellation_kills_the_program() {
        let started = std::time::Instant::now();
        let run = execute(
            &args(json!({"_raw_params": "sleep 30"})),
            false,
            None,
            &|| true,
        );
        assert!(matches!(run, Run::Cancelled));
        assert!(started.elapsed().as_secs() < 5);
    }

    #[test]
    fn empty_command_is_an_error() {
        let r = done(execute(
            &args(json!({"_raw_params": "   "})),
            false,
            None,
            &|| false,
        ));
        assert!(r.failed());
        assert_eq!(r.0["msg"], "no command given");
    }

    #[test]
    fn a_timeout_kills_the_program_and_reports_ansible_shape() {
        let started = std::time::Instant::now();
        let r = done(execute(
            &args(json!({"_raw_params": "sleep 30"})),
            false,
            Some(std::time::Duration::from_secs(1)),
            &|| false,
        ));
        assert!(started.elapsed().as_secs() < 5);
        assert!(r.failed());
        assert!(!r.changed());
        assert_eq!(r.0["msg"], "Task failed: Timed out after 1 second(s).");
        assert_eq!(r.0["timedout"], json!({"period": 1}));
        assert!(r.0.get("rc").is_none());
        assert!(r.0.get("stdout").is_none());
        assert!(r.0.get("cmd").is_none());
    }

    #[test]
    fn cancellation_kills_the_whole_process_group() {
        // `sh -c` forks a grandchild; killing only the shell would leave `sleep` running.
        // The marker has to live inside `sleep`'s duration argument: GNU `sleep` rejects a
        // second, non-numeric argument outright, and a lone trailing simple command would be
        // exec'd (replacing the shell) rather than forked, leaving no grandchild to kill.
        let marker = format!("30.{}", std::process::id());
        let alive = |marker: &str| {
            std::process::Command::new("pgrep")
                .args(["-f", marker])
                .output()
                .is_ok_and(|o| !o.stdout.is_empty())
        };
        // Cancel only once the grandchild is actually running. Cancelling straight away kills the
        // shell before it forks, so nothing would survive however narrow the kill was.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let run = execute(
            &args(json!({"_raw_params": format!("sleep {marker} & wait")})),
            true,
            None,
            &|| alive(&marker) || std::time::Instant::now() > deadline,
        );
        assert!(matches!(run, Run::Cancelled));
        std::thread::sleep(std::time::Duration::from_millis(200));
        let survivors = std::process::Command::new("pgrep")
            .args(["-f", &marker])
            .output()
            .unwrap();
        assert!(
            survivors.stdout.is_empty(),
            "grandchild survived: {}",
            String::from_utf8_lossy(&survivors.stdout)
        );
    }

    #[test]
    fn a_program_killed_by_a_signal_reports_the_negative_signal_number() {
        let r = done(execute(
            &args(json!({"_raw_params": "sh -c 'kill -TERM $$'"})),
            false,
            None,
            &|| false,
        ));
        assert_eq!(r.0["rc"], -15);
        assert!(r.failed());
    }
}
