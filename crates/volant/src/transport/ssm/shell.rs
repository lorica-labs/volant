// SPDX-License-Identifier: GPL-3.0-or-later
//! The one shell a Session Manager session gives, running the bootstrap's commands one after the
//! other, then the agent.
//!
//! What it does to the session first:
//! - `\n~-` turns off session-manager-plugin's escape sequences, under which a `~.` after a
//!   newline would end the session.
//! - The prompts are emptied, line editing is turned off, and the terminal is put in raw mode
//!   with no echo. The output then holds only what the commands print, and every byte value
//!   sent reaches them.
//!
//! Each command runs under `sh -c` with its stderr in a file. Marker lines follow its output:
//! one, then the stderr, then one more with the exit status. Each marker is printed by `printf`
//! from two halves, so a command line echoed before the terminal was raw cannot match it. A
//! command's payload goes on the terminal after the command line, where `head -c` takes it.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};

use crate::agent::AgentLink;
use crate::transport::bootstrap::Captured;
use crate::transport::defaults::ConnectError;
use crate::transport::trace::shell_quote;

pub(in crate::transport) struct Shell {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,
    /// What the session printed past the last marker read.
    buf: Vec<u8>,
    timeout: Duration,
    /// This session's markers, and how many have been used.
    nonce: String,
    used: u64,
}

impl Shell {
    /// Waits for the session to start, then readies its shell. A session that never starts says
    /// why on the starter's stderr: boto3's refusal, such as an instance that is not connected.
    pub(in crate::transport) async fn open(
        mut child: Child,
        timeout: Duration,
    ) -> Result<Shell, ConnectError> {
        let taken = |what| ConnectError::Unreachable(format!("the session's {what} is not piped"));
        let mut shell = Shell {
            stdin: child.stdin.take().ok_or_else(|| taken("stdin"))?,
            stdout: child.stdout.take().ok_or_else(|| taken("stdout"))?,
            stderr: child.stderr.take().ok_or_else(|| taken("stderr"))?,
            child,
            buf: Vec::new(),
            timeout,
            nonce: nonce(),
            used: 0,
        };
        shell.read_until(b"Starting session with SessionId").await?;
        let ready = shell.marker("UP");
        shell
            .send(
                format!(
                    "\n~-\nPS1= PS2=; (set +o emacs +o vi) 2>/dev/null && set +o emacs +o vi; stty raw -echo; \
                     printf '\\nVOLANT%s\\n' {}\n",
                    ready.trim_start_matches("VOLANT")
                )
                .as_bytes(),
            )
            .await?;
        shell.read_until(format!("\n{ready}\n").as_bytes()).await?;
        Ok(shell)
    }

    /// Runs `command`, with `payload` on its stdin, and says what it printed and how it ended.
    pub(in crate::transport) async fn run(
        &mut self,
        command: &str,
        payload: Option<&[u8]>,
    ) -> Result<Captured, ConnectError> {
        let go = self.marker("GO");
        let err = self.marker("ERR");
        let end = self.marker("END");
        let line = format!(
            "e=$(mktemp) || e=/dev/null; printf '\\nVOLANT%s\\n' {}; sh -c {} 2>\"$e\"; c=$?; \
             printf '\\nVOLANT%s\\n' {}; cat \"$e\"; rm -f \"$e\"; \
             printf '\\nVOLANT%s %s\\n' {} \"$c\"\n",
            go.trim_start_matches("VOLANT"),
            shell_quote(command),
            err.trim_start_matches("VOLANT"),
            end.trim_start_matches("VOLANT"),
        );
        self.send(line.as_bytes()).await?;
        // The shell has read the whole line once it prints this: the payload then reaches the
        // command, never the shell.
        self.read_until(format!("\n{go}\n").as_bytes()).await?;
        if let Some(payload) = payload {
            self.send(payload).await?;
        }
        let stdout = self.read_until(format!("\n{err}\n").as_bytes()).await?;
        let stderr = self.read_until(format!("\n{end} ").as_bytes()).await?;
        let code = self.read_until(b"\n").await?;
        Ok(Captured {
            code: String::from_utf8_lossy(&code).trim().parse().ok(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }

    /// Starts the agent with `command`, writes `preamble` (a `sudo` password) as it is, and
    /// hands the session over to the link.
    pub(in crate::transport) async fn start(
        mut self,
        command: &str,
        preamble: Option<Vec<u8>>,
        stderr_file: String,
    ) -> Result<AgentLink, ConnectError> {
        let go = self.marker("GO");
        let line = format!(
            "printf '\\nVOLANT%s\\n' {}; exec sh -c {}\n",
            go.trim_start_matches("VOLANT"),
            shell_quote(command)
        );
        self.send(line.as_bytes()).await?;
        // Neither the password nor the first frame may reach the shell itself.
        self.read_until(format!("\n{go}\n").as_bytes()).await?;
        if let Some(bytes) = preamble {
            self.send(&bytes).await?;
        }
        // What the plugin says from now on goes nowhere, so that a full pipe never stops it.
        let mut stderr = self.stderr;
        tokio::spawn(async move {
            let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
        });
        let stdout = std::io::Cursor::new(std::mem::take(&mut self.buf)).chain(self.stdout);
        Ok(AgentLink::new_over_lines(
            self.child,
            Box::new(self.stdin),
            Box::new(stdout),
            stderr_file,
        ))
    }

    fn marker(&mut self, what: &str) -> String {
        self.used += 1;
        format!("VOLANT-{what}-{}-{}", self.nonce, self.used)
    }

    async fn send(&mut self, bytes: &[u8]) -> Result<(), ConnectError> {
        let written = async {
            self.stdin.write_all(bytes).await?;
            self.stdin.flush().await
        };
        match tokio::time::timeout(self.timeout, written).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(self.ended().await),
            Err(_) => Err(self.late()),
        }
    }

    /// What the session printed up to `marker`, which is read and left out.
    async fn read_until(&mut self, marker: &[u8]) -> Result<Vec<u8>, ConnectError> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            if let Some(at) = self
                .buf
                .windows(marker.len())
                .position(|window| window == marker)
            {
                let rest = self.buf.split_off(at + marker.len());
                let mut said = std::mem::replace(&mut self.buf, rest);
                said.truncate(at);
                return Ok(said);
            }
            let mut chunk = [0u8; 8192];
            match tokio::time::timeout_at(deadline, self.stdout.read(&mut chunk)).await {
                Ok(Ok(0) | Err(_)) => return Err(self.ended().await),
                Ok(Ok(n)) => self.buf.extend_from_slice(&chunk[..n]),
                Err(_) => return Err(self.late()),
            }
        }
    }

    /// The session ended: what the starter or the plugin said on its way out.
    async fn ended(&mut self) -> ConnectError {
        let mut said = Vec::new();
        let _ =
            tokio::time::timeout(Duration::from_secs(5), self.stderr.read_to_end(&mut said)).await;
        let said = String::from_utf8_lossy(&said);
        let said = said.trim();
        ConnectError::Unreachable(if said.is_empty() {
            "the session ended before the agent started".to_string()
        } else {
            said.to_string()
        })
    }

    fn late(&self) -> ConnectError {
        let seconds = self.timeout.as_secs();
        ConnectError::Unreachable(format!(
            "the session did not answer within {seconds} second{}",
            if seconds == 1 { "" } else { "s" }
        ))
    }
}

/// A word no command prints by chance, from the clock and the process.
fn nonce() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    format!("{:x}{:x}", std::process::id(), nanos)
}
