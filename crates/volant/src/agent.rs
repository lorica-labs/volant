// SPDX-License-Identifier: GPL-3.0-or-later
//! Finding the agent binary and talking to a running agent.

use std::path::PathBuf;

use anyhow::{Context, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::mpsc;
use volant_protocol::frame::{header, payload_len};
use volant_protocol::{FromAgent, PROTOCOL_VERSION, ToAgent};

/// Where agent binaries live: `VOLANT_AGENT_DIR`, then next to this executable. The local
/// agent is `volant-agent`; cross-built ones are `volant-agent-<target triple>`. Embedding them
/// in the controller comes with the release packaging.
#[derive(Debug, Clone)]
pub struct AgentSource {
    dirs: Vec<PathBuf>,
}

impl AgentSource {
    pub fn discover() -> Self {
        let mut dirs = Vec::new();
        if let Ok(dir) = std::env::var("VOLANT_AGENT_DIR") {
            dirs.push(PathBuf::from(dir));
        }
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            dirs.push(dir.to_path_buf());
        }
        Self { dirs }
    }

    pub fn local(&self) -> anyhow::Result<PathBuf> {
        let file = format!("volant-agent{}", std::env::consts::EXE_SUFFIX);
        self.find(&file).with_context(|| {
            format!(
                "no executable agent binary '{file}' in {}. Set VOLANT_AGENT_DIR to the directory that holds it",
                self.describe()
            )
        })
    }

    pub fn for_target(&self, triple: &str) -> Option<PathBuf> {
        self.find(&format!("volant-agent-{triple}"))
    }

    pub fn describe(&self) -> String {
        self.dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn find(&self, file: &str) -> Option<PathBuf> {
        self.dirs.iter().map(|d| d.join(file)).find(|p| runnable(p))
    }
}

/// A file that carries an execute bit. A stray artifact with the right name is not an agent:
/// the local one would fail to spawn, and a cross-built one would be uploaded, made
/// executable on the host and then refuse to run there.
#[cfg(unix)]
fn runnable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn runnable(path: &std::path::Path) -> bool {
    path.is_file()
}

/// How long a bare `drop` waits for the agent to notice end of stream and clean up its task
/// before giving up and letting `kill_on_drop` kill the agent outright. This blocks whatever
/// thread runs the drop (there is no `.await` in `Drop::drop`), so it has to stay small; the
/// agent itself polls for cancellation every 20ms, so a plain multiple of that covers the
/// common case without making a wedged agent stall the caller for long.
const DROP_GRACE: std::time::Duration = std::time::Duration::from_millis(200);
const DROP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(5);

/// A running agent reached through a transport: frames in, frames out.
///
/// `stdin` is an `Option` so `Drop` can close it explicitly, ahead of waiting on `child`, even
/// though a type with a manual `Drop` impl cannot otherwise move a field out of itself.
pub struct AgentLink {
    stdin: Option<ChildStdin>,
    frames: mpsc::Receiver<std::io::Result<Vec<u8>>>,
    child: Child,
}

impl AgentLink {
    pub fn new(mut child: Child) -> anyhow::Result<Self> {
        let stdin = child.stdin.take().context("agent stdin is not piped")?;
        let stdout = BufReader::new(child.stdout.take().context("agent stdout is not piped")?);
        // Owning the stdout reader in its own task, rather than reading it directly inside
        // `recv`, is what makes `recv` cancellation-safe: dropping its future only drops a
        // channel receive, never a partially read frame.
        let (tx, rx) = mpsc::channel(1);
        tokio::spawn(read_frames(stdout, tx));
        Ok(Self {
            stdin: Some(stdin),
            frames: rx,
            child,
        })
    }

    /// The same, having first written `preamble` to the child's stdin unframed. That is how a
    /// `sudo -S` reading the escalation password gets it: on the pipe, ahead of every frame,
    /// and never on a command line, in the environment or through a `format!`.
    pub async fn new_with_preamble(
        child: Child,
        preamble: Option<Vec<u8>>,
    ) -> anyhow::Result<Self> {
        let mut link = Self::new(child)?;
        if let Some(bytes) = preamble {
            link.stdin()
                .write_all(&bytes)
                .await
                .context("writing the escalation password")?;
            link.stdin()
                .flush()
                .await
                .context("writing the escalation password")?;
        }
        Ok(link)
    }

    fn stdin(&mut self) -> &mut ChildStdin {
        self.stdin.as_mut().expect("stdin is only taken on drop")
    }

    pub async fn send(&mut self, msg: &ToAgent) -> std::io::Result<()> {
        let payload = serde_json::to_vec(msg)?;
        self.stdin().write_all(&header(payload.len())?).await?;
        self.stdin().write_all(&payload).await?;
        self.stdin().flush().await
    }

    /// `Ok(None)` when the agent closed its stdout cleanly. Awaits a channel fed by a
    /// dedicated reader task, so a dropped `recv` future never desyncs the stream.
    pub async fn recv(&mut self) -> std::io::Result<Option<FromAgent>> {
        match self.frames.recv().await {
            Some(Ok(payload)) => Ok(Some(serde_json::from_slice(&payload)?)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
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

    /// Asks the agent to stop batch `id` and waits for it to say so, at most `grace`.
    /// Returns `false` when the agent went away or did not answer in time.
    pub async fn cancel(&mut self, id: u64, grace: std::time::Duration) -> bool {
        if self.send(&ToAgent::Cancel { id }).await.is_err() {
            return false;
        }
        let confirmed = async {
            loop {
                match self.recv().await {
                    Ok(Some(FromAgent::BatchDone { batch, .. })) if batch == id => return true,
                    Ok(Some(_)) => continue,
                    Ok(None) | Err(_) => return false,
                }
            }
        };
        tokio::time::timeout(grace, confirmed)
            .await
            .unwrap_or(false)
    }

    /// Kills the agent process if it is still running.
    pub async fn shutdown(mut self) {
        drop(self.stdin.take());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), self.child.wait()).await;
        let _ = self.child.start_kill();
    }
}

impl Drop for AgentLink {
    /// A bare `drop` (no call to `shutdown`) has no `.await` to wait on the agent with, so
    /// this closes stdin and then busy-polls `try_wait` for up to `DROP_GRACE`, giving the
    /// agent a scheduling window to see end of stream and kill its task's process group
    /// before this returns. If the agent is still alive once the grace period runs out, this
    /// falls through and lets the `child` field's own `kill_on_drop` kill it as before.
    fn drop(&mut self) {
        drop(self.stdin.take());
        let deadline = std::time::Instant::now() + DROP_GRACE;
        while std::time::Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => std::thread::sleep(DROP_POLL_INTERVAL),
            }
        }
    }
}

/// Reads whole frames off the agent's stdout in a loop and forwards each outcome: a payload,
/// a read error, or nothing at all once the stream ends cleanly (dropping the sender, which
/// is what turns the matching `recv` into `Ok(None)`).
async fn read_frames(
    mut stdout: BufReader<ChildStdout>,
    tx: mpsc::Sender<std::io::Result<Vec<u8>>>,
) {
    loop {
        match read_frame(&mut stdout).await {
            Ok(Some(payload)) => {
                if tx.send(Ok(payload)).await.is_err() {
                    return;
                }
            }
            Ok(None) => return,
            Err(err) => {
                let _ = tx.send(Err(err)).await;
                return;
            }
        }
    }
}

/// Async twin of `volant_protocol::frame::read_frame`: reads the first length byte alone so
/// a clean end of stream (`Ok(None)`) is distinguished from the stream closing partway
/// through the length prefix or the payload, both of which are errors.
async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut first = [0u8; 1];
    if r.read(&mut first).await? == 0 {
        return Ok(None);
    }
    let mut rest = [0u8; 3];
    r.read_exact(&mut rest).await?;
    let len = payload_len([first[0], rest[0], rest[1], rest[2]])?;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    Ok(Some(payload))
}
