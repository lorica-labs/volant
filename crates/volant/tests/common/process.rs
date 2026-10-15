// SPDX-License-Identifier: GPL-3.0-or-later
//! The one way a test runs a process with a deadline: a run that waits for ever is a failure, not
//! a job that runs until the harness's own timeout.

use std::process::{Command, Output, Stdio};
use std::time::Duration;

/// Runs `command` in a process group of its own, `stdin` written to it when given, and returns its
/// output, or kills the whole group and panics with the stdout so far at `deadline`. The group,
/// because the agents and modules a run starts hold the same pipes; the output is read while
/// waiting, since a child whose output outgrows the pipe blocks until someone does.
pub fn run_within(command: &mut Command, deadline: Duration, stdin: Option<&[u8]>) -> Output {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    let what = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("{what} starts: {e}"));
    if let Some(bytes) = stdin {
        use std::io::Write as _;
        child
            .stdin
            .take()
            .expect("the child's standard input")
            .write_all(bytes)
            .expect("the input reaches the child");
    }
    let pid = child.id();
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || send.send(child.wait_with_output()));
    let Ok(out) = receive.recv_timeout(deadline) else {
        kill_group(pid);
        let out = receive
            .recv()
            .expect("the waiting thread answers")
            .expect("the child's output");
        panic!(
            "{what} did not finish within {deadline:?}:\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
    };
    out.expect("the child's output")
}

#[cfg(unix)]
fn kill_group(pid: u32) {
    let pid = libc::pid_t::try_from(pid).expect("a pid fits a pid_t");
    // Safety: `-pid` is the child's own process group, which it leads and nothing has reaped
    // while the thread waits on it.
    unsafe { libc::kill(-pid, libc::SIGKILL) };
}

#[cfg(not(unix))]
fn kill_group(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .status();
}
