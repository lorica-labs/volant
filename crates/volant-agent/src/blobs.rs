// SPDX-License-Identifier: GPL-3.0-or-later
//! The agent's cache of module payloads and of the files a task stages, addressed by the blake3
//! hash of the bytes they hold.
//!
//! One rule holds the whole file together: a payload is the right payload because its bytes hash
//! to its name, never because a file of that name exists. `remote_tmp` is `/var/tmp` on many
//! hosts, the name of a payload is public - it is the hash of a zip built from a released
//! ansible-core - and what the agent answers `present: true` for is what it later runs, often
//! under `become`. So the bytes are read back and hashed on every path that can answer yes, the
//! cache directory is refused unless this agent owns it and nobody else can write it, and a name
//! that is not 64 hex characters never reaches the filesystem at all.

use std::borrow::Cow;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use volant_protocol::encoding::{BLOB_LIMIT, inflate};
use volant_protocol::frame::payload_len;
use volant_protocol::{BlobEncoding, FromAgent, LogLevel, ToAgent};

/// A message from the controller, with the frame that follows it when it is a `put_blob`: the
/// frame's bytes, or why they were skipped. Empty for every other message.
pub type Incoming = (ToAgent, Result<Vec<u8>, String>);

/// Reads one message and, after a `put_blob`, the blob frame that always follows it, so the
/// stream never falls out of step: a frame longer than the announced `len` is read through and
/// dropped without being held, and answered as a refusal. `Ok(None)` at a clean end of stream.
///
/// A message that does not read is `Some(Err(why))`: the caller logs and drops it, as it always
/// has. A `put_blob` that does not read (an encoding this agent does not know) still has its
/// blob frame after it, which is read through without being held, never taken for the next
/// message; when it names its hash it is answered as a refused blob.
pub fn read_incoming<R: Read>(mut input: R) -> io::Result<Option<Result<Incoming, String>>> {
    let Some(bytes) = volant_protocol::frame::read_frame(&mut input)? else {
        return Ok(None);
    };
    let msg = match serde_json::from_slice::<ToAgent>(&bytes) {
        Ok(msg) => msg,
        Err(err) => return unreadable(&mut input, &bytes, &err.to_string()),
    };
    let blob = match &msg {
        ToAgent::PutBlob { len, .. } => read_blob_frame(&mut input, *len)?,
        _ => Ok(Vec::new()),
    };
    Ok(Some(Ok((msg, blob))))
}

/// A message that did not read, after its frame: the blob frame behind a `put_blob` is read
/// through, and a `put_blob` naming its hash comes back as one whose blob is refused, so the
/// controller waiting for its `BlobState` gets one.
fn unreadable<R: Read>(
    mut input: R,
    bytes: &[u8],
    why: &str,
) -> io::Result<Option<Result<Incoming, String>>> {
    let value: serde_json::Value = serde_json::from_slice(bytes).unwrap_or_default();
    if value.get("type").and_then(|t| t.as_str()) != Some("put_blob") {
        return Ok(Some(Err(why.to_string())));
    }
    // Announced as 0 bytes, any frame is longer and read through without being held.
    let _skipped = read_blob_frame(&mut input, 0)?;
    let Some(hash) = value.get("hash").and_then(|h| h.as_str()) else {
        return Ok(Some(Err(why.to_string())));
    };
    let refused = ToAgent::PutBlob {
        hash: hash.to_string(),
        len: 0,
        encoding: BlobEncoding::Raw,
        staged: value.get("staged").and_then(serde_json::Value::as_bool) == Some(true),
    };
    Ok(Some(Ok((
        refused,
        Err(format!("the put_blob does not read: {why}")),
    ))))
}

