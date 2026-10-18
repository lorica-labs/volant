// SPDX-License-Identifier: GPL-3.0-or-later

//! What the cache asks of the filesystem: hashing a file back, free space, and the private
//! directory.

use std::fs;
use std::io;
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::Ordering;

#[cfg(test)]
use super::HASHED;

/// Whether the file at `at` reads back as the payload named `hash`. A file that is not there is
/// not a match; anything else the filesystem says is an error rather than a silent `false`.
pub(super) fn matches_hash(at: &Path, hash: &str) -> io::Result<bool> {
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
pub(super) fn check_space(
    free: impl Fn(&Path) -> io::Result<u64>,
    dir: &Path,
    need: u64,
) -> io::Result<()> {
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
pub(super) fn nearest_existing(dir: &Path) -> &Path {
    dir.ancestors()
        .find(|ancestor| ancestor.exists())
        .unwrap_or(dir)
}

/// Creates the cache directory mode 0700, and says nothing when it is already there - the
/// ownership and mode of one that already exists is [`check_private`]'s answer to give.
#[cfg(unix)]
pub(super) fn create_private(dir: &Path) -> io::Result<()> {
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
pub(super) fn create_private(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

/// Refuses a cache this agent does not own outright.
///
/// Both halves matter. A directory another user owns cannot be trusted and must not be chmod'ed
/// into shape - that would be `EPERM` forever on a shared `remote_tmp`. A directory anyone else
/// can write is worse: they seed a file under a hash they computed offline, and the agent would
/// answer for a payload it never received.
#[cfg(unix)]
pub(super) fn check_private(dir: &Path) -> io::Result<()> {
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
pub(super) fn check_private(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Makes the rename itself durable, not just the bytes it renames.
#[cfg(unix)]
pub(super) fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
pub(super) fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub(super) fn euid() -> u32 {
    // SAFETY: `geteuid` reads the calling process's own credentials and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
pub(super) fn euid() -> u32 {
    0
}

/// Bytes an unprivileged process can still write to the filesystem holding `dir`.
#[cfg(unix)]
pub(super) fn free_bytes(dir: &Path) -> io::Result<u64> {
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
pub(super) fn free_bytes(_dir: &Path) -> io::Result<u64> {
    Ok(u64::MAX)
}
