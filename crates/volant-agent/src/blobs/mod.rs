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

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicU64;

#[cfg(test)]
use volant_protocol::BlobEncoding;
#[cfg(test)]
use volant_protocol::encoding::BLOB_LIMIT;
use volant_protocol::{FromAgent, LogLevel, ToAgent};

mod fs_guard;
mod incoming;
mod stage;

use fs_guard::{
    check_private, check_space, create_private, euid, free_bytes, matches_hash, nearest_existing,
    sync_dir,
};
use incoming::decoded;
pub use incoming::{Incoming, read_incoming};
#[cfg(test)]
use stage::host_component;
use stage::stage_dir;
pub use stage::{discard, end_connection, sweep, take};

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

#[cfg(test)]
mod tests;
