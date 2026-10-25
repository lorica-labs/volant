// SPDX-License-Identifier: GPL-3.0-or-later
//! `--place <remote_tmp>`: the agent puts itself in its cache directory, for a target with no
//! shell to run the controller's upload script. A runtime's own copy command (`docker cp`) left
//! this file somewhere it can be run; this copies it to
//! `<remote_tmp>/volant-agent-<version>-<uid>/volant-agent` under the rules the script applies,
//! prints that path, and removes the copy it ran from.
//!
//! The rules are the script's: `remote_tmp` belongs to this user, to root or to the user `sudo`
//! names in `SUDO_UID`, and other users cannot write to it unless it is sticky; the directory is a
//! real directory of this user with no group or other write bit, made again when it is this
//! user's own but too open; the program goes in under a temporary name and is renamed. Other
//! versions' directories of this user are removed.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The script's status for a directory another user could change.
const UNSAFE: i32 = 47;

/// Places the agent and prints where, or says why not on stderr and exits non-zero.
pub(crate) fn run(remote_tmp: Option<String>, from_copy: bool) -> i32 {
    let Some(remote_tmp) = remote_tmp else {
        eprintln!("volant-agent: --place needs the remote_tmp directory");
        return 1;
    };
    match place(&remote_tmp) {
        Ok(program) => {
            if from_copy
                && let Ok(source) = std::env::current_exe()
                && source != program
            {
                let _ = fs::remove_file(source);
            }
            println!("{}", program.display());
            0
        }
        Err(Refusal { code, message }) => {
            eprintln!("{message}");
            code
        }
    }
}

struct Refusal {
    code: i32,
    message: String,
}

fn failed(message: impl Into<String>) -> Refusal {
    Refusal {
        code: 1,
        message: message.into(),
    }
}

/// A refused path, in the script's words.
fn refused(path: &Path, meta: Option<&fs::Metadata>, why: &str) -> Refusal {
    let (mode, owner) = meta.map_or(("unknown".to_string(), "unknown".to_string()), |meta| {
        (
            format!("{:o}", meta.mode() & 0o7777),
            meta.uid().to_string(),
        )
    });
    Refusal {
        code: UNSAFE,
        message: format!(
            "{}: permissions {mode}, owner uid {owner}: {why}",
            path.display()
        ),
    }
}

fn place(remote_tmp: &str) -> Result<PathBuf, Refusal> {
    // SAFETY: `getuid` cannot fail and touches nothing.
    let uid = unsafe { libc::getuid() };
    let rt = expand(remote_tmp, uid)?;
    if !rt.is_dir() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&rt)
            .map_err(|e| failed(format!("{}: {e}", rt.display())))?;
    }
    let meta = fs::metadata(&rt).map_err(|e| failed(format!("{}: {e}", rt.display())))?;
    if !tmp_is_safe(&meta, uid) {
        return Err(refused(
            &rt,
            Some(&meta),
            &format!(
                "other users could replace the agent directory in it; it must belong to uid {uid}, to root or to the user who ran sudo, and other users must not be able to write to it unless it has the sticky bit"
            ),
        ));
    }
    let dir = rt.join(format!("volant-agent-{}-{uid}", env!("CARGO_PKG_VERSION")));
    let bad_dir =
        format!("it must be a real directory of uid {uid} with no group or other write bit");
    match fs::symlink_metadata(&dir) {
        Ok(meta) if is_own(&meta, uid, true) => {}
        Ok(meta) if meta.is_dir() && meta.uid() == uid => {
            // This user's own but too open: emptied of what an agent put there and made again.
            for name in ["volant-agent", "agent.stderr"] {
                let _ = fs::remove_file(dir.join(name));
            }
            fs::remove_dir(&dir).map_err(|_| refused(&dir, Some(&meta), &bad_dir))?;
            make_dir(&dir)?;
        }
        Ok(meta) => return Err(refused(&dir, Some(&meta), &bad_dir)),
        Err(_) => make_dir(&dir)?,
    }
    let meta = fs::symlink_metadata(&dir).map_err(|e| failed(format!("{}: {e}", dir.display())))?;
    if !is_own(&meta, uid, true) {
        return Err(refused(&dir, Some(&meta), &bad_dir));
    }
    let program = dir.join("volant-agent");
    let source = std::env::current_exe().map_err(|e| failed(format!("finding itself: {e}")))?;
    if source != program {
        let bytes = fs::read(&source).map_err(|e| failed(format!("{}: {e}", source.display())))?;
        let temporary = dir.join(format!("volant-agent.tmp.{}", std::process::id()));
        let written = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&temporary)
            .and_then(|mut file| file.write_all(&bytes).and_then(|()| file.sync_all()));
        if let Err(e) = written.and_then(|()| fs::rename(&temporary, &program)) {
            let _ = fs::remove_file(&temporary);
            return Err(failed(format!("{}: {e}", program.display())));
        }
    }
    sweep(&rt, &dir, uid);
    Ok(program)
}

