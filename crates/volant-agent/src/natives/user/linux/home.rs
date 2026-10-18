// SPDX-License-Identifier: GPL-3.0-or-later

//! The home `useradd -m` left missing: the skeleton copied, the mode from `login.defs`, the owner.

use std::path::Path;

use crate::natives::common::strerror;

/// Python's `str()` of the `OSError` an `os` call on the text path `path` raises.
pub(super) fn os_error(err: &std::io::Error, path: &Path) -> String {
    let errno = err.raw_os_error().unwrap_or(0);
    let text = strerror(errno).map_or_else(|| err.to_string(), str::to_string);
    format!("[Errno {errno}] {text}: '{}'", path.display())
}

/// `create_homedir` then `chown_homedir`, for a home `useradd -m` left missing: the skeleton
/// copied as `shutil.copytree(symlinks=True)` copies it (or the directory made when there is
/// no skeleton), its mode from `login.defs`, then the account given the home and everything
/// in it.
pub(super) fn create_home(
    skel: &Path,
    login_defs: &Path,
    home: &Path,
    uid: u32,
    gid: u32,
) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    if skel.exists() {
        copy_tree(skel, home)?;
    } else {
        std::fs::create_dir_all(home).map_err(|err| os_error(&err, home))?;
    }
    if login_defs.exists() {
        let defs = std::fs::read_to_string(login_defs).map_err(|err| os_error(&err, login_defs))?;
        // Neither set: `useradd`'s own UMASK of 022.
        let mut mode = 0o755;
        for line in defs.split('\n') {
            if let Some(value) = setting(line, "HOME_MODE") {
                mode = value?;
                break;
            }
            if let Some(value) = setting(line, "UMASK") {
                mode = 0o777 & !value?;
            }
        }
        std::fs::set_permissions(home, std::fs::Permissions::from_mode(mode))
            .map_err(|err| os_error(&err, home))?;
    }
    let chown = |path: &Path| {
        std::os::unix::fs::chown(path, Some(uid), Some(gid)).map_err(|err| os_error(&err, path))
    };
    chown(home)?;
    chown_tree(home, &chown)
}

/// `re.match(r'^<name>\s+(\d+)$', line)`, the digits read by `int(_, 8)`.
pub(super) fn setting(line: &str, name: &str) -> Option<Result<u32, String>> {
    let rest = line.strip_prefix(name)?;
    let digits = rest.trim_start();
    if digits.len() == rest.len() || digits.is_empty() || !digits.chars().all(char::is_numeric) {
        return None;
    }
    Some(
        u32::from_str_radix(digits, 8)
            .map_err(|_| format!("invalid literal for int() with base 8: '{digits}'")),
    )
}

/// `shutil.copytree(src, dst, symlinks=True)`: a link copied as a link with its times, a
/// file with its content, mode and times (`copy2`), a directory's mode and times set once it
/// is filled. Extended attributes, which `copy2` also copies, are not.
fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|err| os_error(&err, dst))?;
    for entry in std::fs::read_dir(src).map_err(|err| os_error(&err, src))? {
        let entry = entry.map_err(|err| os_error(&err, src))?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        let meta = std::fs::symlink_metadata(&from).map_err(|err| os_error(&err, &from))?;
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&from).map_err(|err| os_error(&err, &from))?;
            std::os::unix::fs::symlink(&target, &to).map_err(|err| os_error(&err, &to))?;
            copy_stat(&meta, &to, false)?;
        } else if meta.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).map_err(|err| os_error(&err, &from))?;
            copy_stat(&meta, &to, true)?;
        }
    }
    let meta = std::fs::metadata(src).map_err(|err| os_error(&err, src))?;
    copy_stat(&meta, dst, true)
}

/// `shutil.copystat` without extended attributes: the mode (not for a link, which Linux
/// cannot `chmod`), and the access and modification times to the nanosecond.
fn copy_stat(meta: &std::fs::Metadata, to: &Path, follow: bool) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if follow {
        std::fs::set_permissions(to, std::fs::Permissions::from_mode(meta.mode() & 0o7777))
            .map_err(|err| os_error(&err, to))?;
    }
    let times = [
        libc::timespec {
            tv_sec: meta.atime(),
            tv_nsec: meta.atime_nsec(),
        },
        libc::timespec {
            tv_sec: meta.mtime(),
            tv_nsec: meta.mtime_nsec(),
        },
    ];
    let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
    let path = std::ffi::CString::new(to.as_os_str().as_encoded_bytes())
        .map_err(|_| format!("{} holds a NUL byte", to.display()))?;
    if unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), flags) } != 0 {
        return Err(os_error(&std::io::Error::last_os_error(), to));
    }
    Ok(())
}

/// The loop of `chown_homedir` over `os.walk(home)`: every directory, a link to one
/// included (and followed), and every file that is not a link. A directory that cannot be
/// listed is skipped, as `os.walk` skips it.
fn chown_tree(dir: &Path, chown: &dyn Fn(&Path) -> Result<(), String>) -> Result<(), String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let link = entry.file_type().is_ok_and(|kind| kind.is_symlink());
        if std::fs::metadata(&path).is_ok_and(|meta| meta.is_dir()) {
            chown(&path)?;
            if !link {
                chown_tree(&path, chown)?;
            }
        } else if !link {
            chown(&path)?;
        }
    }
    Ok(())
}