/// The frame after a `put_blob` announcing `len` decoded bytes. Neither encoding makes a blob
/// longer than `len` on the wire (the controller deflates only when that is shorter), so a
/// longer frame, or any frame after a `len` past [`BLOB_LIMIT`], is read through without being
/// held and refused. The end of the stream in its place is an error: the controller went away
/// mid-blob.
fn read_blob_frame<R: Read>(mut input: R, len: u64) -> io::Result<Result<Vec<u8>, String>> {
    let mut header = [0u8; 4];
    input.read_exact(&mut header)?;
    let size = payload_len(header)?;
    if size as u64 > len || len > BLOB_LIMIT as u64 {
        io::copy(&mut input.take(size as u64), &mut io::sink())?;
        return Ok(Err(if len > BLOB_LIMIT as u64 {
            format!("{len} bytes is past the {BLOB_LIMIT} byte limit")
        } else {
            format!("the blob's frame is {size} bytes, longer than the {len} bytes it decodes to")
        }));
    }
    let mut bytes = vec![0u8; size];
    input.read_exact(&mut bytes)?;
    Ok(Ok(bytes))
}

/// The blob a `put_blob`'s frame carries, decoded: refused when `len` is past [`BLOB_LIMIT`],
/// when a raw frame is not `len` bytes, when a deflated one does not inflate to exactly `len`
/// bytes (truncated, damaged, or a bomb stopped at `len`).
fn decoded(len: u64, encoding: BlobEncoding, frame: &[u8]) -> io::Result<Cow<'_, [u8]>> {
    let invalid = |why: String| io::Error::new(io::ErrorKind::InvalidData, why);
    let len = usize::try_from(len)
        .ok()
        .filter(|len| *len <= BLOB_LIMIT)
        .ok_or_else(|| invalid(format!("{len} bytes is past the {BLOB_LIMIT} byte limit")))?;
    let bytes = match encoding {
        BlobEncoding::Raw => Cow::Borrowed(frame),
        BlobEncoding::Deflate => Cow::Owned(inflate(frame, len).map_err(invalid)?),
    };
    if bytes.len() != len {
        return Err(invalid(format!(
            "the blob decoded to {} bytes where {len} were announced",
            bytes.len()
        )));
    }
    Ok(bytes)
}

/// Where the agent keeps payloads, and where it reads `remote_tmp` from.
///
/// `VOLANT_REMOTE_TMP` when the controller sets it; otherwise the agent's own path, which names
/// `remote_tmp` on the ssh transport. `std::env::temp_dir()` when neither answers - a directory
/// the user running the agent can always write.
///
/// **For task 3:** have the controller set `VOLANT_REMOTE_TMP`. It holds the configured
/// `remote_tmp` on both transports and the derivation below only covers one of them: the local
/// transport starts the agent from wherever it was installed, so the operator's configured value
/// is not consulted at all and the cache falls back to the temporary directory.
pub fn remote_tmp() -> String {
    if let Ok(dir) = std::env::var("VOLANT_REMOTE_TMP") {
        return dir;
    }
    std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(tmp_from_exe)
        .and_then(|dir| dir.to_str())
        .map_or_else(
            || std::env::temp_dir().to_string_lossy().into_owned(),
            str::to_string,
        )
}

/// `remote_tmp` read back from the agent's own path, and only where the ssh transport puts it:
/// `<remote_tmp>/volant-agent-<version>/volant-agent`.
///
/// Anywhere else, the grandparent is somebody else's directory rather than `remote_tmp` - a
/// system-wide install over the local transport would make it `/usr/local` - so the shape is
/// checked instead of assumed.
fn tmp_from_exe(exe: &Path) -> Option<&Path> {
    let cache = exe.parent()?;
    if !cache.file_name()?.to_str()?.starts_with("volant-agent-") {
        return None;
    }
    cache.parent()
}

/// The cache directory's name, which carries the effective uid.
///
/// Per user, because `remote_tmp` is shared: two operators on one host, or the same operator with
/// and without `become`, would otherwise meet in one directory that only the first of them can
/// write - and every run of the second would fail identically until somebody found and removed a
/// directory in `/var/tmp` they had no reason to look at.
fn dir(remote_tmp: &str) -> PathBuf {
    Path::new(remote_tmp).join(format!(
        "volant-blobs-{}-{}",
        env!("CARGO_PKG_VERSION"),
        euid()
    ))
}

