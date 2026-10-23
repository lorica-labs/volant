// SPDX-License-Identifier: GPL-3.0-or-later
//! `su` over a local connection: the agent started behind `su` on a pseudo-terminal of the
//! controller's own, the way `ssh -tt` gives one to a remote `su`.

use crate::agent::AgentLink;

use super::super::defaults::{ConnectError, Escalation};

/// Starts `agent` as `escalation.user` through `su`, answers its password prompt and hands back
/// the link. The agent's stderr goes to a file in a directory the link removes with it.
#[cfg(unix)]
pub(in crate::transport) async fn local_su(
    agent: &str,
    escalation: &Escalation,
) -> Result<AgentLink, ConnectError> {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;

    use tokio::process::Command;

    use super::super::shell::single_quoted;
    use super::su::premature_end;
    use super::sudo::become_success;
    use super::{refuse_option_user, su_command, su_exchange};

    /// How long `su` may stay silent before the link is given up: the reference's default
    /// `timeout` of 10 seconds and the two it adds. A local connection has no `ConnectTimeout`
    /// to follow, so `-T` does not move it.
    const LOCAL_SU_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

    refuse_option_user(escalation)?;
    let unreachable =
        |what: &str, e: std::io::Error| ConnectError::Unreachable(format!("{what}: {e}"));
    // Mode 0777, because the account `su` switches to writes into it and may be another one.
    let scratch = tempfile::Builder::new()
        .prefix("volant-su-")
        .tempdir()
        .map_err(|e| unreachable("making the directory for the agent's stderr", e))?;
    std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o777))
        .map_err(|e| unreachable("opening the directory for the agent's stderr", e))?;
    let stderr_file = scratch.path().join("agent.stderr");
    let success = become_success();
    let command = format!(
        "{} 2>{}",
        single_quoted(agent),
        single_quoted(&stderr_file.display().to_string())
    );
    let (master, slave) = pty::open().map_err(|e| unreachable("opening a terminal for su", e))?;
    let mut su = Command::new("sh");
    su.arg("-c")
        .arg(su_command(escalation, &command, &success))
        .stdin(Stdio::from(
            slave
                .try_clone()
                .map_err(|e| unreachable("opening a terminal for su", e))?,
        ))
        .stdout(Stdio::from(
            slave
                .try_clone()
                .map_err(|e| unreachable("opening a terminal for su", e))?,
        ))
        .stderr(Stdio::from(slave))
        .kill_on_drop(true);
    let mut child = su
        .spawn()
        .map_err(|e| ConnectError::Unreachable(format!("starting su: {e}")))?;
    // The terminal's slave must live in `su` alone, or the agent never sees it hang up.
    drop(su);
    let (mut reader, mut writer) =
        pty::split(master).map_err(|e| unreachable("opening a terminal for su", e))?;
    let password = escalation.password.as_deref();
    if let Some(said) = su_exchange(
        &mut reader,
        &mut writer,
        password,
        &escalation.prompt_l10n,
        &success,
        LOCAL_SU_TIMEOUT,
        true,
    )
    .await?
    {
        let _ = child.wait().await;
        return Err(premature_end(&said));
    }
    AgentLink::new_over_local_tty(child, Box::new(writer), Box::new(reader), scratch.keep())
        .map_err(|e| ConnectError::Unreachable(format!("{e:#}")))
}

#[cfg(not(unix))]
pub(in crate::transport) async fn local_su(
    _agent: &str,
    _escalation: &Escalation,
) -> Result<AgentLink, ConnectError> {
    Err(ConnectError::Become(
        "become_method 'su' is not supported on this platform".to_string(),
    ))
}

/// A pseudo-terminal, and the two halves of its master that an async link reads and writes.
#[cfg(unix)]
mod pty {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use tokio::io::{AsyncRead, ReadBuf};
    use tokio::net::unix::pipe::{Receiver, Sender};

    /// The master and the slave of a new terminal, neither inherited by what is started.
    pub(super) fn open() -> io::Result<(OwnedFd, OwnedFd)> {
        let (mut master, mut slave) = (0, 0);
        // SAFETY: both descriptors are written by the call; the name, terminal settings and
        // window size are optional and left to their defaults.
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
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `openpty` returned these two descriptors and nothing else owns them.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        for fd in [&master, &slave] {
            // SAFETY: the descriptor is open for as long as the borrow.
            if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // SAFETY: as above; the flag lives on the master's open file description, which the
        // two halves below share.
        if unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((master, slave))
    }

    /// Reads a terminal's master, where the end of the stream is an `EIO` once nothing holds the
    /// slave any more.
    pub(super) struct Master(Receiver);

    impl AsyncRead for Master {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match Pin::new(&mut self.0).poll_read(cx, buf) {
                Poll::Ready(Err(e)) if e.raw_os_error() == Some(libc::EIO) => Poll::Ready(Ok(())),
                other => other,
            }
        }
    }

    /// The master as a reader and a writer. Must run inside the runtime.
    pub(super) fn split(master: OwnedFd) -> io::Result<(Master, Sender)> {
        let copy = master.try_clone()?;
        Ok((
            Master(Receiver::from_owned_fd_unchecked(copy)?),
            Sender::from_owned_fd_unchecked(master)?,
        ))
    }
}
