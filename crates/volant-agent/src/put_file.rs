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
        Err(err) => (false, Some(refusal(path, &err))),
    };
    send(&FromAgent::PutFileDone {
        hash: hash.to_string(),
        changed,
        error,
    })
}

/// Why a put failed. An error of the system is said in the reference's words, `put_file`'s
/// `AnsibleError` over Python's `OSError`, measured on ansible-core 2.19.12 with a file the
/// connection user may not write: `Failed to transfer file to '<path>': [Errno 13] Permission
/// denied: b'<path>'`.
fn refusal(path: &str, err: &io::Error) -> String {
    match err.raw_os_error() {
        Some(code) => {
            let text = err.to_string();
            let text = text
                .strip_suffix(&format!(" (os error {code})"))
                .unwrap_or(&text);
            format!("Failed to transfer file to '{path}': [Errno {code}] {text}: b'{path}'")
        }
        None => format!("putting the file at {path}: {err}"),
    }
}

/// Whether the content or the mode at `path` changed. A regular file already there is written
/// in place, as the reference's `put_file` copies onto it: it keeps its owner, group and mode, and
/// one the agent may not write is refused, the agent running as the connection user. A directory
/// at `path` is refused; a symlink there is replaced, never followed. A new file is written
/// beside `path` and renamed into place, so a reader never sees half of it.
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
    let bytes = fs::read(staged)?;
    if let Some(held_mode) = held.as_ref().filter(|m| m.is_file()).and_then(mode_of) {
        let same = fs::read(path).is_ok_and(|held| blake3::hash(&held) == blake3::hash(&bytes));
        if !same {
            let mut file = open_in_place(path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        let mode = mode.unwrap_or(held_mode);
        // Read again: a write takes setuid and setgid off.
        if fs::metadata(path).ok().as_ref().and_then(mode_of) != Some(mode) {
            set_mode(path, mode)?;
        }
        return Ok(!same || mode != held_mode);
    }
    let mode = mode.unwrap_or(DEFAULT_MODE);
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

/// The regular file at `path` opened to be written over, never through a symlink put there since
/// it was looked at. `O_CREAT` as `shutil.copyfile` opens it, which `fs.protected_regular`
/// refuses for another user's file in a sticky directory such as `/tmp`, as it refuses the
/// reference.
#[cfg(unix)]
fn open_in_place(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_in_place(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
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
