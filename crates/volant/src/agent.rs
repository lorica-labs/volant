// SPDX-License-Identifier: GPL-3.0-or-later
//! Finding the agent binary and talking to a running agent.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use anyhow::{Context, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::mpsc;
use volant_protocol::frame::{header, payload_len};
use volant_protocol::{BlobEncoding, FromAgent, PROTOCOL_VERSION, ToAgent};

pub(crate) mod embedded;

/// Where agent binaries live, looked up in this order: `VOLANT_AGENT_DIR`, the agents built into
/// this controller (extracted to a cache directory on first use), then the directory of this
/// executable. The local agent is `volant-agent`; cross-built ones are
/// `volant-agent-<target triple>`.
#[derive(Debug, Clone)]
pub struct AgentSource {
    places: Vec<Place>,
    read: Arc<Mutex<HashMap<PathBuf, Arc<AgentFile>>>>,
}

/// An agent binary as read off disk once, with its blake3 hash in hex: the bytes a link uploads
/// and the hash it compares a cached agent with are always the same bytes.
#[derive(Debug)]
pub struct AgentFile {
    pub bytes: Vec<u8>,
    pub hash: String,
}

#[derive(Debug, Clone)]
enum Place {
    Dir(PathBuf),
    Embedded(embedded::Embedded),
}

impl AgentSource {
    pub fn discover() -> Self {
        let exe_dir = std::env::current_exe().ok().and_then(beside_executable);
        Self::ordered(
            std::env::var_os("VOLANT_AGENT_DIR").map(PathBuf::from),
            embedded::Embedded::built_in(),
            exe_dir,
        )
    }

    /// The lookup order, apart from where each place comes from.
    fn ordered(
        env_dir: Option<PathBuf>,
        embedded: Option<embedded::Embedded>,
        exe_dir: Option<PathBuf>,
    ) -> Self {
        let places = env_dir
            .map(Place::Dir)
            .into_iter()
            .chain(embedded.map(Place::Embedded))
            .chain(exe_dir.map(Place::Dir))
            .collect();
        Self {
            places,
            read: Arc::default(),
        }
    }

