// SPDX-License-Identifier: GPL-3.0-or-later
//! Finding the agent binary and talking to a running agent.

use anyhow::{Context, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::mpsc;
use volant_protocol::frame::{header, payload_len};
use volant_protocol::{FromAgent, PROTOCOL_VERSION, ToAgent};

mod blobs;
pub(crate) mod embedded;
mod source;

#[cfg(test)]
pub use blobs::forget_frames;
pub use blobs::{BlobFrame, BlobMemory, frame, keep_frame};
pub use source::AgentSource;

/// How long a bare `drop` waits for the agent to notice end of stream and clean up its task
/// before giving up and letting `kill_on_drop` kill the agent outright. This blocks whatever
/// thread runs the drop (there is no `.await` in `Drop::drop`), so it has to stay small; the
/// agent itself polls for cancellation every 50ms, and a loaded host can take several polls
/// before it has killed the task's process group and exited. The wait ends the moment the agent
/// exits, so the full second is only paid by an agent that is wedged; killing one that was
/// stopping cleanly would leave its task running on the host.
const DROP_GRACE: std::time::Duration = std::time::Duration::from_millis(1000);
const DROP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(5);

/// A running agent reached through a transport: frames in, frames out.
///
/// `stdin` is an `Option` so `Drop` can close it explicitly, ahead of waiting on `child`, even
/// though a type with a manual `Drop` impl cannot otherwise move a field out of itself.
pub struct AgentLink {
    stdin: Option<ChildStdin>,
    frames: mpsc::Receiver<std::io::Result<Vec<u8>>>,
    child: Child,
    blobs: BlobMemory,
    interpreters: Vec<String>,
    natives: Vec<String>,
    ledger: crate::profile::Ledger,
    /// Whether the agent reads a terminal rather than a pipe, as it does behind `su`. Closing
    /// stdin then reaches nobody, so the link ends by stopping `ssh`, whose hangup is the end
    /// of input the agent stops on.
    tty: bool,
    /// Where, on the host, the agent's stderr goes when it cannot share the link's stream.
    stderr_file: Option<String>,
    /// Whether this is [`AgentLink::dead`]'s stand-in for a host that never answered.
    stand_in: bool,
    /// The line `ssh -tt` ends a command's stderr with, for a link over ssh that gives commands a
    /// terminal; `None` for any other.
    tty_target: Option<crate::transport::SshTarget>,
    /// The line, once asked for: `Some(None)` is an answer of no line.
    tty_line: Option<Option<String>>,
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
            blobs: BlobMemory::default(),
            interpreters: Vec::new(),
            natives: Vec::new(),
            ledger: crate::profile::Ledger::default(),
            tty: false,
            stderr_file: None,
            stand_in: false,
            tty_target: None,
            tty_line: None,
        })
    }

    pub(crate) fn set_tty_target(&mut self, target: Option<crate::transport::SshTarget>) {
        self.tty_target = target;
        self.tty_line = None;
    }

    /// The line a command given a terminal ends its stderr with, worked out the first time a
    /// command needs it so a play with no such command pays nothing for it.
    pub async fn tty_close(&mut self) -> Option<String> {
        if self.tty_line.is_none() {
            let line = match &self.tty_target {
                Some(target) => target.tty_close().await,
                None => None,
            };
            self.tty_line = Some(line);
        }
        self.tty_line.clone().flatten()
    }

    /// A link whose agent is already gone: the first batch asked over it fails as a lost
    /// connection does, which is what `wait_for_connection` expects of a host that is down.
    pub fn dead() -> anyhow::Result<Self> {
        let child = tokio::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .context("could not start the stand-in for a host that is down")?;
        let mut link = Self::new(child)?;
        link.stand_in = true;
        Ok(link)
    }

    /// Whether this link is [`AgentLink::dead`]'s, which no batch may be left to run on.
    pub fn is_stand_in(&self) -> bool {
        self.stand_in
    }

    /// The same, for an agent whose stdin and stdout are a remote terminal that `ssh -tt`
    /// carries (see the `tty` field), and whose stderr is `stderr_file` on the host.
    pub fn new_over_tty(child: Child, stderr_file: String) -> anyhow::Result<Self> {
        let mut link = Self::new(child)?;
        link.tty = true;
        link.stderr_file = Some(stderr_file);
        Ok(link)
    }

    /// Where to look for what an agent that stopped said, for a message about it: empty when
    /// its stderr reached the controller's own.
    pub fn stderr_note(&self) -> String {
        self.stderr_file
            .as_ref()
            .map(|file| format!(" (its stderr is in {file} on the host)"))
            .unwrap_or_default()
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

    /// The payloads this link has already asked about. Lives here so it dies with the link.
    pub fn blobs(&mut self) -> &mut BlobMemory {
        &mut self.blobs
    }

    /// The Python interpreters the agent behind this link reported at the handshake, as absolute
    /// paths, best first.
    ///
    /// Empty means **this agent reported none**, which is not the same as this host having none:
    /// an agent older than the field says nothing about interpreters and looks identical on the
    /// wire to a host with no Python at all. Anything refusing on this has to say so that way.
    pub fn interpreters(&self) -> &[String] {
        &self.interpreters
    }

    /// The native modules the agent behind this link reported enabled at the last handshake.
    pub fn natives(&self) -> &[String] {
        &self.natives
    }

    /// What the batches over this link measured since it was last asked.
    pub fn ledger(&mut self) -> &mut crate::profile::Ledger {
        &mut self.ledger
    }

    pub fn take_ledger(&mut self) -> crate::profile::Ledger {
        std::mem::take(&mut self.ledger)
    }

    fn stdin(&mut self) -> &mut ChildStdin {
        self.stdin.as_mut().expect("stdin is only taken on drop")
    }

    pub async fn send(&mut self, msg: &ToAgent) -> std::io::Result<()> {
        self.send_bytes(&serde_json::to_vec(msg)?).await
    }

    /// Writes `bytes` as one frame, as they are: a message's JSON, or the blob a `put_blob` is
    /// followed by.
    pub async fn send_bytes(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.stdin().write_all(&header(bytes.len())?).await?;
        self.stdin().write_all(bytes).await?;
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
                Some(FromAgent::Ready {
                    protocol,
                    interpreters,
                    natives,
                    ..
                }) if protocol == PROTOCOL_VERSION => {
                    // Taken on every handshake, the liveness check on a kept link included, so a
                    // host whose Python changed under a link that survived a play is read as it
                    // is now rather than as it was when the link was opened.
                    self.interpreters = interpreters;
                    self.natives = natives;
                    return Ok(());
                }
                Some(FromAgent::Ready {
                    protocol, version, ..
                }) => {
                    bail!(
                        "agent {version} speaks protocol {protocol}, this controller speaks {PROTOCOL_VERSION}"
                    )
                }
                Some(FromAgent::Log { .. }) => {}
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
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => return false,
                }
            }
        };
        tokio::time::timeout(grace, confirmed)
            .await
            .unwrap_or(false)
    }

    /// Kills the agent without waiting, so a late answer to a question given up on is never
    /// read as another's: every later use of the link finds it closed.
    pub(crate) fn abandon(&mut self) {
        let _ = self.child.start_kill();
    }

    /// Kills the agent and waits for it, leaving the link in place: what a host that rebooted
    /// leaves a controller holding.
    #[cfg(all(test, unix))]
    pub(crate) async fn kill(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }

    /// Kills the agent process if it is still running.
    pub async fn shutdown(mut self) {
        drop(self.stdin.take());
        if self.tty {
            let _ = self.child.start_kill();
        }
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
        // Nothing reaches an agent behind a terminal until `ssh` goes, which `kill_on_drop`
        // sees to as soon as this returns.
        if self.tty {
            return;
        }
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

#[cfg(all(test, unix))]
mod tests;
