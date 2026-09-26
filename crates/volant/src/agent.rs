// SPDX-License-Identifier: GPL-3.0-or-later
//! Finding the agent binary and talking to a running agent.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::mpsc;
use volant_protocol::frame::{header, payload_len};
use volant_protocol::{FromAgent, PROTOCOL_VERSION, ToAgent};

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
        self.read
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    const EMBEDDED: &[(&str, &[u8])] = &[("volant-agent", b"embedded")];

    fn agent_in(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, b"on disk").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Every link of a run compares against, and uploads, the bytes the first one read.
    ///
    /// What would make this red: `read` going back to the disk each time, which the rewritten
    /// file below would show through the clone.
    #[test]
    fn an_agent_file_is_read_and_hashed_once_per_source() {
        let dir = std::env::temp_dir().join(format!("volant-agent-read-{}", std::process::id()));
        let path = agent_in(&dir, "volant-agent-x");
        let source = AgentSource::ordered(Some(dir.clone()), None, None);
        let first = source.read(&path).unwrap();
        assert_eq!(first.bytes, b"on disk");
        assert_eq!(first.hash, blake3::hash(b"on disk").to_hex().to_string());
        std::fs::write(&path, b"rebuilt").unwrap();
        let again = source.clone().read(&path).unwrap();
        assert!(Arc::ptr_eq(&first, &again), "{:?}", again.bytes);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `VOLANT_AGENT_DIR` comes first, then the agents built into the controller, then the
    /// directory the controller sits in; an agent the controller does not embed is still found
    /// there.
    ///
    /// What would make this red: the order in `ordered` changed. Embedded before the
    /// environment, and a developer pointing `VOLANT_AGENT_DIR` at a freshly built agent silently
    /// runs the release one instead; beside the executable before embedded, and a stale agent
    /// left next to an upgraded controller wins over the one built with it.
    #[test]
    fn agents_are_looked_up_in_the_environment_then_the_controller_then_beside_it() {
        let root = embedded::tests::tempdir();
        let (env_dir, exe_dir, cache) =
            (root.0.join("env"), root.0.join("exe"), root.0.join("cache"));
        let from_env = agent_in(&env_dir, "volant-agent");
        agent_in(&exe_dir, "volant-agent");
        let riscv = agent_in(&exe_dir, "volant-agent-riscv64gc-unknown-linux-musl");
        let source = |env: Option<&PathBuf>| {
            AgentSource::ordered(
                env.cloned(),
                Some(embedded::Embedded::new(EMBEDDED, Ok(cache.clone()))),
                Some(exe_dir.clone()),
            )
        };

        assert_eq!(source(Some(&env_dir)).local().unwrap(), from_env);
        let extracted = source(None).local().unwrap();
        assert_eq!(extracted, cache.join("volant-agent"));
        assert_eq!(std::fs::read(&extracted).unwrap(), b"embedded");
        assert_eq!(
            source(None)
                .for_target("riscv64gc-unknown-linux-musl")
                .unwrap(),
            riscv
        );
    }

    /// An embedded agent that cannot be put on disk does not stop a run whose agent sits beside
    /// the controller, and when nothing has one the error says why the embedded one was not used.
    ///
    /// What would make this red: an extraction failure returned from `find` as the answer (the
    /// agent beside the controller is never reached), or dropped from the list of places looked
    /// in (the operator reads "no agent" with no hint that the cache is the problem).
    #[test]
    fn an_agent_that_cannot_be_extracted_falls_through_and_says_why() {
        let root = embedded::tests::tempdir();
        let exe_dir = root.0.join("exe");
        // A cache anyone can write, which extraction refuses.
        let cache = root.0.join("cache");
        std::fs::create_dir(&cache).unwrap();
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o777)).unwrap();
        let source = || {
            AgentSource::ordered(
                None,
                Some(embedded::Embedded::new(EMBEDDED, Ok(cache.clone()))),
                Some(exe_dir.clone()),
            )
        };

        let err = format!("{:#}", source().local().unwrap_err());
        assert!(err.contains("mode 777"), "{err}");
        let beside = agent_in(&exe_dir, "volant-agent");
        assert_eq!(source().local().unwrap(), beside);
    }

    /// A controller started through a link looks for its agents next to the file the link
    /// points at.
    ///
    /// What would make this red: the path used as `current_exe` gave it, which is the link's own
    /// directory on macOS, where a linked install would then find no agent.
    #[test]
    fn the_agents_are_looked_for_beside_the_linked_file() {
        let root = std::env::temp_dir().join(format!("volant-beside-{}", std::process::id()));
        let release = root.join("release");
        let bin = root.join("bin");
        std::fs::create_dir_all(&release).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(release.join("volant"), b"").unwrap();
        std::os::unix::fs::symlink(release.join("volant"), bin.join("volant")).unwrap();

        let found = beside_executable(bin.join("volant"));

        let expected = std::fs::canonicalize(&release).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(found, Some(expected));
    }

    /// A path that cannot be resolved still names a directory, so a missing file behaves as
    /// before rather than dropping the only place the agents could be.
    #[test]
    fn an_unresolvable_path_keeps_its_own_directory() {
        let found = beside_executable(PathBuf::from("/nonexistent/volant-dir/volant"));
        assert_eq!(found, Some(PathBuf::from("/nonexistent/volant-dir")));
    }

    /// A link over a process that reads its stdin and says nothing, which is all this needs: the
    /// memory is asserted, not the protocol.
    fn link() -> AgentLink {
        let child = tokio::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("cat is on PATH");
        AgentLink::new(child).expect("a link over it")
    }

    /// What a link was told about a payload dies with that link, so a connection lost and
    /// reopened asks again and the blob travels again.
    ///
    /// What would make this red: the memory moved anywhere that outlives one link - the map the
    /// driver keeps its links in, the run's options - which is the tempting way to avoid sending
    /// 631 KB again after a blip. The host behind the new link would then be sent a batch whose
    /// payload the agent behind it never received, and its "unknown payload" reads as the module
    /// failing.
    ///
    /// Asserted through `AgentLink` rather than through `BlobMemory::default()`: that a fresh map
    /// is empty is true of a map kept anywhere, so it says nothing about where this one lives.
    #[tokio::test]
    async fn a_new_link_carries_no_memory_of_the_link_before_it() {
        let mut first = link();
        first.blobs().remember("ab", Ok(()));
        assert!(first.blobs().holds("ab"));

        let mut second = link();
        assert!(
            second.blobs().seen("ab").is_none(),
            "a new link inherited what the link before it was told"
        );
    }
}