/// `remote_tmp` with a leading `~` made this user's home, as the script's shell would.
fn expand(remote_tmp: &str, uid: libc::uid_t) -> Result<PathBuf, Refusal> {
    let rest = match remote_tmp.strip_prefix('~') {
        None => return Ok(PathBuf::from(remote_tmp)),
        Some(rest) if rest.is_empty() || rest.starts_with('/') => rest.trim_start_matches('/'),
        Some(_) => {
            return Err(failed(format!(
                "remote_tmp '{remote_tmp}' names another user's home, which this target cannot expand"
            )));
        }
    };
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_of(uid))
        .ok_or_else(|| {
            failed(format!(
                "remote_tmp '{remote_tmp}' starts with '~' and uid {uid} has no home directory here: set ansible_remote_tmp to an absolute path"
            ))
        })?;
    Ok(home.join(rest))
}

fn home_of(uid: libc::uid_t) -> Option<PathBuf> {
    // SAFETY: `getpwuid` returns null or a pointer into static storage that stays valid until the
    // next call; this process makes no other call to it, and the string is copied out at once.
    unsafe {
        let entry = libc::getpwuid(uid);
        if entry.is_null() || (*entry).pw_dir.is_null() {
            return None;
        }
        let dir = std::ffi::CStr::from_ptr((*entry).pw_dir)
            .to_string_lossy()
            .into_owned();
        (!dir.is_empty()).then(|| PathBuf::from(dir))
    }
}

/// The script's `oktmp`, less the case of a group-writable home it cannot judge without `id -g`.
fn tmp_is_safe(meta: &fs::Metadata, uid: libc::uid_t) -> bool {
    let sudo_uid = std::env::var("SUDO_UID")
        .ok()
        .and_then(|id| id.parse().ok());
    let owner = meta.uid();
    let mode = meta.permissions().mode();
    meta.is_dir()
        && (owner == uid || owner == 0 || Some(owner) == sudo_uid)
        && (mode & 0o1000 != 0 || mode & 0o002 == 0)
        && (mode & 0o020 == 0 || owner == uid)
}

/// The script's `okmode`: the right kind, this user's, and no group or other write bit.
fn is_own(meta: &fs::Metadata, uid: libc::uid_t, dir: bool) -> bool {
    let kind = if dir { meta.is_dir() } else { meta.is_file() };
    kind && !meta.file_type().is_symlink() && meta.uid() == uid && meta.mode() & 0o022 == 0
}

fn make_dir(dir: &Path) -> Result<(), Refusal> {
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(()),
        Err(_) if dir.is_dir() => Ok(()),
        Err(e) => Err(failed(format!("{}: {e}", dir.display()))),
    }
}

/// Removes the other versions' directories of this user that nobody else can write.
fn sweep(rt: &Path, keep: &Path, uid: libc::uid_t) {
    let Ok(entries) = fs::read_dir(rt) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ours = entry
            .file_name()
            .to_string_lossy()
            .starts_with("volant-agent-");
        if ours
            && path != keep
            && fs::symlink_metadata(&path).is_ok_and(|meta| is_own(&meta, uid, true))
        {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

#[cfg(test)]
mod tests;