/// The cache directory, created mode 0700 and refused unless it is this agent's alone.
///
/// Created with the mode rather than created and then chmod'ed: the second form leaves a window
/// where another user can drop a file in, and it also `EPERM`s against a directory somebody else
/// owns instead of saying so.
fn cache_dir(remote_tmp: &str) -> io::Result<PathBuf> {
    let dir = dir(remote_tmp);
    create_private(&dir)?;
    check_private(&dir)?;
    Ok(dir)
}

/// Where a payload of this hash sits, once the name is known to be one.
///
/// The name comes off the wire and is joined onto a path, so it is checked here rather than in
/// each of the three places that will open the result: `../../home/mallory/evil` is a blob name
/// no agent should be able to build a path from.
pub fn path(remote_tmp: &str, hash: &str) -> io::Result<PathBuf> {
    if hash.len() != 64 || !hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("'{hash}' is not a payload name: 64 lowercase hex characters"),
        ));
    }
    Ok(dir(remote_tmp).join(hash))
}

/// The name of this connection's own directory: `stage-<host>-<pid>`.
///
/// A pid means something on one host only, and the cache can be shared by several: a home
/// directory mounted over NFS on every host puts `~/.ansible/tmp` in one place for all of them.
/// Without the host, two hosts with one pid would share one directory, and one host's sweep would
/// read another host's live agent as dead.
fn stage_name() -> String {
    format!("stage-{}-{}", host(), std::process::id())
}

/// This host's name as [`host_component`] writes it, read once, so a host renamed mid-run still
/// removes and sweeps under the name it started with.
fn host() -> &'static str {
    static HOST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HOST.get_or_init(host_name)
}

/// This host's name as one path component the sweep can split on: `[A-Za-z0-9._]` kept, anything
/// else - `-` included, so the last `-` of a directory name is always the one before the pid -
/// replaced by `_`, and at most 64 bytes.
fn host_component(raw: &[u8]) -> String {
    let name: String = raw
        .iter()
        .take(64)
        .map(|&b| {
            if b.is_ascii_alphanumeric() || b == b'.' || b == b'_' {
                char::from(b)
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() {
        "unnamed".to_string()
    } else {
        name
    }
}

#[cfg(unix)]
fn host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is writable for its whole length, which is the length passed.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    host_component(if ok { &buf[..len] } else { b"" })
}

#[cfg(not(unix))]
fn host_name() -> String {
    host_component(b"")
}

/// The directory of this connection's own staged files, `<cache>/stage-<host>-<pid>`, created
/// with the same privacy as the cache.
///
/// One per agent process rather than the shared cache: two links to one host account (two
/// inventory names for one machine, two hosts delegating to one) run two agents on one cache,
/// and a file one of them consumed was one the other had been told was there.
fn stage_dir(remote_tmp: &str) -> io::Result<PathBuf> {
    let dir = cache_dir(remote_tmp)?.join(stage_name());
    create_private(&dir)?;
    check_private(&dir)?;
    Ok(dir)
}

/// Takes a file this connection put for one task, and hands back where it now is.
///
/// Looked for in this connection's directory only, never in the shared cache. Renamed rather
/// than copied, to a name `store` never writes, so a second task staging the same file finds it
/// consumed, and a later `put_blob` of the same bytes cannot land on the file a module is using.
/// The bytes are hashed first, so a blob that does not match its name is never staged; it stays
/// where it is for the next `put_blob` to replace.
pub fn take(remote_tmp: &str, hash: &str) -> io::Result<PathBuf> {
    static STAGED: AtomicU64 = AtomicU64::new(0);

    path(remote_tmp, hash)?;
    let dir = stage_dir(remote_tmp)?;
    let at = dir.join(hash);
    let actual = blake3::hash(&fs::read(&at)?).to_hex().to_string();
    if actual != hash {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("blob '{hash}' holds bytes that hash to '{actual}'"),
        ));
    }
    let staged = dir.join(format!("{hash}-{}", STAGED.fetch_add(1, Ordering::Relaxed)));
    fs::rename(&at, &staged)?;
    Ok(staged)
}

