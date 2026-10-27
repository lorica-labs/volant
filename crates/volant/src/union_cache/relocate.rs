// SPDX-License-Identifier: GPL-3.0-or-later
//! Whether a union's key has to name the working directory, and the paths under it written
//! relative to it when it does not.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Whether a build here can read a file through the working directory that the key does not
/// stamp: a relative path in an `ANSIBLE_*` or `PYTHON*` path variable, an `ansible.cfg` under it
/// (its relative paths are read against its own directory), or a `collections/` beside a playbook
/// or role under it. The key then names the directory, as two directories holding those files
/// can differ where the key cannot see. Otherwise the run's union does not depend on where it
/// runs, and the same project in another checkout finds the entry this one wrote.
pub(super) fn reads_through(
    cwd: &Path,
    env: &[(OsString, OsString)],
    configs: &[(PathBuf, Vec<u8>)],
    plugin_dirs: &[PathBuf],
) -> bool {
    env.iter().any(|(name, value)| {
        let (Some(name), Some(value)) = (name.to_str(), value.to_str()) else {
            return name.as_encoded_bytes().starts_with(b"ANSIBLE_");
        };
        relative_path_in(name, value)
    }) || configs
        .iter()
        .any(|(path, _)| path.is_relative() || path.starts_with(cwd))
        || plugin_dirs
            .iter()
            .any(|dir| dir.starts_with(cwd) && dir.join("collections").exists())
}

/// Whether `value`, read as ansible-core reads the variable `name`, holds a path relative to the
/// working directory. A path variable is any `ANSIBLE_*` whose name says it holds one, and the
/// `PYTHON*` variables that name directories; any other value counts when one of its `:` or `,`
/// separated parts looks like a path (holds a `/`, or starts with `.`). An absolute part, or one
/// starting with `~` or `$`, does not count.
fn relative_path_in(name: &str, value: &str) -> bool {
    const PATH_ENDINGS: [&str; 11] = [
        "_PATH",
        "_PATHS",
        "_DIR",
        "_DIRS",
        "_FILE",
        "_CONFIG",
        "_ROOT",
        "_LIBRARY",
        "_PLUGINS",
        "_INVENTORY",
        "_UTILS",
    ];
    const PYTHON_PATHS: [&str; 5] = [
        "PYTHONPATH",
        "PYTHONHOME",
        "PYTHONUSERBASE",
        "PYTHONSTARTUP",
        "PYTHONPYCACHEPREFIX",
    ];
    let path_variable = if name.starts_with("ANSIBLE_") {
        PATH_ENDINGS.iter().any(|end| name.ends_with(end))
    } else if name.starts_with("PYTHON") {
        PYTHON_PATHS.contains(&name)
    } else {
        return false;
    };
    value
        .split([':', ','])
        .map(str::trim)
        .filter(|part| !part.is_empty() && !part.starts_with(['/', '~', '$']))
        .any(|part| path_variable || part.contains('/') || part.starts_with('.'))
}

/// `bytes` with every path under `cwd` it holds written from `<cwd>` on: a key that does not
/// name the working directory still tells a file under it from one elsewhere.
pub(super) fn relative_to(bytes: &[u8], cwd: &[u8]) -> Vec<u8> {
    if cwd.is_empty() {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some(at) = rest.windows(cwd.len()).position(|window| window == cwd) {
        let after = &rest[at + cwd.len()..];
        out.extend_from_slice(&rest[..at]);
        // Only the directory itself or a path inside it: `/work` in `/workshop` is left as it is.
        if after.is_empty() || matches!(after[0], b'/' | b':' | b',' | 0) {
            out.extend_from_slice(b"<cwd>");
        } else {
            out.extend_from_slice(cwd);
        }
        rest = after;
    }
    out.extend_from_slice(rest);
    out
}
