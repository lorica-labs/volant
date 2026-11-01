// SPDX-License-Identifier: GPL-3.0-or-later
//! `sudo` over a local connection: the agent starts behind a shell that first prints a fresh
//! success line, as the reference's command prints its `BECOME-SUCCESS-<random>`. What comes
//! before that line - a `become_exe` wrapper's own words on stdout or stderr - is read as such,
//! never as the agent's first frame.

use std::fmt::Write as _;
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;

use crate::agent::AgentLink;

use super::super::defaults::ConnectError;

/// How long `sudo` may take to start the shell once the probe has shown it will: the
/// reference's default `become_success_timeout` of 10 seconds and the two the `su` link adds.
const SUCCESS_TIMEOUT: Duration = Duration::from_secs(12);

/// Hands `password` to `child`, a `sudo` started with its three pipes, waits for `success` on
/// its stdout and returns the link over what follows. What the two streams said before is kept
/// for the link's `raw` tasks with `keep`, else it goes to the controller's stderr, where the
/// agent's own stderr goes from then on.
pub(in crate::transport) async fn local_sudo(
    mut child: Child,
    password: Option<Vec<u8>>,
    success: &str,
    keep: bool,
) -> Result<AgentLink, ConnectError> {
    let (Some(mut stdin), Some(stdout), Some(mut stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(ConnectError::Unreachable(
            "sudo was started without its pipes".to_string(),
        ));
    };
    if let Some(bytes) = password {
        // A `sudo` that refuses before reading closes the pipe: the end of stream says so below.
        let _ = stdin.write_all(&bytes).await;
        let _ = stdin.flush().await;
    }
    let mut stdout = BufReader::new(stdout);
    let (mut before, mut said) = (Vec::new(), Vec::new());
    let waited = tokio::time::timeout(
        SUCCESS_TIMEOUT,
        until_success(&mut stdout, &mut stderr, &mut before, &mut said, success),
    )
    .await;
    match waited {
        Ok(true) => {}
        Ok(false) => {
            let _ = child.wait().await;
            return Err(ConnectError::Become(waiting_message(
                "Premature end of stream",
                &before,
                &said,
            )));
        }
        Err(_) => {
            return Err(ConnectError::Unreachable(waiting_message(
                "Timed out",
                &before,
                &said,
            )));
        }
    }
    let preamble = if keep {
        // What stderr said just before the line may still be on its way.
        let mut chunk = [0; 4096];
        while let Ok(Ok(read @ 1..)) =
            tokio::time::timeout(Duration::from_millis(20), stderr.read(&mut chunk)).await
        {
            said.extend_from_slice(&chunk[..read]);
        }
        // The reference keeps its success line too.
        before.extend_from_slice(success.as_bytes());
        before.push(b'\n');
        Some((before, said))
    } else {
        let _ = std::io::Write::write_all(&mut std::io::stderr(), &said);
        None
    };
    tokio::spawn(async move {
        let mut chunk = [0; 4096];
        while let Ok(read @ 1..) = stderr.read(&mut chunk).await {
            let _ = std::io::Write::write_all(&mut std::io::stderr(), &chunk[..read]);
        }
    });
    Ok(AgentLink::new_over(
        child,
        Box::new(stdin),
        Box::new(stdout),
        preamble,
    ))
}

/// Reads `stdout` line by line into `before`, and `stderr` into `said`, until a line of stdout
/// that is `success`, which is not kept: `false` when stdout ends first.
async fn until_success(
    stdout: &mut (impl AsyncBufRead + Unpin),
    stderr: &mut (impl AsyncRead + Unpin),
    before: &mut Vec<u8>,
    said: &mut Vec<u8>,
    success: &str,
) -> bool {
    let mut line = Vec::new();
    let mut chunk = [0; 4096];
    let mut open = true;
    loop {
        // A line cut short by the other branch stays in `line`, and the next read goes on with it.
        tokio::select! {
            read = stdout.read_until(b'\n', &mut line) => match read {
                Ok(0) | Err(_) => {
                    before.extend_from_slice(&line);
                    return false;
                }
                Ok(_) if line.trim_ascii_end() == success.as_bytes() => return true,
                Ok(_) => {
                    before.extend_from_slice(&line);
                    line.clear();
                }
            },
            read = stderr.read(&mut chunk), if open => match read {
                Ok(0) | Err(_) => open = false,
                Ok(read) => said.extend_from_slice(&chunk[..read]),
            },
        }
    }
}

/// The reference's words for a become that did not succeed, with what each stream said.
fn waiting_message(reason: &str, stdout: &[u8], stderr: &[u8]) -> String {
    let mut message = format!("{reason} waiting for become success.");
    for (name, said) in [("Output", stdout), ("Error", stderr)] {
        if !said.is_empty() {
            let said = String::from_utf8_lossy(said);
            let _ = write!(message, "\n>>> Standard {name}\n{said}");
        }
    }
    message
}

#[cfg(test)]
mod tests {
    use super::{until_success, waiting_message};

    #[tokio::test]
    async fn what_comes_before_the_success_line_is_not_the_agent_s() {
        let success = "VOLANT-BECOME-SUCCESS-1";
        let mut stdout: &[u8] = b"stdout noise\nVOLANT-BECOME-SUCCESS-1\n\0\0\0\x05frame";
        let mut stderr: &[u8] = b"stderr noise\n";
        let (mut before, mut said) = (Vec::new(), Vec::new());
        assert!(until_success(&mut stdout, &mut stderr, &mut before, &mut said, success).await);
        assert_eq!(before, b"stdout noise\n");
        assert_eq!(stdout, b"\0\0\0\x05frame");

        let mut stdout: &[u8] = b"no success here\n";
        let mut stderr: &[u8] = b"";
        let (mut before, mut said) = (Vec::new(), Vec::new());
        assert!(!until_success(&mut stdout, &mut stderr, &mut before, &mut said, success).await);
        assert_eq!(
            waiting_message("Premature end of stream", &before, b"err"),
            "Premature end of stream waiting for become success.\n>>> Standard Output\nno success here\n\n>>> Standard Error\nerr"
        );
    }
}