/// Removes the file this connection staged under `hash`, if any: what a caller does when the
/// file cannot be used, so a refused secret does not wait for the connection to end.
pub fn discard(remote_tmp: &str, hash: &str) {
    if path(remote_tmp, hash).is_ok()
        && let Ok(dir) = stage_dir(remote_tmp)
    {
        let _ = fs::remove_file(dir.join(hash));
    }
}

/// Removes this connection's staged files, taken or not, once the controller has gone.
///
/// A file put and never taken - a batch cancelled first, a link dropped between the put and the
/// batch - is often a rendered secret, and nothing else would ever remove it. An agent that dies
/// before it gets here leaves the directory to [`sweep`] on the next start.
pub fn end_connection(remote_tmp: &str) {
    let dir = dir(remote_tmp).join(stage_name());
    match fs::remove_dir_all(&dir) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => {
            eprintln!(
                "volant-agent: removing staged files {}: {err}",
                dir.display()
            );
        }
        _ => {}
    }
}

/// Removes the `stage-<host>-<pid>` directories dead agents of this user left on this host.
///
/// Only this host's entries are looked at: another host sharing the cache has its own pids, and
/// `ESRCH` here says nothing about them. Entries without a host - the `stage-<hash>-<pid>-<n>`
/// files of the layout before this one, which no release wrote - cannot be placed on a host, so
/// they are left too. Only a pid that no longer exists is removed (`kill(pid, 0)` says `ESRCH`),
/// because two agents of one user run at once whenever two runs reach one host. A pid reused by
/// another live process leaves the entry where it is; that is the safe way round, and the entry
/// goes once that process has. The one exception is this agent's own pid, which no live agent
/// can hold. A cache this agent does not own is not touched at all.
pub fn sweep(remote_tmp: &str) {
    let cache = dir(remote_tmp);
    if check_private(&cache).is_err() {
        return;
    }
    let Ok(list) = fs::read_dir(&cache) else {
        return;
    };
    for entry in list.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix("stage-")) else {
            continue;
        };
        let pid = rest
            .rsplit_once('-')
            .filter(|(name, _)| *name == host())
            .and_then(|(_, pid)| pid.parse::<u32>().ok());
        if !pid.is_some_and(|pid| pid == std::process::id() || !alive(pid)) {
            continue;
        }
        let at = entry.path();
        let removed = if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            fs::remove_dir_all(&at)
        } else {
            fs::remove_file(&at)
        };
        if let Err(err) = removed {
            eprintln!(
                "volant-agent: removing staged files {}: {err}",
                at.display()
            );
        }
    }
}

