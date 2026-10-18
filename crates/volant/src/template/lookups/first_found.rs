// SPDX-License-Identifier: GPL-3.0-or-later
//! `lookup('first_found')`: the first of a list of files that exists, with Ansible's search rules.

use std::path::{Path, PathBuf};

use minijinja::value::{Kwargs, ValueKind};
use minijinja::{Error, State, Value};

use super::{invalid, search_path, text_of};

/// `lookup('first_found', ...)`, `plugins/lookup/first_found.py`: every term, a name, a list of
/// names or a `{files, paths}` mapping, becomes a list of candidates, and each candidate is
/// tried against each entry of `ansible_search_path` as it is written. Measured on ansible-core
/// 2.19.12 in a role holding `files/second.txt`: `second.txt` is not found and
/// `files/second.txt` is, so nothing adds `files/` or `templates/` of its own accord.
pub(super) fn first_found(
    state: &State,
    terms: &[Value],
    kwargs: Kwargs,
    base_dir: &Path,
) -> Result<Vec<Value>, Error> {
    let mut options = Options {
        files: kwargs.get::<Option<Value>>("files")?.unwrap_or_default(),
        paths: kwargs.get::<Option<Value>>("paths")?.unwrap_or_default(),
        skip: kwargs.get::<Option<bool>>("skip")?.unwrap_or(false),
    };
    kwargs.assert_all_used()?;
    let mut candidates = Vec::new();
    if terms.is_empty() {
        let files = split_on(&options.files, &[',', ';']);
        push_candidates(files, &options.paths, &mut candidates);
    }
    for term in terms {
        candidates_of(term, &mut candidates, &mut options)?;
    }
    let search = search_path(state, base_dir);
    let subdir = state
        .lookup(WITH_SUBDIR)
        .and_then(|v| v.as_str().map(str::to_string));
    for name in &candidates {
        if let Some(found) = relative_stack(&search, subdir.as_deref(), name)
            .into_iter()
            .find(|p| p.exists())
        {
            return Ok(vec![Value::from(found_path(found.display().to_string())?)]);
        }
    }
    if options.skip {
        return Ok(Vec::new());
    }
    Err(invalid("No file was found when using first_found."))
}

/// The directory `with_first_found` searches in each entry before the entry itself, which the
/// task executor sets when it runs the lookup for a `with_first_found` loop and nothing else
/// does: a plain `lookup('first_found')` searches no subdirectory. Two colons cannot appear in a
/// Jinja identifier, so no template can read or write it. `executor/prepare.rs` writes the same
/// literal.
pub(super) const WITH_SUBDIR: &str = "volant::first_found_subdir";

/// `DataLoader.path_dwim_relative_stack` for one name, as ansible-core 2.19.12's `first_found`
/// calls it (`first_found.py`, `find_file_in_search_path(variables, subdir, fn)`): each search
/// entry's `<subdir>/<name>`, unless the name already starts with that directory, before the
/// entry's own `<name>`. An absolute name is itself whatever it is joined to.
pub(super) fn relative_stack(search: &[PathBuf], subdir: Option<&str>, name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in search {
        if let Some(sub) = subdir
            && name.split('/').next() != Some(sub)
        {
            out.push(dir.join(sub).join(name));
        }
        out.push(dir.join(name));
    }
    out
}

/// A path `fileglob` or `first_found` found on the controller, refused when its name holds a
/// template marker. The path is bound as the playbook's own content when its terms were, and an
/// author's string is rendered again while it still looks like a template: a file named
/// `{{ lookup('pipe', ...) }}.tar.gz` under a globbed directory would run its command here. The
/// reference never templates a lookup's result again and copies such a file as it is; this
/// release fails the task naming it instead, because binding the path as data would make the
/// plugin that reads it refuse it as a path a host chose.
pub(super) fn found_path(path: String) -> Result<String, Error> {
    if ["{{", "{%", "{#"]
        .iter()
        .any(|marker| path.contains(marker))
    {
        return Err(invalid(format!(
            "the lookup found '{path}', whose name holds a template marker; rename the file"
        )));
    }
    Ok(path)
}

/// The plugin's options as `first_found.py` holds them: set from the keywords, and replaced
/// whole by `set_options(direct=term)` for each mapping term, so a mapping without `skip` resets
/// it to false and its `paths` are the ones later string terms use.
pub(super) struct Options {
    files: Value,
    paths: Value,
    skip: bool,
}

/// `_process_terms`: a string is a list of names split on `,` and `;`, joined to the options'
/// `paths`; a mapping sets the options and its `files` joined to its `paths`; a list is each of
/// its terms.
pub(super) fn candidates_of(
    term: &Value,
    out: &mut Vec<String>,
    options: &mut Options,
) -> Result<(), Error> {
    match term.kind() {
        ValueKind::String => push_candidates(split_on(term, &[',', ';']), &options.paths, out),
        ValueKind::Seq => {
            for t in term.try_iter()? {
                candidates_of(&t, out, options)?;
            }
        }
        ValueKind::Map => {
            *options = Options {
                files: term.get_attr("files")?,
                paths: term.get_attr("paths")?,
                skip: term.get_attr("skip")?.is_true(),
            };
            let files = split_on(&options.files, &[',', ';']);
            push_candidates(files, &options.paths, out);
        }
        _ => {
            return Err(invalid(format!(
                "Invalid term supplied. A string, dict or list is required, not '{}'.",
                crate::template::type_name(&serde_json::to_value(term).unwrap_or_default())
            )));
        }
    }
    Ok(())
}

/// Each file under each path (split on `,`, `:` and `;`), or the files alone without a path.
pub(super) fn push_candidates(files: Vec<String>, paths: &Value, out: &mut Vec<String>) {
    let paths = split_on(paths, &[',', ':', ';']);
    if paths.is_empty() {
        out.extend(files);
        return;
    }
    for path in &paths {
        for file in &files {
            out.push(Path::new(path).join(file).display().to_string());
        }
    }
}

/// `_split_on`: a string split on each of `seps`, empty pieces kept; a list, each of its items.
pub(super) fn split_on(value: &Value, seps: &[char]) -> Vec<String> {
    match value.kind() {
        ValueKind::String => text_of(value).split(seps).map(str::to_string).collect(),
        ValueKind::Seq => value
            .try_iter()
            .map(|items| items.flat_map(|v| split_on(&v, seps)).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}
