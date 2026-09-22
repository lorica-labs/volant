// SPDX-License-Identifier: GPL-3.0-or-later
//! `command`, `shell` and `raw`: run a program and report rc, stdout and stderr like Ansible.

use std::cell::Cell;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;
use volant_protocol::modules::{COMMAND, arg_bool};

use super::{Context, Module, Run, glob};
use crate::clock;

pub const MODULE: Module = Module {
    spec: &COMMAND,
    run: run_command,
};

fn run_command(args: &Map<String, Value>, ctx: &Context, cancelled: &dyn Fn() -> bool) -> Run {
    execute(args, false, ctx, cancelled)
}

/// Runs one command. `uses_shell` selects `shell` semantics (`sh -c`) over `command`.
pub(crate) fn execute(
    args: &Map<String, Value>,
    uses_shell: bool,
    ctx: &Context,
    cancelled: &dyn Fn() -> bool,
) -> Run {
    let timeout = ctx.timeout;
    // Every boolean here is read the way the reference reads one, spellings and all: `"false"`,
    // `"no"` and `0` turn the argument off there, and `Value::as_bool` sees none of them.
    let uses_shell = uses_shell || args.get("_uses_shell").and_then(arg_bool).unwrap_or(false);
    let strip_empty_ends = args
        .get("strip_empty_ends")
        .and_then(arg_bool)
        .unwrap_or(true);
    let chdir = args.get("chdir").and_then(Value::as_str);
    // Measured on ansible-core 2.19.12: the newline is appended without looking at what the value
    // already ends with, so a value that ends in one gets a second.
    let stdin_data = args.get("stdin").and_then(Value::as_str).map(|s| {
        if args
            .get("stdin_add_newline")
            .and_then(arg_bool)
            .unwrap_or(true)
        {
            format!("{s}\n")
        } else {
            s.to_string()
        }
    });

    let raw = args
        .get("_raw_params")
        .or_else(|| args.get("cmd"))
        .and_then(Value::as_str)
        .map_or("", str::trim);
    // Measured on ansible-core 2.19.12: an `argv` on `shell` runs the way it runs on `command` -
    // the list is the process and no shell is started for it, and the task reports the list as
    // its `cmd`. A `shell:` written as a mapping carrying only `argv` used to be refused here as
    // "no command given", against a reference that runs it.
    let from_argv = args.get("argv").and_then(Value::as_array);
    let argv: Vec<String> = match from_argv {
        Some(list) => list
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        // `executable` is the shell the line is handed to, and selecting it is the whole point of
        // writing the argument: `sh` is a fallback, not the answer to every task.
        None if uses_shell && !raw.is_empty() => vec![
            args.get("executable")
                .and_then(Value::as_str)
                .unwrap_or("sh")
                .to_string(),
            "-c".into(),
            raw.to_string(),
        ],
        None if uses_shell => Vec::new(),
        None => shlex::split(raw).unwrap_or_default(),
    };
    let display: Value = if uses_shell && from_argv.is_none() {
        json!(raw)
    } else {
        json!(argv)
    };
    if argv.is_empty() {
        return Run::Done(TaskResult::failed_with("no command given"));
    }

    // The reference module changes directory before it reads either guard, so a `chdir` that
    // cannot be entered fails the task here rather than being read as a guard that found
    // nothing under a base that cannot exist. Without this, a typo in `chdir` next to a
    // `removes` turns a cleanup that never happened into a green skip.
    // `metadata` alone is not `chdir`: it succeeds on a regular file, where `chdir` raises
    // `NotADirectoryError`. A `chdir` typo that lands on a file next to a `removes` would find
    // nothing under `file/…` and report the cleanup as a green skip.
    //
    // A directory the agent cannot search is the one case still left open: `chdir` would fail
    // with `EACCES` and this passes it through to `spawn`, which reports it. Checking it here
    // would mean reading the directory, and a `--x` directory is searchable without being
    // readable - refusing that would be the worse mistake.
    if let Some(dir) = chdir {
        match std::fs::metadata(dir) {
            Err(err) => return Run::Done(spawn_failure(display, chdir, &err)),
            Ok(meta) if !meta.is_dir() => {
                let err = std::io::Error::from_raw_os_error(libc::ENOTDIR);
                return Run::Done(spawn_failure(display, chdir, &err));
            }
            Ok(_) => {}
        }
    }
    let guard_base = chdir.map(Path::new);
    // An empty value applies no guard at all, which is what the reference's own `if creates:`
    // does with it: a template that rendered to nothing must not decide anything.
    if let Some(path) = args.get("creates").and_then(Value::as_str)
        && !path.is_empty()
        && glob::matches_any(guard_base, path)
    {
        return Run::Done(skipped(
            display,
            format!("Did not run command since '{path}' exists"),
            format!("skipped, since {path} exists"),
        ));
    }
    if let Some(path) = args.get("removes").and_then(Value::as_str)
        && !path.is_empty()
        && !glob::matches_any(guard_base, path)
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
    // Added to what the agent inherited rather than replacing it, which is what Ansible's
    // `environment` does: a task setting `PATH` keeps `HOME`, and `sudo`'s own `env_reset` is
    // already behind us - the escalated agent is the process this one is forked from.
    command.envs(&ctx.environment);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return Run::Done(spawn_failure(display, chdir, &err)),
    };
    // Read before the handle moves into the reaping thread below. The child was started with
    // `process_group(0)`, so this is also the group's id, and the group keeps that id for as long
    // as it has members: killing by it still reaches descendants after the leader has been reaped.
    // Once the group is empty the id can in principle be handed to an unrelated group, and the
    // kills below would then reach it; that needs a full pid wraparound between the reap and the
    // kill a few microseconds later, and the window is accepted rather than guarded.
    let pid = child.id();
    // Drain stdout and stderr on their own threads before writing stdin: the child may
    // start writing output while it is still reading input, and if nobody is reading
    // that output yet, both sides block once a pipe buffer fills up. Writing stdin from
    // its own thread, waited on below under the same deadline as the readers, keeps that
    // write off the thread that has to reach the cancellation check further down.
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    // A channel rather than a `JoinHandle`, for the same reason as the readers: a descendant that
    // holds the read end open without reading blocks this write once the pipe buffer is full, and
    // that wait has to be abandonable. It sends the readers' payload type so one bounded wait
    // serves all three.
    let stdin_written = child.stdin.take().map(|mut stdin| {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            if let Some(data) = stdin_data {
                let _ = stdin.write_all(data.as_bytes());
            }
            let _ = tx.send(String::new());
        });
        rx
    });

    // The end of the process is signalled rather than polled, so a command that returns at once
    // is noticed at once instead of paying a poll interval.
    let (done_tx, done) = mpsc::channel();
    {
        // `wait` needs the child, so the reaping thread owns the handle and hands the status back.
        let mut child = child;
        thread::spawn(move || {
            let _ = done_tx.send(child.wait());
        });
    }

    // `cancelled` has a side effect: it is a `try_recv` on the control channel, so it pops the
    // cancellation message and answers `true` exactly **once**. The wait below and each of the two
    // readers ask it, and every one of them has to get the same answer, so the first `true` is
    // latched here and the rest of this function asks the latch.
    let latch = {
        let seen = Cell::new(false);
        // `cancelled` here is still the argument: the binding below only shadows it afterwards.
        move || {
            if !seen.get() && cancelled() {
                seen.set(true);
            }
            seen.get()
        }
    };
    let cancelled: &dyn Fn() -> bool = &latch;

    let deadline = timeout.map(|t| Instant::now() + t);
    let status = loop {
        let left = match deadline {
            // `checked_duration_since` gives None once the deadline is behind us, which is the
            // expiry: a plain subtraction would panic there.
            Some(d) => match d.checked_duration_since(Instant::now()) {
                None => {
                    kill_group(pid);
                    let seconds = timeout.map(|t| t.as_secs()).unwrap_or_default();
                    let _ = done.recv();
                    return Run::Done(TaskResult::timed_out(seconds));
                }
                Some(left) => left.min(CANCEL_POLL),
            },
            None => CANCEL_POLL,
        };
        match done.recv_timeout(left) {
            Ok(Ok(status)) => break status,
            // `wait` failed, so the process's fate is as unknown as it is on the disconnected
            // arm below: kill the group rather than leave it running behind a failed task.
            Ok(Err(err)) => {
                kill_group(pid);
                return Run::Done(spawn_failure(display, chdir, &err));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if cancelled() {
                    kill_group(pid);
                    let _ = done.recv();
                    return Run::Cancelled;
                }
            }
            // The waiting thread died without sending: the process's fate is unknown, which is a
            // failed task rather than a silent success.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                kill_group(pid);
                return Run::Done(TaskResult::failed_with(
                    "the agent lost track of the command",
                ));
            }
        }
    };
    let rc = exit_code(status);
    // All three pipes wait under what is left of the same deadline. A descendant that outlived the
    // process it was forked from still holds them, and waiting for it here is what used to carry a
    // task well past its timeout and then report success. Only the endings below kill the group,
    // and the success path is not one of them: a write left blocked there is never unblocked by an
    // `EPIPE` that nothing sends. The three calls run before the match, and the `else` asks a
    // fourth time, which is what the latch above is for.
    //
    // A write the deadline cut short reports the timeout, never a success over a truncated stdin -
    // which is what the reference reports for the same command.
    let (Some(mut stdout), Some(mut stderr), Some(_)) = (
        collect(&stdout, deadline, cancelled),
        collect(&stderr, deadline, cancelled),
        // Nothing to wait for when the command was given no `stdin:`: its input is `/dev/null`.
        stdin_written
            .as_ref()
            .map_or(Some(String::new()), |rx| collect(rx, deadline, cancelled)),
    ) else {
        kill_group(pid);
        if cancelled() {
            return Run::Cancelled;
        }
        let seconds = timeout.map(|t| t.as_secs()).unwrap_or_default();
        return Run::Done(TaskResult::timed_out(seconds));
    };
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

