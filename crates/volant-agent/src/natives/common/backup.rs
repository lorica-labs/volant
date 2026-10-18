// SPDX-License-Identifier: GPL-3.0-or-later
//! The `backup` argument: a timestamped copy of a file taken before the native changes it.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::natives::runtime::{Clock, Stop};

use super::check;

/// Backups this agent has named so far.
static BACKUPS: AtomicU64 = AtomicU64::new(0);

/// Why `backup_local` made no backup.
#[derive(Debug)]
pub enum BackupError {
    /// A file already has the backup's name. Nothing was written: a native hands back.
    Exists(String),
    /// The reference's exception text; the backup may be partly written.
    Failed(String),
    /// The task's `timeout` ran out, or the controller cancelled it, during the copy: the backup
    /// is left partly written, as a killed module leaves it.
    Stopped(Stop),
}

/// `backup_local`: a copy of `path` named `<path>.<number>.<%Y-%m-%d@%H:%M:%S>~`, with the
/// file's mode, times and owner (`preserved_copy`).
///
/// The reference's number is its module's pid, new for every task. A native runs inside the
/// agent, whose pid every task shares, so two backups of one file in the same second would
/// share a name. The number here is the agent's pid followed by a sequence number of this run,
/// at least four digits, never reused; and a name some file already has is never overwritten.
pub fn backup_local(path: &str, clock: Clock) -> Result<String, BackupError> {
    let seq = BACKUPS.fetch_add(1, Ordering::Relaxed) + 1;
    let name = format!("{path}.{}{seq:04}.{}", std::process::id(), local_stamp());
    backup_copy(path, &name, clock)?;
    Ok(name)
}

/// `preserved_copy(path, name)` into a file created for it, never over one that exists. The
/// deadline and the cancel are looked at before every 1 MiB copied: a backup can be of a disk
/// image.
pub fn backup_copy(path: &str, name: &str, clock: Clock) -> Result<(), BackupError> {
    let failed = |_: io::Error| {
        BackupError::Failed(format!("Could not make backup of '{path}' to '{name}'."))
    };
    let meta = fs::metadata(path).map_err(failed)?;
    let mut source = fs::File::open(path).map_err(failed)?;
    let mut copy = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(name)
    {
        Ok(copy) => copy,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            return Err(BackupError::Exists(name.to_string()));
        }
        Err(err) => return Err(failed(err)),
    };
    let mut block = vec![0; 1024 * 1024];
    loop {
        check(clock).map_err(BackupError::Stopped)?;
        match io::Read::read(&mut source, &mut block).map_err(failed)? {
            0 => break,
            n => io::Write::write_all(&mut copy, &block[..n]).map_err(failed)?,
        }
    }
    drop(copy);
    fs::set_permissions(name, fs::Permissions::from_mode(meta.mode() & 0o7777)).map_err(failed)?;
    set_times(name, &meta).map_err(failed)?;
    chown_if_permitted(name, &meta).map_err(failed)?;
    Ok(())
}

/// `time.strftime("%Y-%m-%d@%H:%M:%S~", time.localtime())`.
fn local_stamp() -> String {
    // SAFETY: `time` accepts a null pointer; `tm` is plain data that `localtime_r` fills.
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&raw const now, &raw mut tm) };
    format!(
        "{:04}-{:02}-{:02}@{:02}:{:02}:{:02}~",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// `shutil.copystat`'s times: `meta`'s access and modification times, to the nanosecond.
fn set_times(path: &str, meta: &fs::Metadata) -> io::Result<()> {
    let path = CString::new(path).map_err(io::Error::other)?;
    let times = [
        libc::timespec {
            tv_sec: meta.atime() as _,
            tv_nsec: meta.atime_nsec() as _,
        },
        libc::timespec {
            tv_sec: meta.mtime() as _,
            tv_nsec: meta.mtime_nsec() as _,
        },
    ];
    // SAFETY: `times` holds the two entries `utimensat` reads.
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// `os.chown(path, uid, gid)` of `meta`'s owner when it differs, a refusal ignored.
pub fn chown_if_permitted(path: &str, meta: &fs::Metadata) -> io::Result<()> {
    let now = fs::metadata(path)?;
    if (now.uid(), now.gid()) == (meta.uid(), meta.gid()) {
        return Ok(());
    }
    match std::os::unix::fs::chown(path, Some(meta.uid()), Some(meta.gid())) {
        Err(err) if err.raw_os_error() != Some(libc::EPERM) => Err(err),
        _ => Ok(()),
    }
}
