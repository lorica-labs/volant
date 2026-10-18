// SPDX-License-Identifier: GPL-3.0-or-later
//! Running one command: the arguments read, the process started and waited for, the result built.

use std::cell::Cell;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;

#[cfg(unix)]
use super::exec_failure;
#[cfg(unix)]
use super::expand::expand_user;
use super::expand::expand_vars;
#[cfg(unix)]
use super::spawn::open_pty;
use super::spawn::{CANCEL_POLL, collect, drain, exit_code, kill_group};
use super::{bool_arg, chdir_failure, lines, not_run, spawn_failure};
use crate::clock;
use crate::modules::{Context, Run, glob};
/// Runs one command. `uses_shell` selects `shell` semantics (`sh -c`) over `command`.
#[expect(clippy::too_many_lines)]
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
