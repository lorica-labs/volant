// SPDX-License-Identifier: GPL-3.0-or-later
//! Paths and what sits on them: owner, group and mode changes, extended attributes and inode
//! flags, and the file information a result carries.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use serde_json::{Map, Value};

use crate::natives::runtime::{Clock, Stop};

use super::{Account, ModeError, bytes_repr, lookup_group, lookup_user, parse_mode, strerror};

/// Why `set_fs_attributes` stopped.
#[derive(Debug)]
pub enum FsError {
    /// The reference's `fail_json`, with these keys (`msg`, `path`, maybe `details`).
    Failed(Map<String, Value>),
    /// The reference's module would end on this exception, as Python prints it; `true` when an
    /// owner or group was changed before it.
    Raised(String, bool),
}

/// `set_fs_attributes_if_different` without SELinux or `attributes`: owner, then group, then
/// mode, each read from the path again, each changed only when it differs, and the first failure
/// stops the rest, the attributes already set staying set. Returns whether anything changed.
pub fn set_fs_attributes(
    path: &str,
    owner: Option<&Account>,
    group: Option<&Account>,
    mode: Option<&Value>,
) -> Result<bool, FsError> {
    set_fs_attributes_diff(path, owner, group, mode, &mut Map::new())
}

/// `set_fs_attributes`, filling `diff` as the reference fills its `diff` argument: `before` and
/// `after` maps holding the `owner` and `group` ids and the `mode` text of each attribute about
/// to change, written before the change is tried.
pub fn set_fs_attributes_diff(
    path: &str,
    owner: Option<&Account>,
    group: Option<&Account>,
    mode: Option<&Value>,
    diff: &mut Map<String, Value>,
) -> Result<bool, FsError> {
    let mut record = |key: &str, before: Value, after: Value| {
        for (side, value) in [("before", before), ("after", after)] {
            if let Value::Object(map) = diff
                .entry(side)
                .or_insert_with(|| Value::Object(Map::new()))
            {
                map.insert(key.to_string(), value);
            }
        }
    };
    let mut changed = false;
    let lstat = |changed: bool| {
        fs::symlink_metadata(path).map_err(|err| FsError::Raised(os_error(&err, path), changed))
    };
    let lookup_failed = |msg: String| Err(FsError::Failed(failure(path, msg)));
    if let Some(owner) = owner {
        let current = lstat(changed)?.uid();
        let uid = match owner {
            Account::Id(uid) => *uid,
            Account::Unknown(name) => {
                return lookup_failed(format!("chown failed: failed to look up user {name}"));
            }
        };
        if current != uid {
            record("owner", current.into(), uid.into());
            std::os::unix::fs::lchown(path, Some(uid), None)
                .map_err(|_| FsError::Failed(failure(path, "chown failed".into())))?;
            changed = true;
        }
    }
    if let Some(group) = group {
        let current = lstat(changed)?.gid();
        let gid = match group {
            Account::Id(gid) => *gid,
            Account::Unknown(name) => {
                return lookup_failed(format!("chgrp failed: failed to look up group {name}"));
            }
        };
        if current != gid {
            record("group", current.into(), gid.into());
            std::os::unix::fs::lchown(path, None, Some(gid))
                .map_err(|_| FsError::Failed(failure(path, "chgrp failed".into())))?;
            changed = true;
        }
    }
    if let Some(mode) = mode {
        let meta = lstat(changed)?;
        let prev = meta.mode() & 0o7777;
        let mode = match parse_mode(mode, prev, meta.is_dir()) {
            Ok(mode) => mode,
            Err(ModeError::Invalid(details)) => {
                let mut fail = failure(path, "mode must be in octal or symbolic form".into());
                fail.insert("details".into(), Value::String(details));
                return Err(FsError::Failed(fail));
            }
            Err(ModeError::Unsupported(why)) => return Err(FsError::Raised(why, changed)),
        };
        if prev != mode {
            record(
                "mode",
                format!("0{prev:03o}").into(),
                format!("0{mode:03o}").into(),
            );
            // Python's chmod of a link goes through it and puts the target back; no native
            // sets a mode on a link.
            if meta.file_type().is_symlink() {
                return Err(FsError::Raised(
                    format!("{path} is a symbolic link"),
                    changed,
                ));
            }
            match fs::set_permissions(path, fs::Permissions::from_mode(mode)) {
                Err(err) if !matches!(err.raw_os_error(), Some(libc::ENOENT | libc::ELOOP)) => {
                    return Err(FsError::Raised(os_error(&err, path), changed));
                }
                _ => {}
            }
            changed |= lstat(changed)?.mode() & 0o7777 != prev;
        }
    }
    Ok(changed)
}

/// The keys of a `fail_json(path=path, msg=msg)`, before `failed` and `add_path_info`.
pub fn failure(path: &str, msg: String) -> Map<String, Value> {
    let mut fail = Map::new();
    fail.insert("msg".into(), Value::String(msg));
    fail.insert("path".into(), Value::String(path.to_string()));
    fail
}

