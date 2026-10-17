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
#[cfg(unix)]
use crate::natives::common::{clean_args, popen_error};

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
    pop_cancel: &dyn Fn() -> bool,
) -> Run {
    let timeout = ctx.timeout;
    // Every boolean here is read the way the reference reads one, spellings and all: `"false"`,
    // `"no"` and `0` turn the argument off there, and `Value::as_bool` sees none of them.
    let flag = |name: &str, default: bool| bool_arg(args, name, default);
    let (uses_shell, strip_empty_ends, add_newline, expand_vars_on) = match (
        flag("_uses_shell", false),
        flag("strip_empty_ends", true),
        flag("stdin_add_newline", true),
        flag("expand_argument_vars", true),
    ) {
        (Ok(shell), Ok(strip), Ok(newline), Ok(expand)) => {
            (uses_shell || shell, strip, newline, expand)
        }
        (Err(r), ..) | (_, Err(r), ..) | (_, _, Err(r), _) | (.., Err(r)) => return Run::Done(r),
    };
    // Measured on ansible-core 2.19.12: the newline is appended without looking at what the value
    // already ends with, so a value that ends in one gets a second.
    let stdin_data = args.get("stdin").and_then(Value::as_str).map(|s| {
        if add_newline {
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
    let mut argv: Vec<String> = match from_argv {
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
    // Measured on ansible-core 2.19.12: `command` expands `$VAR`, `${VAR}` and `~` in each word it
    // runs, on by default; `shell` does not, with an `argv` or without. The task still reports the
    // words as written, and only a failure to run reports them expanded.
    let var = |name: &str| {
        ctx.environment
            .get(name)
            .cloned()
            .or_else(|| std::env::var(name).ok())
    };
    if !uses_shell && expand_vars_on {
        for word in &mut argv {
            let expanded = expand_vars(word, &var);
            #[cfg(unix)]
            let expanded = expand_user(&expanded, &var);
            *word = expanded;
        }
    }
    // Measured on ansible-core 2.19.12: `chdir`, `creates` and `removes` are declared as paths, so
    // `~` and `$VAR` are expanded in them for `command` and `shell` alike, and the skip message
    // names the expanded path.
    let path_arg = |name: &str| {
        args.get(name).and_then(Value::as_str).map(|p| {
            let expanded = expand_vars(p, &var);
            #[cfg(unix)]
            let expanded = expand_user(&expanded, &var);
            expanded
        })
    };
    let chdir = path_arg("chdir");
    let chdir = chdir.as_deref();

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
            Err(err) => return Run::Done(chdir_failure(display, dir, &err)),
            Ok(meta) if !meta.is_dir() => {
                let err = std::io::Error::from_raw_os_error(libc::ENOTDIR);
                return Run::Done(chdir_failure(display, dir, &err));
            }
            Ok(_) =>
            {
                #[cfg(unix)]
                if !crate::natives::common::access(dir, libc::X_OK) {
                    let err = std::io::Error::last_os_error();
                    return Run::Done(chdir_failure(display, dir, &err));
                }
            }
        }
    }
    let guard_base = chdir.map(Path::new);
    let shoulda = if ctx.check_mode { "Would" } else { "Did" };
    // An empty value applies no guard at all, which is what the reference's own `if creates:`
    // does with it: a template that rendered to nothing must not decide anything.
    let creates = path_arg("creates");
    if let Some(path) = creates.as_deref()
        && !path.is_empty()
        && glob::matches_any(guard_base, path)
    {
        return Run::Done(not_run(
            display,
            format!("{shoulda} not run command since '{path}' exists"),
            format!("skipped, since {path} exists"),
        ));
    }
    let removes = path_arg("removes");
    if let Some(path) = removes.as_deref()
        && !path.is_empty()
        && !glob::matches_any(guard_base, path)
    {
        return Run::Done(not_run(
            display,
            format!("{shoulda} not run command since '{path}' does not exist"),
            format!("skipped, since {path} does not exist"),
        ));
    }
    if ctx.check_mode {
        // The reference's check mode: changed when a guard was given and let it through,
        // skipped when there was no guard to judge by. `creates: ""` counts as given there too.
        let mut result = not_run(
            display,
            "Command would have run if not in check mode".into(),
            String::new(),
        );
        if creates.is_none() && removes.is_none() {
            result.0.insert("skipped".into(), json!(true));
        } else {
            result.0.insert("changed".into(), json!(true));
        }
        return Run::Done(result);
    }

    // The controller asks for a terminal when ssh would have given one: the text it carries is the
    // line ssh prints when the session ends, which the command's stderr ends with.
    let tty = match args.get("_volant_tty") {
        None | Some(Value::Bool(false)) => None,
        Some(Value::Bool(true)) => Some(String::new()),
        Some(Value::String(close)) => Some(close.clone()),
        Some(_) => {
            return Run::Done(TaskResult::failed_with(
                "_volant_tty must be a boolean or the text ssh prints when the session closes",
            ));
        }
    };
    if tty.is_some() && stdin_data.is_some() {
        return Run::Done(TaskResult::failed_with(
            "a command run with a terminal takes no stdin",
        ));
    }
    #[cfg(not(unix))]
    if tty.is_some() {
        return Run::Done(TaskResult::failed_with(
            "running a command under a terminal is not supported on this platform",
        ));
    }
    #[cfg(unix)]
    let pty = if tty.is_some() {
        match open_pty() {
            Ok(pty) => Some(pty),
            Err(err) => {
                return Run::Done(TaskResult::failed_with(format!(
                    "could not open a terminal for the command: {err}"
                )));
            }
        }
    } else {
        None
    };

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
    // Everything the command writes goes through the terminal, which merges the two streams and
    // turns each newline into CR LF, as the terminal ssh -tt allocates does.
    #[cfg(unix)]
    let master = pty.map(|(master, [stdin, stdout, stderr])| {
        use std::os::unix::process::CommandExt;
        command.stdin(stdin).stdout(stdout).stderr(stderr);
        // SAFETY: only async-signal-safe calls between fork and exec. The command leads a session
        // of its own, which makes the slave its controlling terminal; the session leader's id is
        // also its group's, so `kill_group` still reaches everything it started.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        master
    });
    if let Some(dir) = chdir {
        command.current_dir(dir);
    }
    // Added to what the agent inherited rather than replacing it, which is what Ansible's
    // `environment` does: a task setting `PATH` keeps `HOME`, and `sudo`'s own `env_reset` is
    // already behind us - the escalated agent is the process this one is forked from.
    command.envs(&ctx.environment);
    #[cfg(unix)]
    if master.is_none() {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let spawned = command.spawn();
    // The slave end the command was given must not stay open here: the master reads end-of-file
    // only once every copy of it is closed.
    drop(command);
    let mut child = match spawned {
        Ok(child) => child,
        #[cfg(unix)]
        Err(err) => return Run::Done(exec_failure(&argv, &err)),
        #[cfg(not(unix))]
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
    #[cfg(unix)]
    let stdout = match master {
        Some(master) => drain(Some(master)),
        None => drain(child.stdout.take()),
    };
    #[cfg(not(unix))]
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

    // `pop_cancel` has a side effect: it is a `try_recv` on the control channel, so it pops the
    // cancellation message and answers `true` exactly **once**. The wait below and each of the two
    // readers ask it, and every one of them has to get the same answer, so the first `true` is
    // latched here and the rest of this function asks the latch, under the name `cancelled`. The
    // raw predicate has another name so that no call below can reach it by that one: a reader
    // handed it after an earlier ask popped the `Cancel` would wait for ever.
    let latch = {
        let seen = Cell::new(false);
        move || {
            if !seen.get() && pop_cancel() {
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
    if let Some(close) = tty.filter(|close| !close.is_empty()) {
        stderr.push_str(&close);
        stderr.push_str("\r\n");
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

/// A pseudo-terminal: the master end the output is read from, and the slave end the command runs
/// on. Neither is inherited by what the command starts.
#[cfg(unix)]
fn open_pty() -> std::io::Result<(std::fs::File, [Stdio; 3])> {
    use std::os::fd::{FromRawFd, OwnedFd};
    let (mut master, mut slave) = (0, 0);
    // SAFETY: both descriptors are written by the call; the name, terminal settings and window
    // size are optional and left to their defaults, which turn a newline into CR LF on output.
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if opened != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `openpty` returned these two descriptors and nothing else owns them.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [&master, &slave] {
        use std::os::fd::AsRawFd;
        // SAFETY: the descriptor is open for as long as the borrow.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    let again = || slave.try_clone().map(Stdio::from);
    Ok((
        std::fs::File::from(master),
        [again()?, again()?, Stdio::from(slave)],
    ))
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

/// A guard that stopped the command. The reference leaves the task `ok`, never `skipped`, and
/// `start`, `end` and `delta` null because nothing ran.
fn not_run(cmd: Value, msg: String, stdout: String) -> TaskResult {
    let mut result = Map::new();
    result.insert("cmd".into(), cmd);
    for key in ["start", "end", "delta"] {
        result.insert(key.into(), Value::Null);
    }
    result.insert("rc".into(), json!(0));
    result.insert("stdout".into(), json!(stdout));
    result.insert("stderr".into(), json!(""));
    result.insert("stdout_lines".into(), lines(&stdout));
    result.insert("stderr_lines".into(), json!([]));
    result.insert("changed".into(), json!(false));
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

/// A boolean argument as the reference reads it: unset (or null) is `default`, and a value it
/// cannot convert fails the task with its own sentence. The list of valid spellings is a Python
/// set, so the reference prints it in an order that changes from run to run; this one is fixed.
fn bool_arg(args: &Map<String, Value>, name: &str, default: bool) -> Result<bool, TaskResult> {
    let value = match args.get(name) {
        None | Some(Value::Null) => return Ok(default),
        Some(value) => value,
    };
    if let Some(flag) = arg_bool(value) {
        return Ok(flag);
    }
    let (kind, why) = match value {
        Value::String(text) => ("str", text.clone()),
        Value::Number(n) => (if n.is_f64() { "float" } else { "int" }, n.to_string()),
        Value::Array(_) => ("list", String::new()),
        _ => ("dict", String::new()),
    };
    let why = if matches!(value, Value::Array(_) | Value::Object(_)) {
        format!("<class '{kind}'> cannot be converted to a bool")
    } else {
        format!(
            "The value '{why}' is not a valid boolean. Valid booleans include: 'n', 1, '0', 0, 'true', 't', 'off', 'y', 'on', 'f', 'no', 'false', 'yes', '1'"
        )
    };
    let mut result = TaskResult::failed_with(format!(
        "argument '{name}' is of type {kind} and we were unable to convert to bool: {why}"
    ));
    result.0.insert("changed".into(), json!(false));
    Err(result)
}

/// A `chdir` that cannot be entered, as the reference reports it (measured on 2.19.12): the words as
/// written, `rc` and the clock null, nothing run, and the errno in the `exception`.
#[cfg(unix)]
fn chdir_failure(cmd: Value, dir: &str, err: &std::io::Error) -> TaskResult {
    let errno = err.raw_os_error().unwrap_or(libc::EIO);
    let msg = "Unable to change directory before execution.";
    let mut exception = popen_error(errno, dir);
    exception["event"]["msg"] = json!(msg);
    let mut result = Map::new();
    result.insert("cmd".into(), cmd);
    for key in ["rc", "start", "end", "delta"] {
        result.insert(key.into(), Value::Null);
    }
    for key in ["stdout", "stderr"] {
        result.insert(key.into(), json!(""));
        result.insert(format!("{key}_lines"), json!([]));
    }
    result.insert("changed".into(), json!(false));
    result.insert("msg".into(), json!(msg));
    result.insert("failed".into(), json!(true));
    result.insert("exception".into(), exception);
    TaskResult(result)
}

#[cfg(not(unix))]
fn chdir_failure(cmd: Value, dir: &str, err: &std::io::Error) -> TaskResult {
    spawn_failure(cmd, Some(dir), err)
}

/// A program that would not start, as the reference reports `Popen` raising: `rc` is the errno,
/// `cmd` is the words after expansion, quoted and joined into one string, and the cause is in the
/// `exception`. A shell line is the `executable -c line` that was started.
#[cfg(unix)]
fn exec_failure(argv: &[String], err: &std::io::Error) -> TaskResult {
    let errno = err.raw_os_error().unwrap_or(libc::EIO);
    let words: Vec<&str> = argv.iter().map(String::as_str).collect();
    let mut result = Map::new();
    result.insert("cmd".into(), json!(clean_args(&words)));
    result.insert("rc".into(), json!(errno));
    result.insert("msg".into(), json!("Error executing command."));
    for key in ["stdout", "stderr"] {
        result.insert(key.into(), json!(""));
        result.insert(format!("{key}_lines"), json!([]));
    }
    result.insert("changed".into(), json!(false));
    result.insert("failed".into(), json!(true));
    result.insert("exception".into(), popen_error(errno, &argv[0]));
    TaskResult(result)
}

/// `os.path.expandvars`: `$name` and `${name}` whose name `var` knows; any other stays as written.
fn expand_vars(text: &str, var: &dyn Fn(&str) -> Option<String>) -> String {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let (name, len) = if let Some(braced) = after.strip_prefix('{') {
            braced
                .find('}')
                .map_or(("", 0), |end| (&braced[..end], end + 2))
        } else {
            let end = after.find(|c| !word(c)).unwrap_or(after.len());
            (&after[..end], end)
        };
        match (len > 0).then(|| var(name)).flatten() {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[at..=at + len]),
        }
        rest = &after[len..];
    }
    out.push_str(rest);
    out
}

/// `os.path.expanduser`: a leading `~` or `~name` becomes the home directory of the module's
/// `HOME`, or of the account; an unknown account leaves the word as written.
#[cfg(unix)]
fn expand_user(path: &str, var: &dyn Fn(&str) -> Option<String>) -> String {
    let Some(tail) = path.strip_prefix('~') else {
        return path.to_string();
    };
    let uid = unsafe { libc::getuid() };
    let end = tail.find('/').unwrap_or(tail.len());
    let (name, rest) = tail.split_at(end);
    let home = if name.is_empty() {
        var("HOME").or_else(|| passwd_home(&uid.to_string(), true))
    } else {
        passwd_home(name, false)
    };
    let Some(home) = home else {
        return path.to_string();
    };
    let joined = format!("{}{rest}", home.trim_end_matches('/'));
    if joined.is_empty() {
        "/".into()
    } else {
        joined
    }
}

/// The home directory of an account, by name or by uid. The agent is a static musl binary, whose
/// `getpwnam` reads `/etc/passwd` and nothing else, as this does.
#[cfg(unix)]
fn passwd_home(key: &str, by_uid: bool) -> Option<String> {
    let text = std::fs::read_to_string("/etc/passwd").ok()?;
    text.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        (fields.len() >= 6 && fields[usize::from(by_uid) * 2] == key).then(|| fields[5].to_string())
    })
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
    use crate::testing::{args, done, scratch};
    use rstest::rstest;
    use serde_json::json;

    /// `stdin_add_newline` decides whether the value written to the child ends in a newline, and
    /// its default is to add one.
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

    /// `chdir`, `creates` and `removes` expand `$VAR` from the task's environment, for `shell` too.
    #[test]
    fn the_three_paths_expand_variables() {
        let dir = scratch();
        std::fs::write(dir.path().join("marker"), b"").unwrap();
        let ctx = Context {
            environment: [("S3D".to_string(), dir.path().to_str().unwrap().to_string())].into(),
            ..Context::default()
        };
        for shell in [false, true] {
            let params = json!({"_raw_params": "pwd", "_uses_shell": shell, "chdir": "$S3D", "creates": "$S3D/marker"});
            let r = done(execute(&args(params), false, &ctx, &|| false));
            let want = format!(
                "Did not run command since '{}/marker' exists",
                dir.path().display()
            );
            assert_eq!(r.0["msg"], want.as_str(), "{:?}", r.0);
        }
    }

    /// `creates` and `removes` resolve under `chdir`, expand globs per directory component, keep
    /// the pattern as written in the message, and an empty `creates` is no guard.
    #[rstest]
    #[case::relative_creates(
        &["marker"],
        json!({"_raw_params": "echo never", "chdir": "$DIR", "creates": "marker"}),
        false,
        "marker"
    )]
    #[case::relative_removes(
        &["cache"],
        json!({"_raw_params": "echo ran", "chdir": "$DIR", "removes": "cache"}),
        true,
        "ran"
    )]
    #[case::empty_creates(
        &[],
        json!({"_raw_params": "echo ran", "chdir": "$DIR", "creates": ""}),
        true,
        "ran"
    )]
    #[case::glob_in_a_directory_component(
        &["sub/marker"],
        json!({"_raw_params": "echo never", "chdir": "$DIR", "creates": "s*/marker"}),
        false,
        "s*/marker"
    )]
    #[case::absolute_glob_ignores_chdir(
        &["x-1"],
        json!({"_raw_params": "echo never", "chdir": "$ELSE", "creates": "$DIR/x-*"}),
        false,
        "$DIR/x-*"
    )]
    fn a_guard_decides_whether_the_command_runs(
        #[case] fixtures: &[&str],
        #[case] params: Value,
        #[case] ran: bool,
        #[case] text: &str,
    ) {
        let dir_guard = scratch();
        let else_guard = scratch();
        let dir = dir_guard.path().to_str().unwrap();
        for file in fixtures {
            let path = dir_guard.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"").unwrap();
        }
        let fill = |s: &str| {
            s.replace("$DIR", dir)
                .replace("$ELSE", else_guard.path().to_str().unwrap())
        };
        let params: Value = serde_json::from_str(&fill(&params.to_string())).unwrap();
        let r = done(execute(&args(params), false, &Context::default(), &|| {
            false
        }));
        let text = fill(text);
        if ran {
            assert!(r.changed(), "a guard skipped what it should not: {:?}", r.0);
            assert_eq!(r.0["stdout"], text.as_str());
        } else {
            assert!(!r.changed(), "{:?}", r.0);
            assert_eq!(
                r.0["msg"],
                format!("Did not run command since '{text}' exists")
            );
            assert_eq!(r.0["stdout"], format!("skipped, since {text} exists"));
        }
    }

    /// A command that cannot run fails the task in the reference's shape: its `rc` and message
    /// where it has them, before any guard is read when the `chdir` is the problem.
    #[rstest]
    #[case::non_zero_rc(
        json!({"_raw_params": "false"}), Some(json!(1)), Some("non-zero return code")
    )]
    #[case::missing_program(
        json!({"_raw_params": "volant-no-such-program"}), Some(json!(2)),
        Some("Error executing command.")
    )]
    #[case::missing_chdir(
        json!({"_raw_params": "echo never", "chdir": "/definitely/not/here"}), Some(json!(null)),
        Some("Unable to change directory before execution.")
    )]
    #[case::missing_chdir_before_a_guard(
        json!({"_raw_params": "rm -rf cache", "chdir": "/definitely/not/here", "removes": "cache"}),
        Some(json!(null)), Some("Unable to change directory before execution.")
    )]
    #[case::chdir_that_is_a_file(
        json!({"_raw_params": "rm -rf cache", "chdir": "$FILE", "removes": "cache"}), None, None
    )]
    #[case::empty_command(json!({"_raw_params": "   "}), None, Some("no command given"))]
    #[case::killed_by_a_signal(
        json!({"_raw_params": "sh -c 'kill -TERM $$'"}), Some(json!(-15)), None
    )]
    fn a_command_that_cannot_run_fails_in_the_reference_shape(
        #[case] params: Value,
        #[case] rc: Option<Value>,
        #[case] msg: Option<&str>,
    ) {
        let dir_guard = scratch();
        let file = dir_guard.path().join("config.yml");
        std::fs::write(&file, "").unwrap();
        let params: Value =
            serde_json::from_str(&params.to_string().replace("$FILE", file.to_str().unwrap()))
                .unwrap();
        let r = done(execute(&args(params), false, &Context::default(), &|| {
            false
        }));
        assert!(r.failed(), "{:?}", r.0);
        if let Some(rc) = rc {
            assert_eq!(r.0["rc"], rc, "{:?}", r.0);
        }
        if let Some(msg) = msg {
            assert!(r.0["msg"].as_str().unwrap().starts_with(msg), "{:?}", r.0);
        }
    }

    /// A boolean argument the reference cannot convert fails the task with its sentence.
    #[rstest]
    #[case::string(json!("maybe"), "of type str and we were unable to convert to bool: The value 'maybe' is not a valid boolean.")]
    #[case::int(json!(2), "of type int and we were unable to convert to bool: The value '2' is not a valid boolean.")]
    #[case::list(json!(["a"]), "of type list and we were unable to convert to bool: <class 'list'> cannot be converted to a bool")]
    fn an_unreadable_boolean_fails_the_task(#[case] value: Value, #[case] want: &str) {
        let r = done(execute(
            &args(json!({"_raw_params": "echo hi", "expand_argument_vars": value})),
            false,
            &Context::default(),
            &|| false,
        ));
        let msg = r.0["msg"].as_str().unwrap_or_default();
        assert!(
            r.failed()
                && msg.starts_with("argument 'expand_argument_vars' is ")
                && msg.contains(want),
            "{:?}",
            r.0
        );
    }

    /// A directory the agent may not search fails as an unusable `chdir`, not as a program that
    /// would not start.
    #[test]
    fn a_chdir_without_search_permission_is_a_chdir_failure() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root searches any directory, so there is nothing to see.
        if unsafe { libc::geteuid() } != 0 {
            let r = done(execute(
                &args(json!({"_raw_params": "echo hi", "chdir": locked.to_str().unwrap()})),
                false,
                &Context::default(),
                &|| false,
            ));
            assert_eq!(
                r.0["msg"], "Unable to change directory before execution.",
                "{:?}",
                r.0
            );
            assert_eq!(r.0["rc"], Value::Null);
        }
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// Each form runs the way the reference runs it: the shell form through `sh`, `argv`
    /// and `cmd` directly, an `argv` on the shell path as the list, and a fed standard input.
    #[rstest]
    #[case::shell_form(json!({"_raw_params": "echo $((6 * 7))"}), true, "42", json!("echo $((6 * 7))"))]
    #[case::argv(
        json!({"argv": ["printf", "%s-%s", "a", "b"]}), false, "a-b", json!(["printf", "%s-%s", "a", "b"])
    )]
    #[case::cmd(json!({"cmd": "echo cmd-form"}), false, "cmd-form", json!(["echo", "cmd-form"]))]
    #[case::argv_on_the_shell_path(json!({"argv": ["echo", "hi"]}), true, "hi", json!(["echo", "hi"]))]
    #[case::stdin_is_fed(
        json!({"_raw_params": "cat", "stdin": "from stdin"}), false, "from stdin", json!(["cat"])
    )]
    fn a_form_runs_the_way_the_reference_runs_it(
        #[case] params: Value,
        #[case] shell: bool,
        #[case] stdout: &str,
        #[case] cmd: Value,
    ) {
        let r = done(execute(&args(params), shell, &Context::default(), &|| {
            false
        }));
        assert_eq!(r.0["stdout"], stdout, "{:?}", r.0);
        assert_eq!(r.0["cmd"], cmd);
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

    /// A guard that skips reports `ok` with the reference's keys, key for key, and no `skipped`.
    #[test]
    fn a_skipped_guard_reports_the_reference_shape() {
        {
            let r = done(execute(
                &args(json!({"_raw_params": "true", "creates": "/"})),
                false,
                &Context::default(),
                &|| false,
            ));
            assert_eq!(
                Value::Object(r.0),
                json!({
                    "changed": false, "cmd": ["true"], "delta": null, "end": null,
                    "msg": "Did not run command since '/' exists", "rc": 0, "start": null,
                    "stderr": "", "stderr_lines": [], "stdout": "skipped, since / exists",
                    "stdout_lines": ["skipped, since / exists"],
                })
            );
        }
        {
            let r = done(execute(
                &args(json!({"_raw_params": "true", "removes": "/definitely/not/here"})),
                false,
                &Context::default(),
                &|| false,
            ));
            assert_eq!(
                Value::Object(r.0),
                json!({
                    "changed": false, "cmd": ["true"], "delta": null, "end": null,
                    "msg": "Did not run command since '/definitely/not/here' does not exist",
                    "rc": 0, "start": null, "stderr": "", "stderr_lines": [],
                    "stdout": "skipped, since /definitely/not/here does not exist",
                    "stdout_lines": ["skipped, since /definitely/not/here does not exist"],
                })
            );
        }
    }

    /// Check mode runs nothing, and a guard says "Would" where it said "Did", as the reference does.
    #[rstest::rstest]
    #[case::no_guard(json!({"_raw_params": "touch /never"}), "Command would have run if not in check mode", false, true)]
    #[case::creates_absent(json!({"_raw_params": "touch /never", "creates": "/definitely/not/here"}), "Command would have run if not in check mode", true, false)]
    #[case::removes_present(json!({"_raw_params": "touch /never", "removes": "/"}), "Command would have run if not in check mode", true, false)]
    #[case::creates_present(json!({"_raw_params": "touch /never", "creates": "/"}), "Would not run command since '/' exists", false, false)]
    #[case::removes_absent(json!({"_raw_params": "touch /never", "removes": "/definitely/not/here"}), "Would not run command since '/definitely/not/here' does not exist", false, false)]
    fn check_mode_runs_no_command(
        #[case] params: Value,
        #[case] msg: &str,
        #[case] changed: bool,
        #[case] skipped: bool,
    ) {
        let ctx = Context {
            check_mode: true,
            ..Context::default()
        };
        let r = done(execute(&args(params), false, &ctx, &|| false));
        assert_eq!(r.0["msg"], msg);
        assert_eq!(r.0["changed"], changed);
        assert_eq!(r.0.get("skipped").is_some(), skipped, "{:?}", r.0);
        assert_eq!(r.0["start"], Value::Null, "nothing ran");
    }

    /// 4 MiB of standard input does not deadlock the write, and `chdir` sets the working directory.
    #[test]
    fn a_large_stdin_does_not_deadlock_and_chdir_sets_the_directory() {
        {
            let payload: String = "abcdefghijklmnopqrstuvwxyz0123456789\n"
                .chars()
                .cycle()
                .take(4 * 1_048_576)
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
        {
            let dir_guard = scratch();
            let dir = dir_guard.path().to_path_buf();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("marker"), b"").unwrap();
            let r = done(execute(
                &args(json!({"_raw_params": "ls", "chdir": dir.to_str().unwrap()})),
                false,
                &Context::default(),
                &|| false,
            ));
            assert_eq!(r.0["stdout"], "marker");
        }
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
        assert_eq!(r.0["timedout"]["period"], json!(1));
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

    /// A command run under a terminal: both streams arrive as stdout with CR LF line ends,
    /// stderr holds only the close line when one is asked for, and the return code is the command's.
    #[rstest]
    #[case::merged("echo out; echo err >&2; exit 3", json!(true), "out\r\nerr\r\n", "", 3)]
    #[case::close_line("printf 'a\nb'", json!("Shared connection to h closed."), "a\r\nb", "Shared connection to h closed.\r\n", 0)]
    #[case::a_terminal_is_stdin("test -t 0 && test -t 1 && test -t 2", json!(true), "", "", 0)]
    #[case::off("echo out", json!(false), "out\n", "", 0)]
    fn a_command_with_a_terminal_reports_in_the_reference_shape(
        #[case] line: &str,
        #[case] tty: Value,
        #[case] stdout: &str,
        #[case] stderr: &str,
        #[case] rc: i64,
    ) {
        let Run::Done(r) = execute(
            &args(json!({"_raw_params": line, "_volant_tty": tty, "strip_empty_ends": false})),
            true,
            &Context::default(),
            &|| false,
        ) else {
            panic!("cancelled")
        };
        assert_eq!(r.0["stdout"], stdout);
        assert_eq!(r.0["stderr"], stderr);
        assert_eq!(r.0["rc"], rc);
    }

    #[rstest]
    #[case::not_a_boolean(json!({"_raw_params": "true", "_volant_tty": 3}), "_volant_tty must be")]
    #[case::stdin(json!({"_raw_params": "cat", "_volant_tty": true, "stdin": "x"}), "takes no stdin")]
    fn a_terminal_that_cannot_be_given_is_refused(#[case] given: Value, #[case] why: &str) {
        let Run::Done(r) = execute(&args(given), true, &Context::default(), &|| false) else {
            panic!("cancelled")
        };
        assert!(r.failed());
        assert!(
            r.0["msg"].as_str().unwrap().contains(why),
            "{:?}",
            r.0["msg"]
        );
    }

    #[test]
    fn cancellation_kills_the_group_of_a_command_with_a_terminal() {
        let marker = format!("31.{}", std::process::id());
        // Without `pgrep` the survivor check below would pass over nothing.
        Command::new("pgrep")
            .arg("--version")
            .output()
            .expect("this test needs pgrep to look for survivors");
        let alive = |marker: &str| {
            Command::new("pgrep")
                .args(["-f", marker])
                .output()
                .is_ok_and(|o| !o.stdout.is_empty())
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let run = execute(
            &args(json!({"_raw_params": format!("sleep {marker} & wait"), "_volant_tty": true})),
            true,
            &Context::default(),
            &|| alive(&marker) || Instant::now() > deadline,
        );
        assert!(matches!(run, Run::Cancelled));
        thread::sleep(Duration::from_millis(200));
        assert!(!alive(&marker), "the command's child outlived the cancel");
    }

    /// The cancellation predicate consumes what it reports: in the agent it is a `try_recv` on the
    /// control channel, so the `Cancel` is popped and the answer is `true` exactly once.
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
}
