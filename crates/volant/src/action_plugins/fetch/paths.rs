// SPDX-License-Identifier: GPL-3.0-or-later
//! The path work of `fetch`: `dest` made absolute, normalised, and walked without following links.

use std::path::{Component, Path, PathBuf};

/// `dest` as the reference reads it: `~` for the controller's home, relative to the playbook.
pub(super) fn absolute(dest: &str, playbook_dir: &Path) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match (dest.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            PathBuf::from(format!("{}{rest}", home.display()))
        }
        _ => playbook_dir.join(dest),
    }
}

/// A path with `.` dropped and `..` taken off the component before it, and nothing looked up:
/// a link in it is not followed, it is refused later by [`walk`].
pub(super) fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // `/..` is `/`, as `os.path.normpath` has it.
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other),
        }
    }
    out
}

/// Every component of `path` below `root`, refused when it is a symbolic link; with `create`, the
/// directories that are missing made one by one on the way down. With `guarded`, a directory that
/// already exists is refused when another user could change it (see [`super::dest::guard`]).
pub(super) fn walk(root: &Path, path: &Path, create: bool, guarded: bool) -> Result<(), String> {
    let fail = |err: std::io::Error| format!("Failed to fetch the file: {err}");
    if create {
        std::fs::create_dir_all(root).map_err(fail)?;
    }
    let below: Vec<Component<'_>> = path
        .strip_prefix(root)
        .map_err(|_| {
            format!(
                "the fetched path {} is outside '{}'",
                path.display(),
                root.display()
            )
        })?
        .components()
        .collect();
    let mut at = root.to_path_buf();
    for (i, part) in below.iter().enumerate() {
        at.push(part);
        match std::fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "the fetched path {} goes through the symbolic link {}, and fetch never follows one",
                    path.display(),
                    at.display()
                ));
            }
            Ok(meta) if guarded && meta.is_dir() => super::dest::guard(&at, &meta)?,
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if create && i + 1 < below.len() {
                    std::fs::create_dir(&at).map_err(fail)?;
                }
            }
            // A component that is a file: nothing below it can exist, and the write says so.
            Err(_) if !create => return Ok(()),
            Err(err) => return Err(fail(err)),
        }
    }
    Ok(())
}