/// `AnsibleModule.add_path_info`: what the reference adds to a result naming an existing `path`
/// (or `dest`): owner and group by id and by name, mode, kind and size.
///
/// A name the host cannot resolve reads as the id, as it does in the reference when there is no
/// such account; so does one whose lookup failed, which a native rules out before acting.
///
/// A lookup that outlives the task's `timeout`, or that the controller cancels, ends the answer
/// the way the Python module's would end.
pub fn add_path_info(result: &mut Map<String, Value>, clock: Clock) -> Result<(), Stop> {
    let path = match result.get("path") {
        Some(path) => path,
        None => match result.get("dest") {
            Some(dest) => dest,
            None => return Ok(()),
        },
    };
    let Some(path) = path.as_str().map(str::to_string) else {
        return Ok(());
    };
    let (Ok(stat), Ok(lstat)) = (fs::metadata(&path), fs::symlink_metadata(&path)) else {
        return Ok(());
    };
    let (uid, gid) = (lstat.uid(), lstat.gid());
    let name = |found: Result<Option<String>, Stop>, id: u32| match found {
        Ok(Some(name)) => Ok(name),
        Ok(None) | Err(Stop::HandBack(_)) => Ok(id.to_string()),
        Err(stop) => Err(stop),
    };
    let owner = name(
        lookup_user(&uid.to_string(), clock).map(|p| p.map(|p| p.name)),
        uid,
    )?;
    let group = name(
        lookup_group(&gid.to_string(), clock).map(|g| g.map(|g| g.name)),
        gid,
    )?;
    let state = if lstat.file_type().is_symlink() {
        "link"
    } else if stat.is_dir() {
        "directory"
    } else if stat.nlink() > 1 {
        "hard"
    } else {
        "file"
    };
    result.insert("uid".into(), uid.into());
    result.insert("gid".into(), gid.into());
    result.insert("owner".into(), owner.into());
    result.insert("group".into(), group.into());
    result.insert(
        "mode".into(),
        format!("0{:03o}", lstat.mode() & 0o7777).into(),
    );
    result.insert("state".into(), state.into());
    result.insert("size".into(), lstat.size().into());
    Ok(())
}

/// Python's `str()` of the `OSError` an `os` call on the bytes path `path` raises:
/// `[Errno 13] Permission denied: b'/x'`, with glibc's wording, which is what the reference's
/// interpreter prints on the hosts a native answers for.
pub fn os_error(err: &io::Error, path: &str) -> String {
    let Some(errno) = err.raw_os_error() else {
        return err.to_string();
    };
    let text = strerror(errno).map_or_else(|| err.to_string(), str::to_string);
    format!("[Errno {errno}] {text}: {}", bytes_repr(path))
}

/// `os.access(path, mode)`: by the real uid, following links.
pub fn access(path: &str, mode: libc::c_int) -> bool {
    CString::new(path).is_ok_and(|path| unsafe { libc::access(path.as_ptr(), mode) } == 0)
}

/// `os.path.realpath` for a path whose every link resolves; `Err` for one that does not, where
/// Python's non-strict answer would need reproducing.
pub fn realpath(path: &str) -> Result<String, String> {
    fs::canonicalize(path)
        .ok()
        .and_then(|real| real.to_str().map(str::to_string))
        .ok_or_else(|| format!("{path} does not resolve"))
}

/// `os.path.dirname(path)`, for a path without repeated slashes.
pub fn parent(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(at) => &path[..at],
        None => "",
    }
}

/// Whether the path, or the file a link there names, carries any extended attribute, ACLs
/// included: what `shutil.copystat` would copy.
#[cfg(target_os = "linux")]
pub fn has_xattrs(path: &str) -> bool {
    let Ok(path) = CString::new(path) else {
        return true;
    };
    // SAFETY: a null buffer of size 0 asks only for the size of the list.
    let size = unsafe { libc::listxattr(path.as_ptr(), std::ptr::null_mut(), 0) };
    size != 0 && !(size < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ENOTSUP))
}

/// Elsewhere a native hands every task back before it gets here.
#[cfg(not(target_os = "linux"))]
pub fn has_xattrs(_: &str) -> bool {
    true
}

/// `FS_IMMUTABLE_FL` and `FS_APPEND_FL`.
pub const IMMUTABLE: libc::c_long = 0x10;

pub const APPEND: libc::c_long = 0x20;

/// The flags a fresh copy on the same file system gets anyway: ext4's extents and inline data.
pub const AUTOMATIC: libc::c_long = 0x0008_0000 | 0x1000_0000;

/// Whether the file at `path` (a link followed) has one of the inode flags in `mask`. `lsattr`
/// fails where the flags cannot be read, and the reference then copies none; a file that cannot
/// be opened counts as flagged.
#[cfg(target_os = "linux")]
pub fn has_flags(path: &str, mask: libc::c_long) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return true;
    };
    let mut flags: libc::c_long = 0;
    // SAFETY: FS_IOC_GETFLAGS writes one `long` through the pointer.
    let rc = unsafe {
        libc::ioctl(
            std::os::fd::AsRawFd::as_raw_fd(&file),
            libc::FS_IOC_GETFLAGS,
            &raw mut flags,
        )
    };
    rc == 0 && flags & mask != 0
}

#[cfg(not(target_os = "linux"))]
pub fn has_flags(_: &str, _: libc::c_long) -> bool {
    true
}
