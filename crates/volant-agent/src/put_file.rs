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
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    /// Written once with the default mode, unchanged the second time, replaced when it differs.
    #[test]
    fn a_file_is_replaced_by_checksum_and_gets_its_mode() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged");
        let at = dir.path().join("out");
        fs::write(&staged, b"one").unwrap();
        assert!(put(&staged, &at, None).unwrap());
        assert_eq!(fs::read(&at).unwrap(), b"one");
        assert_eq!(mode(&at), DEFAULT_MODE);
        assert!(!put(&staged, &at, Some(0o600)).unwrap());
        fs::write(&staged, b"two").unwrap();
        assert!(put(&staged, &at, Some(0o640)).unwrap());
        assert_eq!(fs::read(&at).unwrap(), b"two");
        assert_eq!(mode(&at), 0o640);
        let left: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(left.len(), 2, "no temporary file is left behind");
    }

    /// A mode alone is a change; no mode keeps the one the file has, in both branches.
    #[test]
    fn a_mode_change_counts_and_no_mode_keeps_the_existing_one() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged");
        let at = dir.path().join("out");
        fs::write(&staged, b"same").unwrap();
        put(&staged, &at, Some(0o644)).unwrap();
        assert!(put(&staged, &at, Some(0o4755)).unwrap(), "mode alone");
        assert_eq!(mode(&at), 0o4755);
        assert!(!put(&staged, &at, Some(0o4755)).unwrap());
        assert!(!put(&staged, &at, None).unwrap());
        assert_eq!(mode(&at), 0o4755);
        fs::write(&staged, b"new").unwrap();
        assert!(put(&staged, &at, None).unwrap());
        assert_eq!(mode(&at), 0o4755, "a new content keeps the mode too");
    }

    #[test]
    fn a_directory_is_refused_and_a_symlink_is_replaced_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged");
        fs::write(&staged, b"data").unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let err = put(&staged, &sub, None).unwrap_err();
        assert_eq!(err.to_string(), "the path is a directory");
        assert!(sub.is_dir());

        let target = dir.path().join("target");
        fs::write(&target, b"data").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(put(&staged, &link, Some(0o600)).unwrap());
        assert!(!fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(fs::read(&link).unwrap(), b"data");
        assert_eq!(mode(&link), 0o600);
        assert_eq!(mode(&target), 0o644, "the target is untouched");
    }

    #[test]
    fn a_relative_path_and_a_missing_directory_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged");
        fs::write(&staged, b"x").unwrap();
        let err = put(&staged, Path::new("out"), None).unwrap_err();
        assert_eq!(err.to_string(), "the path is not absolute");
        assert!(put(&staged, &dir.path().join("no/such/out"), None).is_err());
    }
}
