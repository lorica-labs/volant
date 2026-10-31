// SPDX-License-Identifier: GPL-3.0-or-later
//! Which `ansible.cfg` a run reads, with the rule of ansible-core's `find_ini_config_file`: a
//! working directory that other users can write is not a source, because any of them could
//! plant a file there that names a program to run.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The mode bit "write for others". The reference tests only this bit: the sticky bit and the
/// owner of the directory change nothing.
#[cfg(unix)]
const WORLD_WRITABLE: u32 = 0o002;

/// The outcome of the search.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Located {
    /// The file the run reads, if there is one.
    pub(super) path: Option<PathBuf>,
    /// An `ansible.cfg` exists in a world-writable working directory, was not used, and the
    /// run did not read `ANSIBLE_CONFIG` instead: the reference prints a warning then.
    pub(super) skipped: bool,
}
/// `ANSIBLE_CONFIG`, then `./ansible.cfg` unless the working directory is world-writable,
/// `~/.ansible.cfg`, `/etc/ansible/ansible.cfg`.
pub(crate) fn locate() -> Option<PathBuf> {
    current().path
}

/// The `ANSIBLE_CONFIG` the Python helper gets when this run skipped an `ansible.cfg` in a
/// world-writable directory: the file the run reads, or none, so the helper's ansible-core reads
/// the same and does not print the warning the run printed already.
pub(crate) fn helper_config() -> Option<PathBuf> {
    let found = current();
    let none = if cfg!(windows) { "NUL" } else { "/dev/null" };
    found
        .skipped
        .then(|| found.path.unwrap_or_else(|| PathBuf::from(none)))
}

/// The search for this process: its environment and its working directory.
pub(super) fn current() -> Located {
    let home = super::paths::home();
    find(
        std::env::var_os("ANSIBLE_CONFIG").as_deref(),
        Path::new("."),
        home.as_deref(),
        Path::new("/etc/ansible/ansible.cfg"),
    )
}

/// `explicit` (`ANSIBLE_CONFIG`), then `cwd/ansible.cfg`, `home/.ansible.cfg` and `system`.
///
/// A set `ANSIBLE_CONFIG` ends the search, whether or not it names a file. `cwd` is skipped
/// when its mode has the world-writable bit; a working directory that cannot be read is
/// skipped too, without a warning.
pub(super) fn find(
    explicit: Option<&OsStr>,
    cwd: &Path,
    home: Option<&Path>,
    system: &Path,
) -> Located {
    let in_cwd = cwd.join("ansible.cfg");
    let world_writable = is_world_writable(cwd);
    let skipped = world_writable && in_cwd.exists();
    if let Some(explicit) = explicit {
        let path = PathBuf::from(explicit);
        let used = path.is_file();
        return Located {
            path: used.then_some(path),
            skipped: skipped && !used,
        };
    }
    let mut candidates = Vec::new();
    if !world_writable {
        candidates.push(in_cwd);
    }
    if let Some(home) = home {
        candidates.push(home.join(".ansible.cfg"));
    }
    candidates.push(system.to_path_buf());
    Located {
        path: candidates.into_iter().find(|p| p.is_file()),
        skipped,
    }
}

/// Whether `dir` has the world-write bit. A directory that cannot be read counts as writable:
/// it is then no source, which is what the reference does when it cannot read the directory.
#[cfg(unix)]
pub(crate) fn is_world_writable(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(dir).map_or(true, |meta| meta.permissions().mode() & WORLD_WRITABLE != 0)
}

/// A platform without Unix mode bits has no "writable by others" bit to test, so no directory
/// is skipped there. Its access lists are not part of the reference's rule either.
#[cfg(not(unix))]
pub(crate) fn is_world_writable(_dir: &Path) -> bool {
    false
}

/// The warning line of the reference, without the `[WARNING]: ` prefix. The address is the
/// one the reference prints, so a script that matches it matches here.
pub(super) fn world_writable_warning(cwd: &Path) -> String {
    format!(
        "Ansible is being run in a world writable directory ({}), ignoring it as an ansible.cfg \
         source. For more information see \
         https://docs.ansible.com/ansible/devel/reference_appendices/config.html#cfg-in-world-writable-dir",
        cwd.display()
    )
}

/// Prints the warning once for the process: `Config::load` runs more than once in a run.
pub(super) fn warn_once() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        let cwd = std::env::current_dir().unwrap_or_default();
        eprintln!("[WARNING]: {}", world_writable_warning(&cwd));
    });
}

/// The value of `key` in `[defaults]` of the configuration file in use, as written and trimmed;
/// `None` when there is no file, no such key or an empty value.
pub(crate) fn defaults_value(key: &str) -> Option<String> {
    let text = std::fs::read_to_string(current().path?).ok()?;
    value_in(&text, key)
}

/// [`defaults_value`] over the text of a file.
fn value_in(text: &str, key: &str) -> Option<String> {
    let mut section = "";
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim();
        } else if section == "defaults"
            && !line.starts_with(['#', ';'])
            && let Some((name, value)) = line.split_once('=')
            && name.trim() == key
        {
            // A `;` or `#` after whitespace starts a comment, as in ansible's own ini reading.
            let end = value
                .char_indices()
                .zip(value.chars().skip(1))
                .find(|((_, before), after)| before.is_whitespace() && matches!(after, ';' | '#'))
                .map_or(value.len(), |((at, _), _)| at);
            return Some(value[..end].trim().to_string()).filter(|v| !v.is_empty());
        }
    }
    None
}

#[cfg(test)]
mod value_tests {
    use super::value_in;
    use rstest::rstest;

    #[rstest]
    #[case::found("[defaults]\neditor = nano\n", Some("nano"))]
    #[case::trimmed("[defaults]\n  editor   =   nano -w  \n", Some("nano -w"))]
    #[case::missing("[defaults]\nforks = 5\n", None)]
    #[case::empty("[defaults]\neditor =\n", None)]
    #[case::semicolon_comment("[defaults]\neditor = nano ; the usual one\n", Some("nano"))]
    #[case::hash_comment("[defaults]\neditor = nano # the usual one\n", Some("nano"))]
    #[case::no_space_no_comment("[defaults]\neditor = nano;x\n", Some("nano;x"))]
    #[case::only_a_comment("[defaults]\neditor = # none\n", None)]
    #[case::commented_line("[defaults]\n# editor = nano\n; editor = vim\n", None)]
    #[case::other_section("[privilege_escalation]\neditor = nano\n", None)]
    #[case::section_after(
        "[ssh_connection]\neditor = vim\n[defaults]\neditor = nano\n",
        Some("nano")
    )]
    #[case::no_file_text("", None)]
    fn reads_one_key_of_defaults(#[case] text: &str, #[case] want: Option<&str>) {
        assert_eq!(value_in(text, "editor").as_deref(), want);
    }
}