/// Whether `pid` names a process that exists, for any user. Anything but `ESRCH` counts as
/// alive, which is the answer that keeps a file rather than removing one in use.
#[cfg(unix)]
fn alive(pid: u32) -> bool {
    // A pid of 0 or past `i32::MAX` is no process: `kill` would read them as "my group" or "every
    // process", which exist.
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 sends nothing; it only asks whether the pid exists.
    let found = unsafe { libc::kill(pid, 0) } == 0;
    found || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn alive(_pid: u32) -> bool {
    true
}

/// Whether the cache holds this payload **and** its bytes still hash to its name.
///
/// The name alone is never enough, for the reason at the top of this file. Reading and hashing
/// the 631 KB union payload costs a fraction of a millisecond, once per run for each link, which
/// buys the only thing that makes `present: true` mean what the controller reads it to mean.
/// Bytes that do not match answer `false`, so the controller re-sends and [`store`] replaces
/// them: a cache poisoned by a power loss mid-rename heals on the next run instead of being
/// trusted forever.
pub fn holds(remote_tmp: &str, hash: &str) -> io::Result<bool> {
    let at = path(remote_tmp, hash)?;
    cache_dir(remote_tmp)?;
    matches_hash(&at, hash)
}

/// Answers a blob message, wherever in the conversation it arrives.
///
/// `serve` takes these between batches and `runner::is_cancelled` takes them during one, and
/// both have to answer: a controller that sent `put_blob` waits for a `BlobState`, so one
/// logged and dropped mid-batch left it waiting for a state that never came. `Ok(false)` says
/// the message was not a blob message and is still the caller's to handle.
///
/// `frame` is what [`read_incoming`] read after a `put_blob`.
pub fn answer<F>(
    remote_tmp: &str,
    msg: &ToAgent,
    frame: &Result<Vec<u8>, String>,
    send: &mut F,
) -> io::Result<bool>
where
    F: FnMut(&FromAgent) -> io::Result<()>,
{
    match msg {
        ToAgent::HasBlob { hash } => {
            let present = match holds(remote_tmp, hash) {
                Ok(present) => present,
                Err(err) => {
                    send(&FromAgent::Log {
                        level: LogLevel::Error,
                        message: format!("looking for payload {hash}: {err}"),
                    })?;
                    false
                }
            };
            send(&FromAgent::BlobState {
                hash: hash.clone(),
                present,
            })?;
        }
        // A refused payload is answered, never left silent: the controller waits for this state
        // before it sends the batch that needs the payload, and the log is the only place the
        // reason survives.
        ToAgent::PutBlob {
            hash,
            len,
            encoding,
            staged,
        } => match frame
            .as_ref()
            .map_err(|why| io::Error::new(io::ErrorKind::InvalidData, why.clone()))
            .and_then(|frame| decoded(*len, *encoding, frame))
            .and_then(|bytes| store(remote_tmp, hash, &bytes, *staged))
        {
            Ok(_) => send(&FromAgent::BlobState {
                hash: hash.clone(),
                present: true,
            })?,
            Err(err) => {
                send(&FromAgent::Log {
                    level: LogLevel::Error,
                    message: format!(
                        "storing {} {hash}: {err}",
                        if *staged { "file" } else { "payload" }
                    ),
                })?;
                send(&FromAgent::BlobState {
                    hash: hash.clone(),
                    present: false,
                })?;
            }
        },
        _ => return Ok(false),
    }
    Ok(true)
}

/// Hashes the decoded bytes, refuses a mismatch, then writes atomically under the hash.
///
/// The hash is checked **before** anything is written, so a mismatch never
/// leaves a file behind under any name, nor even the cache directory. The temporary name carries
/// the pid so two agents racing on one host cannot interleave into a file of exactly the right
/// length.
///
/// A `staged` blob lands in this connection's own directory rather than the shared cache: see
/// [`stage_dir`].
pub fn store(remote_tmp: &str, hash: &str, zip: &[u8], staged: bool) -> io::Result<PathBuf> {
    store_with(free_bytes, remote_tmp, hash, zip, staged)
}

/// [`store`] with the free-space reader handed in, so a test can drive the guard's call site
/// rather than only the guard.
fn store_with(
    free: impl Fn(&Path) -> io::Result<u64>,
    remote_tmp: &str,
    hash: &str,
    zip: &[u8],
    staged: bool,
) -> io::Result<PathBuf> {
    let actual = blake3::hash(zip).to_hex().to_string();
    if actual != hash {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("payload arrived as '{hash}' but its bytes hash to '{actual}'"),
        ));
    }
    path(remote_tmp, hash)?;
    // Before the directory exists: `create_dir_all` is itself a write, and on a filesystem that
    // is genuinely full it would fail `ENOSPC` first and hide the two figures below.
    check_space(
        &free,
        nearest_existing(Path::new(remote_tmp)),
        zip.len() as u64,
    )?;
    let dir = if staged {
        stage_dir(remote_tmp)?
    } else {
        cache_dir(remote_tmp)?
    };
    let final_path = dir.join(hash);
    if matches_hash(&final_path, hash)? {
        return Ok(final_path);
    }
    let tmp = dir.join(format!("{hash}.tmp.{}", std::process::id()));
    let mut file = fs::File::create(&tmp)?;
    if let Err(err) = file.write_all(zip).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    drop(file);
    if let Err(err) = fs::rename(&tmp, &final_path) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    // The file's own `sync_all` above only promises its contents. Without this, a power loss
    // just after the rename can leave the directory entry pointing at a short or zero-filled
    // file, and `holds` is the only thing that would ever notice - one run too late, since the
    // controller has already been told the payload is there.
    sync_dir(&dir)?;
    Ok(final_path)
}

