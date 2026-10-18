// SPDX-License-Identifier: GPL-3.0-or-later
//! `put_file`: a file the controller staged, moved to a path of the host.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use volant_protocol::FromAgent;

/// The mode of a new file the task gave none for; an existing file keeps its own.
const DEFAULT_MODE: u32 = 0o600;

/// Answers one `put_file`. The staged file is consumed whatever the outcome, so a refused path
/// leaves no secret in the stage directory.
pub fn answer<F>(
    remote_tmp: &str,
    hash: &str,
    path: &str,
    mode: Option<u32>,
    send: &mut F,
) -> io::Result<()>
where
    F: FnMut(&FromAgent) -> io::Result<()>,
{
    let staged = crate::blobs::take(remote_tmp, hash);
    let outcome = match &staged {
        Ok(staged) => put(staged, Path::new(path), mode),
        Err(err) => Err(io::Error::new(
            err.kind(),
            format!("no staged file {hash}: {err}"),
        )),
    };
    match &staged {
        Ok(staged) => {
            let _ = fs::remove_file(staged);
        }
        // A blob that does not match its name stays where `take` left it; nothing will use it.
        Err(_) => crate::blobs::discard(remote_tmp, hash),
    }
    let (changed, error) = match outcome {
        Ok(changed) => (changed, None),
        Err(err) => (false, Some(format!("putting the file at {path}: {err}"))),
    };
    send(&FromAgent::PutFileDone {
        hash: hash.to_string(),
        changed,
        error,
    })
}

/// Whether the content or the mode at `path` changed. A directory at `path` is refused; a symlink
/// there is replaced, never followed. The new file is written beside `path` and renamed over
/// it, so a reader sees the old file or the new one, never half of it.
fn put(staged: &Path, path: &Path, mode: Option<u32>) -> io::Result<bool> {
    static TEMP: AtomicU64 = AtomicU64::new(0);

    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the path is not absolute",
        ));
    }
    let Some(dir) = path.parent().filter(|_| path.file_name().is_some()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the path names no file",
        ));
    };
    let held = fs::symlink_metadata(path).ok();
    if held.as_ref().is_some_and(fs::Metadata::is_dir) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the path is a directory",
        ));
    }
    let held_mode = held.as_ref().filter(|m| m.is_file()).and_then(mode_of);
    let mode = mode.or(held_mode).unwrap_or(DEFAULT_MODE);
    let bytes = fs::read(staged)?;
    if held_mode.is_some()
        && fs::read(path).is_ok_and(|held| blake3::hash(&held) == blake3::hash(&bytes))
    {
        if held_mode == Some(mode) {
            return Ok(false);
        }
        set_mode(path, mode)?;
        return Ok(true);
    }
    let temp = dir.join(format!(
        ".volant-put-{}-{}",
        std::process::id(),
        TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let written = create(&temp, mode).and_then(|mut file| {
        file.write_all(&bytes)?;
        file.sync_all()
    });
    // A mode the umask narrowed at creation is set outright before the file takes its name.
    let renamed = written
        .and_then(|()| set_mode(&temp, mode))
        .and_then(|()| fs::rename(&temp, path));
    if renamed.is_err() {
        let _ = fs::remove_file(&temp);
    }
    renamed.map(|()| true)
}

#[cfg(unix)]
fn create(temp: &Path, mode: u32) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(temp)
}

#[cfg(not(unix))]
fn create(temp: &Path, _mode: u32) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp)
}

/// The permission bits, setuid and the like included, of a file.
#[cfg(unix)]
fn mode_of(meta: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn mode_of(_meta: &fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests;