    /// The agent file at `path`, read and hashed on the first call and shared by every link of
    /// the run after that, clones of this source included. A file rebuilt during a run is not
    /// read again.
    pub fn read(&self, path: &Path) -> std::io::Result<Arc<AgentFile>> {
        if let Some(file) = self.lock().get(path) {
            return Ok(Arc::clone(file));
        }
        let bytes = std::fs::read(path)?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        Ok(Arc::clone(
            self.lock()
                .entry(path.to_path_buf())
                .or_insert_with(|| Arc::new(AgentFile { bytes, hash })),
        ))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Arc<AgentFile>>> {
        self.read.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn local(&self) -> anyhow::Result<PathBuf> {
        let file = format!("volant-agent{}", std::env::consts::EXE_SUFFIX);
        self.find(&file).map_err(|looked| {
            anyhow::anyhow!(
                "no executable agent binary '{file}' in {looked}. Set VOLANT_AGENT_DIR to the directory that holds it"
            )
        })
    }

    /// The agent to upload to a host of this target triple, or the places it was looked for in.
    pub fn for_target(&self, triple: &str) -> Result<PathBuf, String> {
        self.find(&format!("volant-agent-{triple}"))
    }

    /// The first place holding `file`, or every place looked in when none does, each with the
    /// reason it could not be used when there is one.
    fn find(&self, file: &str) -> Result<PathBuf, String> {
        let mut looked = Vec::new();
        for place in &self.places {
            match place {
                Place::Dir(dir) => {
                    let path = dir.join(file);
                    if runnable(&path) {
                        return Ok(path);
                    }
                    looked.push(dir.display().to_string());
                }
                Place::Embedded(embedded) => match embedded.find(file) {
                    Ok(Some(path)) => return Ok(path),
                    Ok(None) => looked.push(embedded.describe()),
                    Err(err) => looked.push(format!("{}: {err}", embedded.describe())),
                },
            }
        }
        Err(looked.join(", "))
    }
}

/// The directory the agents ship in: the one holding the controller's own file, behind any link.
///
/// Linux answers `current_exe` with the resolved file, macOS with the path the program was
/// started through. An install that keeps the release directory whole and links `volant` onto
/// the `PATH`, which is what the install script does, would otherwise look for the agents in
/// the directory of the link.
fn beside_executable(exe: PathBuf) -> Option<PathBuf> {
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    exe.parent().map(Path::to_path_buf)
}

/// A file that carries an execute bit. A stray artifact with the right name is not an agent:
/// the local one would fail to spawn, and a cross-built one would be uploaded, made
/// executable on the host and then refuse to run there.
#[cfg(unix)]
fn runnable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn runnable(path: &Path) -> bool {
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

/// A blob as it goes on the wire in the frame after its `put_blob`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobFrame {
    /// The decoded length, which the `put_blob` announces.
    pub len: u64,
    pub encoding: BlobEncoding,
    pub bytes: Vec<u8>,
}

impl BlobFrame {
    /// `raw` as it is.
    pub fn raw(raw: Vec<u8>) -> BlobFrame {
        BlobFrame {
            len: raw.len() as u64,
            encoding: BlobEncoding::Raw,
            bytes: raw,
        }
    }

    /// `deflated` when it is shorter than `raw`, whose deflated form it is, and `raw` otherwise:
    /// the agent refuses a frame longer than the blob, so a deflate that grew is never sent.
    pub fn smaller(raw: Vec<u8>, deflated: Vec<u8>) -> BlobFrame {
        if deflated.len() < raw.len() {
            BlobFrame {
                len: raw.len() as u64,
                encoding: BlobEncoding::Deflate,
                bytes: deflated,
            }
        } else {
            BlobFrame::raw(raw)
        }
    }

    /// `raw`, deflated first when it is past 4 KiB and that makes it shorter. Under that, the
    /// deflate costs more than the bytes it saves. Past 64 KiB, the first 64 KiB are deflated
    /// alone first, and bytes whose sample does not shrink by a tenth (an archive, an image, a
    /// binary) go raw without paying a deflate of the whole file.
    pub fn packed(raw: Vec<u8>) -> BlobFrame {
        const SAMPLE: usize = 64 * 1024;
        if raw.len() <= 4096 {
            return BlobFrame::raw(raw);
        }
        if raw.len() > SAMPLE
            && volant_protocol::encoding::deflate(&raw[..SAMPLE]).len() > SAMPLE / 10 * 9
        {
            return BlobFrame::raw(raw);
        }
        #[cfg(test)]
        DEFLATED.fetch_add(raw.len() as u64, std::sync::atomic::Ordering::Relaxed);
        let deflated = volant_protocol::encoding::deflate(&raw);
        BlobFrame::smaller(raw, deflated)
    }
}

/// How many bytes [`BlobFrame::packed`] deflated whole, so a test can tell a deflate that ran
/// from one that was skipped or shared.
#[cfg(test)]
static DEFLATED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One blob's frame, made by the first caller that needs it while the others wait for it.
type Slot = Arc<OnceLock<Result<Arc<BlobFrame>, String>>>;

/// The frame each blob this process sends goes up in, by the blake3 of its content: the union's,
/// kept by the union cache when it stores or loads the entry, and each staged file's, made on its
/// first send. So a blob sent to every host of a run is decoded and deflated once, not once per
/// host.
///
/// Keyed by the content hash alone, so an entry here can never be stale: other bytes are another
/// key.
static FRAMES: Mutex<BTreeMap<String, Slot>> = Mutex::new(BTreeMap::new());

/// Keeps `frame` as the one the blob named `hash` goes up in.
pub fn keep_frame(hash: &str, frame: BlobFrame) {
    FRAMES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(
            hash.to_string(),
            Arc::new(OnceLock::from(Ok(Arc::new(frame)))),
        );
}

/// Forgets every frame this process kept, as a new run starts with none.
#[cfg(test)]
pub fn forget_frames() {
    FRAMES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
}

/// The frame the blob named `hash`, whose bytes are `b64`, goes up in: the one kept for it, or
/// made now from `b64` by [`BlobFrame::packed`], once for the whole process however many hosts
/// ask at the same moment, and on a blocking thread rather than on an async worker.
pub async fn frame(hash: &str, b64: &str) -> Result<Arc<BlobFrame>, String> {
    let slot = Arc::clone(
        FRAMES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(hash.to_string())
            .or_default(),
    );
    if let Some(made) = slot.get() {
        return made.clone();
    }
    let (named, b64) = (hash.to_string(), b64.to_string());
    tokio::task::spawn_blocking(move || {
        slot.get_or_init(|| {
            volant_protocol::encoding::b64_decode(&b64)
                .map(|raw| Arc::new(BlobFrame::packed(raw)))
                .map_err(|err| format!("decoding the blob {named}: {err}"))
        })
        .clone()
    })
    .await
    .map_err(|err| format!("framing the blob {hash}: {err}"))?
}

/// What one link knows about the module payloads the agent behind it holds: `Ok` for a payload
/// the agent confirmed, `Err` for one it refused, with the sentence the refusal failed under.
///
/// It belongs to the link and to nothing longer-lived, which is the whole answer to a
/// connection lost and reopened: the new link is a new `AgentLink` carrying an empty memory, so
/// its first batch asks again rather than assuming what the link before it was told. A memory
/// kept per host, or per run, would send a batch that needs a payload to an agent that never
/// received one.
#[derive(Debug, Default)]
pub struct BlobMemory(HashMap<String, Result<(), String>>);

impl BlobMemory {
    /// What this link was told about `hash`, if it has been told anything.
    pub fn seen(&self, hash: &str) -> Option<Result<(), String>> {
        self.0.get(hash).cloned()
    }

    pub fn remember(&mut self, hash: &str, state: Result<(), String>) {
        self.0.insert(hash.to_string(), state);
    }

    /// Whether the agent behind this link confirmed holding this payload. A refusal is not a
    /// hold, which is why this reads the value rather than the key.
    pub fn holds(&self, hash: &str) -> bool {
        matches!(self.0.get(hash), Some(Ok(())))
    }
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
