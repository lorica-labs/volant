// SPDX-License-Identifier: GPL-3.0-or-later

//! This connection's staged files: their directory, taking and discarding them, and the sweep of
//! the directories dead agents left.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{cache_dir, check_private, create_private, dir, path};

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
pub(super) fn host_component(raw: &[u8]) -> String {
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
pub(super) fn stage_dir(remote_tmp: &str) -> io::Result<PathBuf> {
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