/// How many times a file was read and hashed, so a test can tell a verification that ran from
/// one that was skipped.
#[cfg(test)]
pub(crate) static HASHED: AtomicU64 = AtomicU64::new(0);

/// Whether the file at `at` reads back as the payload named `hash`. A file that is not there is
/// not a match; anything else the filesystem says is an error rather than a silent `false`.
fn matches_hash(at: &Path, hash: &str) -> io::Result<bool> {
    #[cfg(test)]
    HASHED.fetch_add(1, Ordering::Relaxed);
    match fs::read(at) {
        Ok(zip) => Ok(blake3::hash(&zip).to_hex().to_string() == hash),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

/// Refuses a payload the filesystem cannot hold, before a byte of it is written.
///
/// A short write noticed afterwards would already have spent the space, and the message an
/// operator gets would be `ENOSPC` on a temporary name rather than the two figures that say how
/// far short the host is.
fn check_space(free: impl Fn(&Path) -> io::Result<u64>, dir: &Path, need: u64) -> io::Result<()> {
    let free = free(dir)?;
    if free < need {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            format!(
                "payload needs {need} bytes, the filesystem holding {} has {free}",
                dir.display()
            ),
        ));
    }
    Ok(())
}

/// The nearest ancestor of `dir` that exists, for a question only an existing directory can be
/// asked. `remote_tmp` is `~/.ansible/tmp` by default and may not have been created yet.
fn nearest_existing(dir: &Path) -> &Path {
    dir.ancestors()
        .find(|ancestor| ancestor.exists())
        .unwrap_or(dir)
}

/// Creates the cache directory mode 0700, and says nothing when it is already there - the
/// ownership and mode of one that already exists is [`check_private`]'s answer to give.
#[cfg(unix)]
fn create_private(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    let make = || fs::DirBuilder::new().mode(0o700).create(dir);
    match make() {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = dir.parent() {
                fs::create_dir_all(parent)?;
            }
            make()
        }
        other => other,
    }
    .or_else(|err| match err.kind() {
        io::ErrorKind::AlreadyExists => Ok(()),
        _ => Err(err),
    })
}

/// The agent runs on a managed unix host and nowhere else; the arms off unix only keep the crate
/// compiling on a developer's machine.
#[cfg(not(unix))]
fn create_private(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

/// Refuses a cache this agent does not own outright.
///
/// Both halves matter. A directory another user owns cannot be trusted and must not be chmod'ed
/// into shape - that would be `EPERM` forever on a shared `remote_tmp`. A directory anyone else
/// can write is worse: they seed a file under a hash they computed offline, and the agent would
/// answer for a payload it never received.
#[cfg(unix)]
fn check_private(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let meta = fs::metadata(dir)?;
    let mode = meta.mode() & 0o7777;
    if meta.uid() != euid() || mode & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is owned by uid {} with mode {mode:o}; this agent runs as uid {} and will not \
                 trust a payload cache another user can write",
                dir.display(),
                meta.uid(),
                euid()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Makes the rename itself durable, not just the bytes it renames.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn euid() -> u32 {
    // SAFETY: `geteuid` reads the calling process's own credentials and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn euid() -> u32 {
    0
}

/// Bytes an unprivileged process can still write to the filesystem holding `dir`.
#[cfg(unix)]
fn free_bytes(dir: &Path) -> io::Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(dir.as_os_str().as_bytes())
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a NUL-terminated C string that outlives the call, and `stat` is a
    // writable, correctly aligned `statvfs` the call fills before it returns 0.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call above returned 0, so it initialised the structure.
    let stat = unsafe { stat.assume_init() };
    // Through `u128` because the two fields are as wide as a `u64` on the targets the agent is
    // built for, and their product is not.
    let free = u128::from(stat.f_bavail) * u128::from(stat.f_frsize);
    Ok(u64::try_from(free).unwrap_or(u64::MAX))
}

/// Nothing asks the filesystem off unix, where the agent never runs: the guard passes and the
/// write itself is left to report a full filesystem.
#[cfg(not(unix))]
fn free_bytes(_dir: &Path) -> io::Result<u64> {
    Ok(u64::MAX)
}

#[cfg(test)]
mod tests;
