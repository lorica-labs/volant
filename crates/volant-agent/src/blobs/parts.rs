// SPDX-License-Identifier: GPL-3.0-or-later

//! A staged file longer than one frame: written piece by piece as its `put_blob_part`s arrive,
//! never held whole, and checked against its hash at the `end_blob` before it takes its name.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::fs_guard::{check_space, free_bytes, matches_hash, nearest_existing, sync_dir};
use super::path;
use super::stage::stage_dir;

/// Where the pieces of `hash` are put together: in this connection's directory, beside the
/// staged file they become, so the end is a rename. The connection's end removes it with the
/// rest of the directory if the `end_blob` never comes.
fn assembling(remote_tmp: &str, hash: &str) -> io::Result<PathBuf> {
    path(remote_tmp, hash)?;
    Ok(stage_dir(remote_tmp)?.join(format!("{hash}.part")))
}

fn invalid(why: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why)
}

/// Writes the piece at `offset` of the `total` bytes of `hash`. The first piece checks there is
/// room for all of them and starts the file over; every other one must land where the one
/// before it ended. A piece that fails takes the file with it, so the `end_blob` finds nothing.
pub(super) fn write(
    remote_tmp: &str,
    hash: &str,
    offset: u64,
    total: u64,
    bytes: &[u8],
) -> io::Result<()> {
    let at = assembling(remote_tmp, hash)?;
    let written = write_at(remote_tmp, &at, offset, total, bytes);
    if written.is_err() {
        let _ = fs::remove_file(&at);
    }
    written
}

fn write_at(remote_tmp: &str, at: &Path, offset: u64, total: u64, bytes: &[u8]) -> io::Result<()> {
    let end = offset.saturating_add(bytes.len() as u64);
    if end > total {
        return Err(invalid(format!(
            "a piece ending at {end} bytes is past the {total} bytes announced"
        )));
    }
    let mut file = if offset == 0 {
        check_space(free_bytes, nearest_existing(Path::new(remote_tmp)), total)?;
        fs::File::create(at)?
    } else {
        let file = OpenOptions::new().append(true).open(at)?;
        let held = file.metadata()?.len();
        if held != offset {
            return Err(invalid(format!(
                "a piece for offset {offset} arrived after {held} bytes"
            )));
        }
        file
    };
    file.write_all(bytes)
}

/// The pieces of `hash` as its staged file: synced, checked against the hash, and renamed to the
/// name a `put_blob` with `staged` gives it. Pieces that do not hash to `hash` are removed.
pub(super) fn end(remote_tmp: &str, hash: &str) -> io::Result<PathBuf> {
    let at = assembling(remote_tmp, hash)?;
    fs::File::open(&at)?.sync_all()?;
    if !matches_hash(&at, hash)? {
        let _ = fs::remove_file(&at);
        return Err(invalid(format!(
            "the pieces of '{hash}' hash to something else"
        )));
    }
    let dir = stage_dir(remote_tmp)?;
    let staged = dir.join(hash);
    fs::rename(&at, &staged)?;
    sync_dir(&dir)?;
    Ok(staged)
}
