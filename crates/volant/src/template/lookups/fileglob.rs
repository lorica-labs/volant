// SPDX-License-Identifier: GPL-3.0-or-later
//! `lookup('fileglob')` and the shell-style pattern matching under it.

use std::path::{Path, PathBuf};

use minijinja::value::Kwargs;
use minijinja::{Error, State, Value};

use super::first_found::found_path;
use super::{search_path, text_of};

/// `lookup('fileglob', pattern...)`, `plugins/lookup/fileglob.py`, read on the dev machine
/// (ansible-core 2.19.12): a pattern with no directory of its own is globbed under `files/` of
/// each entry of `ansible_search_path`, then under the entry itself, the first entry with a
/// match winning; a pattern that already names a directory - an absolute one, measured on the
/// two static `with_fileglob` patterns of the `airgap` role, or a relative one under a search
/// entry - is globbed at that directory directly. Only files match, in the order the directory
/// lists them (`glob.glob`, no sort, no dedupe), and several terms concatenate. The result then
/// goes through the same shaping every other plugin's does ([`super::shaped`]).
///
/// The result is controller content, not tainted: a matched name is a path the pattern's author
/// named and the controller's filesystem confirmed, the same standing `lookup('first_found')`
/// gives the path it finds. Nothing here reads a managed host.
pub(super) fn fileglob(
    state: &State,
    terms: &[Value],
    kwargs: Kwargs,
    base_dir: &Path,
) -> Result<Vec<Value>, Error> {
    kwargs.assert_all_used()?;
    let search = search_path(state, base_dir);
    let mut out: Vec<String> = Vec::new();
    for term in terms {
        let pattern = text_of(term);
        for dir in fileglob_dirs(&pattern, &search) {
            let matched = glob_files(&dir, fileglob_file(&pattern))
                .into_iter()
                .map(found_path)
                .collect::<Result<Vec<_>, _>>()?;
            if !matched.is_empty() {
                out.extend(matched);
                break;
            }
        }
    }
    Ok(out.into_iter().map(Value::from).collect())
}

/// The glob part of a pattern: everything after its last `/`, or the whole pattern when it names
/// no directory.
pub(super) fn fileglob_file(pattern: &str) -> &str {
    pattern.rsplit('/').next().unwrap_or(pattern)
}

/// The directories a pattern is globbed in, in the order they are tried. A pattern with no `/`
/// is the reference's bare-name case: `files/` of each search entry, then the entry itself. A
/// pattern that names its own directory is globbed there directly - the entry itself when that
/// directory is absolute (the `airgap` role's own patterns, `{{ airgap_dir }}/*.tar.gz` rendered
/// to an absolute path), each search entry's own copy of it otherwise.
pub(super) fn fileglob_dirs(pattern: &str, search: &[PathBuf]) -> Vec<PathBuf> {
    match pattern.rsplit_once('/') {
        None => search
            .iter()
            .flat_map(|dir| [dir.join("files"), dir.clone()])
            .collect(),
        Some(("", _)) => vec![PathBuf::from("/")],
        Some((dir, _)) if Path::new(dir).is_absolute() => vec![PathBuf::from(dir)],
        Some((dir, _)) => search
            .iter()
            .flat_map(|entry| [entry.join("files").join(dir), entry.join(dir)])
            .collect(),
    }
}

/// One directory read non-recursively for the files (not the directories) whose name matches a
/// shell glob pattern (`*`, `?`, `[...]`, `[!...]`), Python's `fnmatch` set. A directory that
/// does not exist, or a pattern that fails to compile, matches nothing rather than erroring: the
/// reference's own `glob.glob` answers the same way for a directory that is not there. A name
/// starting with `.` matches only a pattern starting with `.`, as `glob.glob` without
/// `include_hidden` does.
pub(super) fn glob_files(dir: &Path, pattern: &str) -> Vec<String> {
    let Ok(re) = regex::Regex::new(&fnmatch_to_regex(pattern)) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let include_hidden = pattern.starts_with('.');
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| (include_hidden || !name.starts_with('.')) && re.is_match(name))
        })
        .map(|entry| entry.path().display().to_string())
        .collect()
}

/// A shell glob pattern (`*`, `?`, a `[...]` or negated `[!...]` character class, everything else
/// literal) as an anchored regular expression.
pub(super) fn fnmatch_to_regex(pattern: &str) -> String {
    let mut re = String::from("(?s)^");
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            '[' => {
                re.push('[');
                if chars.peek() == Some(&'!') {
                    re.push('^');
                    chars.next();
                }
                for c in chars.by_ref() {
                    if c == ']' {
                        break;
                    }
                    if "\\^]".contains(c) {
                        re.push('\\');
                    }
                    re.push(c);
                }
                re.push(']');
            }
            other => re.push_str(&regex::escape(&other.to_string())),
        }
    }
    re.push('$');
    re
}
