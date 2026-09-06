// SPDX-License-Identifier: GPL-3.0-or-later
//! Finding the agent binary and talking to a running agent.

use std::path::PathBuf;

use anyhow::{Context, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use volant_protocol::frame::MAX_FRAME_LEN;
use volant_protocol::{FromAgent, PROTOCOL_VERSION, ToAgent};

/// Where the agent binary is: `VOLANT_AGENT_DIR`, then next to this executable.
/// Embedding the agent in the controller comes with the release packaging.
pub fn locate() -> anyhow::Result<PathBuf> {
    let file = format!("volant-agent{}", std::env::consts::EXE_SUFFIX);
    let mut candidates = Vec::new();
    if let Ok(dir) = std::env::var("VOLANT_AGENT_DIR") {
        candidates.push(PathBuf::from(dir).join(&file));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join(&file));
    }
    candidates.iter().find(|p| p.is_file()).cloned().with_context(|| {
        format!(
            "agent binary '{file}' not found; looked in {}. Set VOLANT_AGENT_DIR to the directory that holds it",
            candidates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        )
    })
}

/// A running agent reached through a transport: frames in, frames out.
pub struct AgentLink {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl AgentLink {
    pub fn new(mut child: Child) -> anyhow::Result<Self> {
        let stdin = child.stdin.take().context("agent stdin is not piped")?;
        let stdout = BufReader::new(child.stdout.take().context("agent stdout is not piped")?);
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    pub async fn send(&mut self, msg: &ToAgent) -> std::io::Result<()> {
        let payload = serde_json::to_vec(msg)?;
        let len =
            u32::try_from(payload.len()).map_err(|_| std::io::Error::other("frame too large"))?;
        self.stdin.write_all(&len.to_be_bytes()).await?;
        self.stdin.write_all(&payload).await?;
        self.stdin.flush().await
    }

    /// `Ok(None)` when the agent closed its stdout.
    pub async fn recv(&mut self) -> std::io::Result<Option<FromAgent>> {
        let mut len = [0u8; 4];
        match self.stdout.read_exact(&mut len).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let len = u32::from_be_bytes(len) as usize;
        if len > MAX_FRAME_LEN {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "frame too large",
            ));
        }
        let mut payload = vec![0u8; len];
        self.stdout.read_exact(&mut payload).await?;
        Ok(Some(serde_json::from_slice(&payload)?))
    }

    pub async fn handshake(&mut self) -> anyhow::Result<()> {
        self.send(&ToAgent::Hello {
            protocol: PROTOCOL_VERSION,
        })
        .await
        .context("sending hello")?;
        loop {
            match self
                .recv()
                .await
                .context("waiting for the agent to answer")?
            {
                Some(FromAgent::Ready { protocol, .. }) if protocol == PROTOCOL_VERSION => {
                    return Ok(());
                }
                Some(FromAgent::Ready {
                    protocol, version, ..
                }) => {
                    bail!(
                        "agent {version} speaks protocol {protocol}, this controller speaks {PROTOCOL_VERSION}"
                    )
                }
                Some(FromAgent::Log { .. }) => continue,
                Some(other) => bail!("unexpected message before ready: {other:?}"),
                None => bail!("agent exited before answering"),
            }
        }
    }

    /// Kills the agent process if it is still running.
    pub async fn shutdown(mut self) {
        drop(self.stdin);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), self.child.wait()).await;
        let _ = self.child.start_kill();
    }
}
