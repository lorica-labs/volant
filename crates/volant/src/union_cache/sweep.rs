// SPDX-License-Identifier: GPL-3.0-or-later
//! The files an entry is written through, the checks on the directory, and the removal of old ones.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use super::STALE;

/// `bytes` to `dir/name`, through a temporary file only this user can read.
pub(super) fn write_new(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    static UNIQUE: AtomicU64 = AtomicU64::new(0);
    let tmp = dir.join(format!(
        "{name}.tmp.{}.{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options
        .open(&tmp)
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|()| fs::rename(&tmp, dir.join(name)));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// Refuses the cache directory, or the `volant` directory holding it, when either is a symbolic
/// link or is not this user's, when the cache directory can be written by anyone else, or when
/// the one above it can be written by everyone. Both are checked where they are, without
/// following a link: a `unions` planted as a link to `~/.ssh` in a `volant` directory another
/// account created under a shared `XDG_CACHE_HOME` would otherwise pass as private.
///
/// The `volant` directory may be group-writable: it is created under the umask, and a umask of
/// 002 with a group of one's own is the default for accounts on several distributions.
pub(super) fn check_dir(dir: &Path) -> io::Result<()> {
    for (dir, forbidden) in [(Some(dir), 0o022), (dir.parent(), 0o002)] {
        let Some(dir) = dir else {
            continue;
        };
        let meta = fs::symlink_metadata(dir)?;
        if !meta.is_dir() {
            return Err(io::Error::other(format!(
                "{} is not a directory of its own",
                dir.display()
            )));
        }
        #[cfg(not(unix))]
        let _ = forbidden;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // SAFETY: `geteuid` reads the calling process's own credentials and cannot fail.
            let euid = unsafe { libc::geteuid() };
            let mode = meta.mode() & 0o7777;
            if meta.uid() != euid || mode & forbidden != 0 {
                return Err(io::Error::other(format!(
                    "{} is owned by uid {} with mode {mode:o}, and this controller runs as uid \
                     {euid}; it has to be this user's, and writable by nobody else",
                    dir.display(),
                    meta.uid()
                )));
            }
        }
    }
    Ok(())
}

/// Whether `name` is one this cache writes: `<key>.zip`, `<key>.deflate`, `<key>.json`, or a
/// temporary file of one of them. Nothing else in the directory is the sweep's to remove.
fn written_here(name: &str) -> bool {
    let (Some(key), Some(rest)) = (name.get(..64), name.get(64..)) else {
        return false;
    };
    let tmp = |rest: &str| {
        let mut parts = rest.split('.');
        parts.next() == Some("tmp")
            && parts.clone().count() == 2
            && parts.all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        && [".zip", ".deflate", ".json"].iter().any(|ext| {
            rest.strip_prefix(ext)
                .is_some_and(|after| after.is_empty() || after.strip_prefix('.').is_some_and(tmp))
        })
}

/// Removes every file of this cache in `dir` nothing has written for [`STALE`]: entries of module
/// sets no run asks for any more, and temporary files a run that died left behind. An entry
/// still in use but older than that is rebuilt once. A link is never followed, and a file this
/// cache did not name is never touched.
pub(super) fn sweep(dir: &Path, now: SystemTime) {
    sweep_within(dir, now, MAX_BYTES);
}

/// How much the cache keeps before it removes its oldest entries, whatever their age. Measured on
/// a development machine whose test runs each built a union of their own: 59 105 files, 15 GB,
/// that only the month-old rule would ever have removed. An entry is a few megabytes.
const MAX_BYTES: u64 = 1 << 30;

/// [`sweep`], then, while the entries left hold more than `max_bytes`, the oldest entry whole:
/// its manifest, zip and deflated form go together, so no half of an entry outlives the rest.
/// An entry is as old as the newest of its files.
pub(super) fn sweep_within(dir: &Path, now: SystemTime, max_bytes: u64) {
    let Some(limit) = now.checked_sub(STALE) else {
        return;
    };
    let Ok(files) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: std::collections::BTreeMap<
        String,
        (SystemTime, u64, Vec<std::path::PathBuf>),
    > = std::collections::BTreeMap::new();
    for file in files.flatten() {
        let Some(name) = file
            .file_name()
            .to_str()
            .filter(|n| written_here(n))
            .map(str::to_string)
        else {
            continue;
        };
        // `DirEntry::metadata` does not follow a link, and a link is not a file.
        let Some(meta) = file.metadata().ok().filter(fs::Metadata::is_file) else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if modified < limit {
            let _ = fs::remove_file(file.path());
            continue;
        }
        let entry = entries.entry(name[..64].to_string()).or_insert((
            SystemTime::UNIX_EPOCH,
            0,
            Vec::new(),
        ));
        entry.0 = entry.0.max(modified);
        entry.1 += meta.len();
        entry.2.push(file.path());
    }
    let mut total: u64 = entries.values().map(|e| e.1).sum();
    let mut oldest: Vec<_> = entries.into_values().collect();
    oldest.sort_by_key(|e| e.0);
    for (_, size, paths) in oldest {
        if total <= max_bytes {
            break;
        }
        for path in paths {
            let _ = fs::remove_file(path);
        }
        total -= size;
    }
}
