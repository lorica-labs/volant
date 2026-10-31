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
    run_within_seen(command, deadline, stdin, |_| {})
}

/// [`run_within`], with `seen` given the child's pid once it has started, for a test that watches
/// the process while it runs.
pub fn run_within_seen(
    command: &mut Command,
    deadline: Duration,
    stdin: Option<&[u8]>,
    seen: impl FnOnce(u32),
) -> Output {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    let what = command.get_program().to_string_lossy().into_owned();
    // What a deadline panic names: the arguments carry the playbook, so a run among many says
    // which one hung.
    let arguments = command
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("{what} starts: {e}"));
    // Written from a thread of its own: a child that answers before it has read everything blocks
    // on a full output pipe, which only the wait below drains.
    let writer = stdin.map(|bytes| {
        use std::io::Write as _;
        let bytes = bytes.to_vec();
        let mut pipe = child.stdin.take().expect("the child's standard input");
        std::thread::spawn(move || {
            // A child that exits without reading it all closes the pipe: not a failure of the test.
            let _ = pipe.write_all(&bytes);
        })
    });
    let pid = child.id();
    seen(pid);
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || send.send(child.wait_with_output()));
    let received = receive.recv_timeout(deadline);
    // Where a hung child waits, taken before it is killed: the panic below carries it.
    let stacks = if received.is_err() {
        stacks_of(pid)
    } else {
        String::new()
    };
    if received.is_err() {
        kill_group(pid);
    }
    if let Some(writer) = writer {
        writer.join().expect("the writing thread ends");
    }
    let Ok(out) = received else {
        let out = receive
            .recv()
            .expect("the waiting thread answers")
            .expect("the child's output");
        panic!(
            "{what} {arguments} did not finish within {deadline:?}:\n{}\n--- stderr\n{}\n{stacks}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    out.expect("the child's output")
}

/// The stacks of each thread of `pid`, where the system can tell them without a debugger: macOS
/// `sample`. Empty elsewhere.
fn stacks_of(pid: u32) -> String {
    if !cfg!(target_os = "macos") {
        return String::new();
    }
    Command::new("sample")
        .args([&pid.to_string(), "1"])
        .output()
        .map(|out| format!("--- stacks\n{}", String::from_utf8_lossy(&out.stdout)))
        .unwrap_or_default()
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
