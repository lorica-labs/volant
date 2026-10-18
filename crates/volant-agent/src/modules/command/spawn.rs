// SPDX-License-Identifier: GPL-3.0-or-later
//! The process side of a run: its pipes and pseudo-terminal, the wait on its output, the kill.

use std::io::Read;
#[cfg(unix)]
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

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
pub(super) fn collect(
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
pub(super) fn open_pty() -> std::io::Result<(std::fs::File, [Stdio; 3])> {
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
pub(super) fn exit_code(status: std::process::ExitStatus) -> i64 {
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
pub(super) fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<String> {
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
