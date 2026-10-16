// SPDX-License-Identifier: GPL-3.0-or-later
//! `put_file`: a file the controller staged, moved to a path of the host.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use volant_protocol::FromAgent;

/// The mode of a file the task gave none for.
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
        Ok(staged) => put(staged, Path::new(path), mode.unwrap_or(DEFAULT_MODE)),
        Err(err) => Err(io::Error::new(
            err.kind(),
            format!("no staged file {hash}: {err}"),
        )),
    };
    if let Ok(staged) = &staged {
        let _ = fs::remove_file(staged);
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

/// Whether the content at `path` changed. The new file is written beside `path` and renamed over
/// it, so a reader sees the old file or the new one, never half of it.
fn put(staged: &Path, path: &Path, mode: u32) -> io::Result<bool> {
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
    let bytes = fs::read(staged)?;
    let same = fs::metadata(path).is_ok_and(|m| m.is_file())
        && fs::read(path).is_ok_and(|held| blake3::hash(&held) == blake3::hash(&bytes));
    if same {
        set_mode(path, mode)?;
        return Ok(false);
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

    /// Written once with the default mode, unchanged the second time, replaced when it differs.
    #[test]
    fn a_file_is_replaced_by_checksum_and_gets_its_mode() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged");
        let at = dir.path().join("out");
        fs::write(&staged, b"one").unwrap();
        assert!(put(&staged, &at, DEFAULT_MODE).unwrap());
        assert_eq!(fs::read(&at).unwrap(), b"one");
        assert_eq!(
            fs::metadata(&at).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!put(&staged, &at, 0o640).unwrap());
        assert_eq!(
            fs::metadata(&at).unwrap().permissions().mode() & 0o777,
            0o640
        );
        fs::write(&staged, b"two").unwrap();
        assert!(put(&staged, &at, 0o640).unwrap());
        assert_eq!(fs::read(&at).unwrap(), b"two");
        let left: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(left.len(), 2, "no temporary file is left behind");
    }

    #[test]
    fn a_relative_path_and_a_missing_directory_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged");
        fs::write(&staged, b"x").unwrap();
        let err = put(&staged, Path::new("out"), 0o600).unwrap_err();
        assert_eq!(err.to_string(), "the path is not absolute");
        assert!(put(&staged, &dir.path().join("no/such/out"), 0o600).is_err());
    }
}
