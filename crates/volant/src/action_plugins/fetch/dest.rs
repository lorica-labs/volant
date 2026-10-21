// SPDX-License-Identifier: GPL-3.0-or-later
//! Where a `fetch` may write when a managed host named its `dest`, once the protection is on:
//! `[volant] host_named_dest_dirs` is set, or `[volant] host_named_paths` is `contained` or
//! `refuse`.
//!
//! A `dest` of this kind is refused unless the setting names a directory above it. The decision
//! is made on the path the file system resolves, not on the text: the deepest part that exists
//! is resolved through every link, and the rest is appended. The write then walks down from the
//! directory of the list and refuses any link and any directory another user controls below it,
//! so nothing can be swapped between the check and the write.

use std::path::{Component, Path, PathBuf};

use crate::config::HostNamedPaths;

/// The refusal when the protection is on and the setting names no directory.
const NEVER: &str = "the 'dest' of this task was named by a managed host, and a controller path a host chose is never written unless host_named_dest_dirs names a directory above it";

/// The refusal of a `dest` that the setting does not cover, naming the setting.
fn refusal(dest: &str, why: &str, dirs: &[PathBuf]) -> String {
    let list: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
    format!(
        "the 'dest' of this task was named by a managed host, and '{dest}' {why} (host_named_dest_dirs = {})",
        list.join(":")
    )
}

/// The directories a `dest` a host named must stay inside, or `None` when it is not limited, as
/// in the reference. The setting, when present, always limits: an empty list refuses every such
/// `dest`. Without it, `contained` and `refuse` limit to no directory at all.
pub(super) fn limit(mode: HostNamedPaths, dirs: Option<&[PathBuf]>) -> Option<&[PathBuf]> {
    match (dirs, mode) {
        (Some(dirs), _) => Some(dirs),
        (None, HostNamedPaths::Allow) => None,
        (None, HostNamedPaths::Contained | HostNamedPaths::Refuse) => Some(&[]),
    }
}

/// What the text of `dest` says, before any path is built from it: nothing is accepted when
/// `dirs` is empty, and a path that is not absolute or holds `..` is never accepted. A `~` is not
/// absolute, so it is refused here and never expanded.
pub(super) fn check_text(dest: &str, dirs: &[PathBuf]) -> Result<(), String> {
    if dirs.is_empty() {
        return Err(NEVER.into());
    }
    let path = Path::new(dest);
    if !path.is_absolute() {
        return Err(refusal(dest, "is not an absolute path", dirs));
    }
    if path.components().any(|part| part == Component::ParentDir) {
        return Err(refusal(dest, "has a '..' component", dirs));
    }
    Ok(())
}

/// What the file system says about `root`, the `dest` made absolute: the path it resolves to
/// must lie strictly inside one of `dirs`, which are themselves resolved, and that directory must
/// not be writable by other users unless its sticky bit is set, as it is on `/tmp`. Run again
/// just before the write, so a link planted after the first check is seen.
///
/// Returns the anchor: the shortest ancestor of `root` that resolves to that directory. The write
/// walks down from it, refusing every link below it, so a path that reaches the directory only
/// through a link elsewhere is refused here.
pub(super) fn check_resolved(root: &Path, dest: &str, dirs: &[PathBuf]) -> Result<PathBuf, String> {
    let real =
        resolve(root).map_err(|err| refusal(dest, &format!("cannot be resolved: {err}"), dirs))?;
    let mut shared = None;
    for dir in dirs {
        let Ok(base) = dir.canonicalize() else {
            continue;
        };
        if !real.starts_with(&base) || real == base {
            continue;
        }
        if open_to_others(&base) {
            shared = Some(base);
            continue;
        }
        let anchor = root
            .ancestors()
            .filter(|at| at.canonicalize().is_ok_and(|at| at == base))
            .last();
        return anchor.map(Path::to_path_buf).ok_or_else(|| {
            refusal(
                dest,
                &format!("reaches '{}' only through a link", base.display()),
                dirs,
            )
        });
    }
    let why = match (shared, real == root) {
        (Some(base), _) => format!(
            "is inside '{}', which other users can write to",
            base.display()
        ),
        (None, true) => "is not inside any of the directories".to_string(),
        (None, false) => format!(
            "resolves to '{}', which is not inside any of the directories",
            real.display()
        ),
    };
    Err(refusal(dest, &why, dirs))
}

/// `path` with every link in it followed: the deepest ancestor that exists, resolved, with the
/// parts that do not exist yet appended. A link that points nowhere is an error.
fn resolve(path: &Path) -> std::io::Result<PathBuf> {
    let mut missing = Vec::new();
    let mut at = path;
    while std::fs::symlink_metadata(at).is_err() {
        let (Some(parent), Some(name)) = (at.parent(), at.file_name()) else {
            break;
        };
        missing.push(name);
        at = parent;
    }
    let mut real = at.canonicalize()?;
    real.extend(missing.iter().rev());
    Ok(real)
}

/// Whether another user, or a group, can create files in `dir` without the sticky bit that keeps
/// each from touching the files of the others.
#[cfg(unix)]
fn open_to_others(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(dir).map_or(true, |meta| {
        let mode = meta.permissions().mode();
        mode & 0o022 != 0 && mode & 0o1000 == 0
    })
}

/// A platform without Unix mode bits has no such bit to test.
#[cfg(not(unix))]
fn open_to_others(_dir: &Path) -> bool {
    false
}

/// The refusal of a directory that already exists below the anchor and that another user could
/// change between the check and the write: one owned by another account than this one or root,
/// or one every user can write without the sticky bit. The directories `fetch` creates itself
/// pass, so a run that fetches again into them is not refused.
#[cfg(unix)]
pub(super) fn guard(dir: &Path, meta: &std::fs::Metadata) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt as _;
    let (uid, mode) = (meta.uid(), meta.mode());
    let euid = crate::askpass::effective_uid();
    if (uid == euid || uid == 0) && (mode & 0o002 == 0 || mode & 0o1000 != 0) {
        return Ok(());
    }
    Err(format!(
        "the 'dest' of this task was named by a managed host, and '{}' is owned by uid {uid} with mode {:o}; below host_named_dest_dirs, fetch writes only through directories this user or root owns and other users cannot write",
        dir.display(),
        mode & 0o7777
    ))
}

/// A platform without Unix owners has nothing to test.
#[cfg(not(unix))]
pub(super) fn guard(_dir: &Path, _meta: &std::fs::Metadata) -> Result<(), String> {
    Ok(())
}
