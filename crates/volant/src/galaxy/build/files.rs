// SPDX-License-Identifier: GPL-3.0-or-later
//! The files of a collection build: the walk of the source tree, links, modes, and the JSON
//! the manifests are written in.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::{Entry, IGNORED_DIRS};
use crate::galaxy::Failure;

/// `_walk`: the entries under `dir`, `rel` being its path from the top, in name order.
pub(super) fn walk(
    dir: &Path,
    top: &Path,
    rel: &str,
    patterns: &[String],
    verbose: u8,
    out: &mut Vec<Entry>,
) -> Result<(), Failure> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| Failure::error(e.to_string()))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for item in names {
        let path = dir.join(&item);
        let rel_path = if rel.is_empty() {
            item.clone()
        } else {
            format!("{rel}/{item}")
        };
        let ignored = patterns.iter().any(|p| fnmatch(p, &rel_path));
        if path.is_dir() {
            if IGNORED_DIRS.contains(&item.as_str()) || ignored {
                if verbose >= 3 {
                    println!("Skipping '{}' for collection build", path.display());
                }
                continue;
            }
            let is_link =
                std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
            if is_link {
                let real = std::fs::canonicalize(&path).unwrap_or_default();
                if !real.starts_with(std::fs::canonicalize(top).unwrap_or_default()) {
                    eprintln!(
                        "[WARNING]: Skipping '{}' as it is a symbolic link to a directory outside the collection",
                        path.display()
                    );
                    continue;
                }
            }
            out.push(Entry {
                name: rel_path.clone(),
                dir: true,
                sha256: None,
            });
            if !is_link {
                walk(&path, top, &rel_path, patterns, verbose, out)?;
            }
        } else {
            if ignored {
                if verbose >= 3 {
                    println!("Skipping '{}' for collection build", path.display());
                }
                continue;
            }
            let data = std::fs::read(&path).map_err(|e| Failure::error(e.to_string()))?;
            out.push(Entry {
                name: rel_path,
                dir: false,
                sha256: Some(format!("{:x}", Sha256::digest(&data))),
            });
        }
    }
    Ok(())
}

/// A link inside the collection, as the path from the link's directory to its target; `None`
/// for anything else. A link whose target does not exist is an error.
pub(super) fn inner_link(source: &Path, top: &Path) -> Result<Option<PathBuf>, Failure> {
    if !std::fs::symlink_metadata(source).is_ok_and(|m| m.file_type().is_symlink()) {
        return Ok(None);
    }
    let Ok(real) = std::fs::canonicalize(source) else {
        let target = std::fs::read_link(source).unwrap_or_default();
        return Err(Failure::error(format!(
            "Failed to find the target path '{}' for the symlink '{}'.",
            source.parent().unwrap_or(top).join(target).display(),
            source.display()
        )));
    };
    let top = std::fs::canonicalize(top).unwrap_or_default();
    if !real.starts_with(&top) {
        return Ok(None);
    }
    let from = std::fs::canonicalize(source.parent().unwrap_or(&top)).unwrap_or_default();
    Ok(Some(relative(&real, &from)))
}

/// `os.path.relpath(path, start)` for two absolute paths.
pub(super) fn relative(path: &Path, start: &Path) -> PathBuf {
    let path: Vec<_> = path.components().collect();
    let start: Vec<_> = start.components().collect();
    let common = path.iter().zip(&start).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..start.len() {
        out.push("..");
    }
    for part in &path[common..] {
        out.push(part);
    }
    out
}

/// `fnmatch.fnmatch`: `*` matches across `/` too.
pub(super) fn fnmatch(pattern: &str, name: &str) -> bool {
    crate::inventory::glob_match(pattern, name)
}

/// Whether the owner may execute the file.
pub(super) fn executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o100 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

pub(super) fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

pub(super) fn make_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    set_mode(path, 0o755)
}

pub(super) fn copy_link(source: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(std::fs::read_link(source)?, target)
    }
    #[cfg(not(unix))]
    {
        std::fs::copy(source, target).map(|_| ())
    }
}

/// `json.dumps(value, indent=True)`: one space of indent, `", "` never used between lines, and
/// every character outside ASCII escaped.
pub(super) fn python_json(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    serde::Serialize::serialize(value, &mut serializer).expect("JSON");
    let text = String::from_utf8(out).expect("UTF-8");
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii() {
            escaped.push(c);
        } else {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                let _ = write!(escaped, "\\u{unit:04x}");
            }
        }
    }
    escaped.into_bytes()
}
