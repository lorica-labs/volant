// SPDX-License-Identifier: GPL-3.0-or-later
//! The directories a setting names: the defaults, the home directory, and path lists.

use std::path::{Path, PathBuf};

/// A home-relative directory first, then the two system ones: the reference's own default for
/// `roles_path`, read off the list a missing role prints.
pub(super) fn default_roles_path() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = home() {
        paths.push(home.join(".ansible/roles"));
    }
    paths.push(PathBuf::from("/usr/share/ansible/roles"));
    paths.push(PathBuf::from("/etc/ansible/roles"));
    paths
}

pub(super) fn default_collections_path() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = home() {
        paths.push(home.join(".ansible/collections"));
    }
    paths.push(PathBuf::from("/usr/share/ansible/collections"));
    paths
}

/// The user's home directory. `HOME` everywhere the engine runs a playbook, `USERPROFILE` on the
/// Windows controller, where reading `HOME` alone drops the `~/.ansible` entry from the search
/// path and a role installed there stops being found.
pub(super) fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// `path` with a leading `~` or `~/` replaced by the home directory, anchored on `base` when it
/// is still relative.
pub(super) fn expand_home(path: &str, base: &Path) -> PathBuf {
    match (path.strip_prefix('~'), home()) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            home.join(rest.trim_start_matches('/'))
        }
        _ => base.join(path),
    }
}

/// A colon-separated list of directories, as every Ansible path setting is written. Relative
/// entries are anchored against `base`, which is the configuration file's directory for a value
/// read from one and the working directory for one read from the environment. Every entry comes
/// out absolute, with `.` and `..` folded out of its text, as the reference's `unfrackpath`
/// leaves it: `./roles` read from `./ansible.cfg` is `<cwd>/roles`.
pub(super) fn path_list(value: &str, base: &Path) -> Vec<PathBuf> {
    value
        .split(':')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| crate::roles::folded(&expand_home(p, base)))
        .collect()
}
