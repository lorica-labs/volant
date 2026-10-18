// SPDX-License-Identifier: GPL-3.0-or-later
//! Which Python interpreters this host has, in the order the reference prefers them.
//!
//! Two rules decide what comes out. A candidate is reported **at the path the walk found it**,
//! never at the path it points to: `python3 -m venv` symlinks the virtualenv's `python3.12` at
//! the system one, and running a module under the target puts the virtualenv's `site-packages`
//! off `sys.path`. And a candidate is reported only when this process could actually run it,
//! which is a question about the caller as much as about the file.

use std::path::{Path, PathBuf};

use volant_protocol::interpreter::CANDIDATES;

/// What tells two names for one interpreter apart from two interpreters: the file itself, by
/// device and inode. Symlinks and hardlinks both collapse, which canonicalising the path alone
/// would not do for a hardlink.
type File = (u64, u64);

/// Every Python this host has, best first, as absolute paths. Empty when it has none.
pub fn discover() -> Vec<String> {
    discover_in(&CANDIDATES, &std::env::var("PATH").unwrap_or_default())
}

/// [`discover`] with the candidate list and the `PATH` handed in.
///
/// A bare name is resolved by walking `path_env` and taking the first entry that holds a file
/// this process could execute, which is what a shell would run; a candidate starting with `/` is
/// taken as it stands. A file already reported is dropped **without moving the entry that
/// reported it first**: on most Linux hosts `python3.12`, `/usr/bin/python3` and `python3` are
/// one file, and re-ordering here is re-ordering the reference's preference.
fn discover_in(candidates: &[&str], path_env: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut seen: Vec<File> = Vec::new();
    for candidate in candidates {
        let Some((at, file)) = resolve(candidate, path_env) else {
            continue;
        };
        if seen.contains(&file) {
            continue;
        }
        // A path whose bytes are not UTF-8 is legal on Linux. Skipped rather than converted:
        // the replacement character would make a string that names no file on the host, and the
        // controller would try to execute it.
        let Ok(path) = at.into_os_string().into_string() else {
            continue;
        };
        seen.push(file);
        found.push(path);
    }
    found
}

/// The file a candidate names, as it was named, with the identity that de-duplicates it.
///
/// Only absolute `PATH` entries are searched. A relative one - `bin`, or the `.` a legacy profile
/// leaves behind - would be resolved against the agent's own working directory, wherever the ssh
/// exec dropped it, and an executable sitting there is not an interpreter the reference would
/// have found.
fn resolve(candidate: &str, path_env: &str) -> Option<(PathBuf, File)> {
    if candidate.starts_with('/') {
        return executable(Path::new(candidate));
    }
    path_env
        .split(':')
        .filter(|entry| entry.starts_with('/'))
        .find_map(|entry| executable(&Path::new(entry).join(candidate)))
}

/// `at` and its identity, when `at` is a file this process could execute.
///
/// `access(X_OK)` rather than the owner's execute bit: a root-owned `0o700` interpreter has that
/// bit set and still cannot be run by the agent, and a `noexec` mount has every bit set and runs
/// nothing. Reporting either in first position would hand the controller an interpreter that
/// answers `EACCES` for every module while a working one sits behind it, which is not what a
/// shell resolving the same name would do.
#[cfg(unix)]
fn executable(at: &Path) -> Option<(PathBuf, File)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    let meta = std::fs::metadata(at).ok()?;
    if !meta.is_file() {
        return None;
    }
    let path = CString::new(at.as_os_str().as_bytes()).ok()?;
    // SAFETY: `path` is a NUL-terminated C string that outlives the call, and `access` only
    // reads it.
    if unsafe { libc::access(path.as_ptr(), libc::X_OK) } != 0 {
        return None;
    }
    Some((at.to_path_buf(), (meta.dev(), meta.ino())))
}

/// The agent runs on a managed unix host and nowhere else. Off unix there is no executable bit
/// to ask about, and a path from a developer's machine is not one a controller could run, so
/// nothing is reported rather than something that would have to be filtered out later.
#[cfg(not(unix))]
fn executable(_at: &Path) -> Option<(PathBuf, File)> {
    None
}

#[cfg(test)]
mod tests;