/// How often the wait wakes up to look at the cancellation flag. The process's own end no longer
/// costs a wait: the reaping thread signals it. Only a cancellation waits for a tick, and only
/// while something is still running.
pub(crate) const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Waits for one pipe thread under what is left of the task's deadline, waking on `CANCEL_POLL` to
/// look at the cancellation flag -- a pipe a descendant holds open has to be abandonable both ways,
/// and a task without a `timeout:` has only the flag. `None` means the wait ended without the
/// thread: the caller kills the group, which closes the pipes and ends the thread, and reports
/// whichever of the two endings applies. What a reader had read is lost, which is what a timed-out
/// or cancelled task reports anyway; the writer sends an empty string and has nothing to lose.
///
/// A thread that disconnected without sending is read as empty output, the way joining a panicked
/// reader was: turning it into an expiry would report a deadline that was never reached.
fn collect(
    rx: &mpsc::Receiver<String>,
    deadline: Option<Instant>,
    cancelled: &dyn Fn() -> bool,
) -> Option<String> {
    loop {
        let left = match deadline {
            Some(d) => match d.checked_duration_since(Instant::now()) {
                Some(left) => left.min(CANCEL_POLL),
                // Past the deadline, but the reader may already have sent: the wait loop can
                // break with most of a `CANCEL_POLL` of the deadline spent, so take what is in
                // the channel rather than report an expiry over output that is complete.
                None => return rx.try_recv().ok(),
            },
            None => CANCEL_POLL,
        };
        match rx.recv_timeout(left) {
            Ok(text) => return Some(text),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Some(String::new()),
            Err(mpsc::RecvTimeoutError::Timeout) if cancelled() => return None,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

/// Kills a whole process group by its id, so pipelines and backgrounded grandchildren go too.
pub(crate) fn kill_group(pgid: u32) {
    #[cfg(unix)]
    {
        // Negative pid targets the group. SIGKILL: the module was already asked to stop.
        unsafe {
            libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
    }
}

/// Ansible reports a signal death as the negative signal number, like Python's `Popen`.
fn exit_code(status: std::process::ExitStatus) -> i64 {
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

/// Drains a pipe on its own thread and sends what it read. A channel rather than a join handle,
/// because the deadline has to be able to stop waiting for a reader that a descendant is keeping
/// open, and `JoinHandle` has no timed join.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut pipe) = pipe {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            text = String::from_utf8_lossy(&bytes).into_owned();
        }
        let _ = tx.send(text);
    });
    rx
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

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn done(run: Run) -> TaskResult {
        match run {
            Run::Done(r) => r,
            Run::Cancelled => panic!("unexpected cancellation"),
        }
    }

    /// `stdin_add_newline` decides whether the value written to the child ends in a newline, and
    /// its default is to add one. Measured on ansible-core 2.19.12 with `od -c`: the newline is
    /// appended without looking at what the value already ends with, so `"ab\n"` reaches the
    /// child as four bytes under the default and three with the argument off.
    ///
    /// What would make this red: the argument accepted and dropped, which is what this release
    /// did -- every command reading standard input got one byte less than the reference gave it,
    /// and a reader waiting for a line never saw one. Or the value read with `Value::as_bool`,
    /// which sees a YAML boolean and nothing else: `"false"` then falls back to the default and
    /// writes the byte the task asked it not to, measured at 2 bytes against the reference's 2
    /// and this release's 3.
    #[test]
    fn stdin_add_newline_decides_the_last_byte_written() {
        let bytes = |stdin: Value, add: Option<Value>| {
            let mut a = args(json!({"_raw_params": "wc -c", "stdin": stdin}));
            if let Some(add) = add {
                a.insert("stdin_add_newline".into(), add);
            }
            done(execute(&a, false, &Context::default(), &|| false)).0["stdout"]
                .as_str()
                .unwrap()
                .trim()
                .to_string()
        };
        assert_eq!(bytes(json!("ab"), None), "3");
        assert_eq!(bytes(json!("ab\n"), None), "4", "appended unconditionally");
        assert_eq!(bytes(json!("ab"), Some(json!(false))), "2");
        assert_eq!(bytes(json!("ab\n"), Some(json!(false))), "3");
        for off in [json!("false"), json!("no"), json!("Off"), json!(0)] {
            assert_eq!(bytes(json!("ab"), Some(off.clone())), "2", "{off}");
        }
        assert_eq!(bytes(json!("ab"), Some(json!("yes"))), "3");
    }

    /// An `argv` on the shell path runs the list itself, which is what the reference does with
    /// it: `shell:` written as a mapping carrying `argv` and no command line prints its output
    /// there, and reports the list as the task's `cmd`.
    ///
    /// What would make this red: the empty-command guard reading the free-form line while an
    /// `argv` sits next to it, which failed the task with "no command given" against a reference
    /// that ran it. A `shell` with neither still has nothing to run, and still says so.
    #[test]
    fn an_argv_runs_on_the_shell_path_and_nothing_still_fails() {
        let r = done(execute(
            &args(json!({"argv": ["echo", "hi"]})),
            true,
            &Context::default(),
            &|| false,
        ));
        assert_eq!(
            r.0.get("stdout").and_then(Value::as_str).map(str::trim),
            Some("hi"),
            "the list never ran: {:?}",
            r.0
        );
        assert_eq!(r.0["cmd"], json!(["echo", "hi"]));
        let empty = done(execute(
            &args(json!({"_raw_params": "  "})),
            true,
            &Context::default(),
            &|| false,
        ));
        assert_eq!(empty.0["msg"], json!("no command given"));
    }

    #[test]
    fn free_form_command_reports_stdout_and_rc() {
        let r = done(execute(
            &args(json!({"_raw_params": "echo hello world"})),
            false,
            &Context::default(),
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
            &Context::default(),
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
            &Context::default(),
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
            &Context::default(),
            &|| false,
        ));
        assert_eq!(r.0["stdout"], "a-b");
        let r = done(execute(
            &args(json!({"cmd": "echo cmd-form"})),
            false,
            &Context::default(),
            &|| false,
        ));
        assert_eq!(r.0["stdout"], "cmd-form");
    }

    #[test]
    fn creates_skips_when_the_path_exists() {
        let r = done(execute(
            &args(json!({"_raw_params": "echo never", "creates": "/"})),
            false,
            &Context::default(),
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
            &Context::default(),
            &|| false,
        ));
        assert_eq!(
            r.0["msg"],
            "Did not run command since '/definitely/not/here' does not exist"
        );
        assert!(!r.changed());
        assert!(r.skipped());
    }

    /// Measurement (o): a relative `creates` resolves under `chdir`, and the message keeps the
    /// path exactly as the playbook wrote it, not prefixed by the `chdir` that resolved it.
    ///
    /// What would make this red: `Path::new(path).exists()` reading `marker` from the agent's
    /// own directory instead of from `chdir`, which finds nothing there and lets the command run.
    #[test]
    fn a_relative_creates_resolves_under_chdir() {
        let dir = std::env::temp_dir().join(format!("volant-guard-chdir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker"), b"").unwrap();
        let r = done(execute(
            &args(json!({
                "_raw_params": "echo never",
                "chdir": dir.to_str().unwrap(),
                "creates": "marker",
            })),
            false,
            &Context::default(),
            &|| false,
        ));
        assert_eq!(r.0["msg"], "Did not run command since 'marker' exists");
        assert_eq!(r.0["stdout"], "skipped, since marker exists");
        assert!(!r.changed());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Measurement (o) for the other guard: `removes` resolves under `chdir` too, so a cleanup
    /// whose target is there runs instead of being skipped.
    ///
    /// What would make this red: reading `cache` from the agent's own directory, where it does
    /// not exist, so `!matches_any` holds and the cleanup is skipped with the play still green.
    #[test]
    fn a_relative_removes_resolves_under_chdir() {
        let dir = std::env::temp_dir().join(format!("volant-guard-removes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cache"), b"").unwrap();
        let r = done(execute(
            &args(json!({
                "_raw_params": "echo ran",
                "chdir": dir.to_str().unwrap(),
                "removes": "cache",
            })),
            false,
            &Context::default(),
            &|| false,
        ));
        assert!(
            !r.skipped(),
            "the cleanup was skipped although 'cache' is there"
        );
        assert_eq!(r.0["stdout"], "ran");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A `chdir` that cannot be entered fails the task, the way the reference's own
    /// `os.chdir` does before it ever looks at a guard.
    ///
    /// What would make this red: evaluating `removes` first. Nothing matches under a base that
    /// cannot exist, so `!matches_any` holds, the task is skipped with `rc: 0` and a play that
    /// never ran its cleanup stays green.
    #[test]
    fn a_missing_chdir_fails_before_the_guards() {
        let r = done(execute(
            &args(json!({
                "_raw_params": "rm -rf cache",
                "chdir": "/definitely/not/here",
                "removes": "cache",
            })),
            false,
            &Context::default(),
            &|| false,
        ));
        assert!(!r.skipped(), "a bad chdir was read as a guard decision");
        assert!(r.failed());
        assert_eq!(r.0["rc"], 2);
        assert_eq!(
            r.0["msg"],
            "[Errno 2] No such file or directory: b'/definitely/not/here'"
        );
    }

    /// A `chdir` that names a regular file fails the task too. `chdir` wants a directory, and
    /// the reference's `os.chdir` raises `NotADirectoryError` on anything else.
    ///
    /// What would make this red: testing only that the path exists. A path typo that lands on a
    /// file passes `metadata`, the guards then walk `file/cache`, nothing matches, and a cleanup
    /// that never ran reports `rc: 0` - the same green skip the test above forbids, through the
    /// other door.
    #[test]
    fn a_chdir_that_is_not_a_directory_fails_before_the_guards() {
        let dir = std::env::temp_dir().join(format!("volant-chdir-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.yml");
        std::fs::write(&file, "").unwrap();
        let r = done(execute(
            &args(json!({
                "_raw_params": "rm -rf cache",
                "chdir": file.to_str().unwrap(),
                "removes": "cache",
            })),
            false,
            &Context::default(),
            &|| false,
        ));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!r.skipped(), "a file as chdir was read as a guard decision");
        assert!(r.failed());
        // The `rc` and `msg` a bad `chdir` reports are a separate, measured divergence from the
        // reference, which answers `rc: null` and `Unable to change directory before execution.`
        // for both a missing directory and a file. This test is about which branch runs, not
        // about what it says; see `architecture.md`.
    }

    /// An empty `creates` applies no guard at all, matching the reference's own `if creates:`.
    ///
    /// What would make this red: an empty pattern splitting into no components and ending the
    /// walk on the base's own existence, which is true for every `chdir` that exists — so
    /// `creates: "{{ marker | default('') }}"` with `marker` undefined would skip the task.
    #[test]
    fn an_empty_creates_applies_no_guard() {
        let dir = std::env::temp_dir().join(format!("volant-guard-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let r = done(execute(
            &args(json!({
                "_raw_params": "echo ran",
                "chdir": dir.to_str().unwrap(),
                "creates": "",
            })),
            false,
            &Context::default(),
            &|| false,
        ));
        assert!(!r.skipped(), "an empty creates was read as a guard");
        assert_eq!(r.0["stdout"], "ran");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Measurement (p): a glob in `creates` expands inside a directory component too, still
    /// resolved under `chdir`, and the message still quotes the pattern rather than the match.
    ///
    /// What would make this red: matching a component pattern against the whole remaining path
    /// instead of one directory entry at a time, which never reaches `sub/marker` through `s*`.
    #[test]
    fn a_glob_creates_resolves_under_chdir() {
        let dir = std::env::temp_dir().join(format!("volant-guard-glob-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("marker"), b"").unwrap();
        let r = done(execute(
            &args(json!({
                "_raw_params": "echo never",
                "chdir": dir.to_str().unwrap(),
                "creates": "s*/marker",
            })),
            false,
            &Context::default(),
            &|| false,
        ));
        assert_eq!(r.0["msg"], "Did not run command since 's*/marker' exists");
        assert_eq!(r.0["stdout"], "skipped, since s*/marker exists");
        assert!(!r.changed());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Measurement (m): an absolute glob is expanded regardless of `chdir`, and the message
    /// quotes the absolute pattern, not the file that matched it. The `chdir` here is a real
    /// but unrelated directory, because a `chdir` that cannot be entered fails the task before
    /// either guard is read.
    ///
    /// What would make this red: joining the pattern onto `chdir` before walking it, which
    /// sends an absolute `creates` somewhere it cannot match and runs a command that should
    /// have been skipped.
    #[test]
    fn an_absolute_glob_creates_ignores_chdir() {
        let dir = std::env::temp_dir().join(format!("volant-guard-abs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x-1"), b"").unwrap();
        let elsewhere =
            std::env::temp_dir().join(format!("volant-guard-elsewhere-{}", std::process::id()));
        std::fs::create_dir_all(&elsewhere).unwrap();
        let pattern = format!("{}/x-*", dir.display());
        let r = done(execute(
            &args(json!({
                "_raw_params": "echo never",
                "chdir": elsewhere.to_str().unwrap(),
                "creates": pattern,
            })),
            false,
            &Context::default(),
            &|| false,
        ));
        assert_eq!(
            r.0["msg"],
            format!("Did not run command since '{pattern}' exists")
        );
        assert!(!r.changed());
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&elsewhere).unwrap();
    }

    #[test]
    fn chdir_changes_the_working_directory() {
        let dir = std::env::temp_dir().join(format!("volant-chdir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker"), b"").unwrap();
        let r = done(execute(
            &args(json!({"_raw_params": "ls", "chdir": dir.to_str().unwrap()})),
            false,
            &Context::default(),
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
            &Context::default(),
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
            &args(json!({"_raw_params": "cat", "stdin": payload})),
            true,
            &Context::default(),
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
            &Context::default(),
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
            &Context::default(),
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
        let started = Instant::now();
        let run = execute(
            &args(json!({"_raw_params": "sleep 30"})),
            false,
            &Context::default(),
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
            &Context::default(),
            &|| false,
        ));
        assert!(r.failed());
        assert_eq!(r.0["msg"], "no command given");
    }

    #[test]
    fn a_timeout_kills_the_program_and_reports_ansible_shape() {
        let started = Instant::now();
        let r = done(execute(
            &args(json!({"_raw_params": "sleep 30"})),
            false,
            &Context {
                timeout: Some(Duration::from_secs(1)),
                ..Context::default()
            },
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
            Command::new("pgrep")
                .args(["-f", marker])
                .output()
                .is_ok_and(|o| !o.stdout.is_empty())
        };
        // Cancel only once the grandchild is actually running. Cancelling straight away kills the
        // shell before it forks, so nothing would survive however narrow the kill was.
        let deadline = Instant::now() + Duration::from_secs(5);
        let run = execute(
            &args(json!({"_raw_params": format!("sleep {marker} & wait")})),
            true,
            &Context::default(),
            &|| alive(&marker) || Instant::now() > deadline,
        );
        assert!(matches!(run, Run::Cancelled));
        thread::sleep(Duration::from_millis(200));
        let survivors = Command::new("pgrep")
            .args(["-f", &marker])
            .output()
            .unwrap();
        assert!(
            survivors.stdout.is_empty(),
            "grandchild survived: {}",
            String::from_utf8_lossy(&survivors.stdout)
        );
    }

    /// The cancellation predicate consumes what it reports: in the agent it is a `try_recv` on
    /// the control channel, so the `Cancel` is popped and the answer is `true` exactly once. Here
    /// the shell exits at once and leaves a descendant holding the pipes, so the process wait is
    /// over before anything is cancelled and the flag is first seen by a *reader*, which is the
    /// path the other cancellation tests never take. The predicate waits out the process so the
    /// reader is the one that asks, and answers `true` a single time.
    ///
    /// What would make this red: asking the predicate again below the first `true` -- the second
    /// reader would then be told the task is fine and wait for a pipe nobody will close, so the
    /// run never comes back and the bounded `recv_timeout` fails instead of the `matches!`. The
    /// bound is this test's own deadline: `execute` runs on a thread precisely so a regression
    /// that never returns is a failure here rather than a hang the harness has to cut off.
    ///
    /// How it can stop proving anything, so a pass on a loaded machine is not over-read: the gate
    /// below assumes the shell is reaped before it opens. If a machine ever took longer than that
    /// to spawn and reap `exit 0`, the wait loop would take the first `true` and this would
    /// duplicate `cancellation_kills_the_program` rather than exercise the reader path. It
    /// degrades to proving less; it cannot go red for that reason. The measured reap is about
    /// 3 ms against a gate of 300, so the degradation needs a hundredfold regression.
    #[test]
    fn a_cancellation_seen_by_one_reader_is_seen_by_the_rest() {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let started = Instant::now();
            let taken = Cell::new(false);
            let run = execute(
                &args(json!({"_raw_params": "sleep 10 & exit 0"})),
                true,
                &Context::default(),
                &|| started.elapsed() > Duration::from_millis(300) && !taken.replace(true),
            );
            let _ = tx.send(matches!(run, Run::Cancelled));
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "the run did not end cancelled: a reader lost the cancellation"
        );
    }

    #[test]
    fn a_program_killed_by_a_signal_reports_the_negative_signal_number() {
        let r = done(execute(
            &args(json!({"_raw_params": "sh -c 'kill -TERM $$'"})),
            false,
            &Context::default(),
            &|| false,
        ));
        assert_eq!(r.0["rc"], -15);
        assert!(r.failed());
    }
}
