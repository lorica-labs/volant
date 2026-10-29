// SPDX-License-Identifier: GPL-3.0-or-later
//! The files `{% include %}`, `{% import %}` and `{% from ... import %}` load, from the search
//! path of the render that asks: the `template` action's and `lookup('template')`'s own lists,
//! or the playbook directory for any other render, as ansible-core 2.19.12 looks them up.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use minijinja::{Environment, Error, ErrorKind, TemplateSource};

thread_local! {
    /// The search path of the file render running on this thread, when it set one.
    static SEARCH: RefCell<Option<Vec<PathBuf>>> = const { RefCell::new(None) };
    /// The refusal of the last name the loader did not find. minijinja words a missing
    /// `{% include %}` its own way, so the render takes the reference's words back from here.
    static MISSED: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// The reference's words for the last name the loader did not find on this thread.
pub(super) fn take_missed() -> Option<String> {
    MISSED.take()
}

/// Runs `f` with `paths` as the search path of the includes it renders.
pub(super) fn with_search_path<T>(paths: Vec<PathBuf>, f: impl FnOnce() -> T) -> T {
    let outer = SEARCH.replace(Some(paths));
    let out = f();
    SEARCH.set(outer);
    out
}

/// The search path of a file render: `<p>/templates`, then `<p>`, for each entry, as both the
/// `template` action and the lookup widen theirs.
#[must_use]
pub(crate) fn widened(entries: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    entries
        .into_iter()
        .flat_map(|p| [p.join("templates"), p])
        .collect()
}

/// Loads included files for `env`. Outside a file render the search path is `base_dir`, the
/// playbook directory.
pub(super) fn install(env: &mut Environment<'static>, base_dir: PathBuf) {
    env.set_loader(move |name| {
        let paths = SEARCH
            .with_borrow(Clone::clone)
            .unwrap_or_else(|| vec![base_dir.clone()]);
        let Some(found) = find(&paths, name) else {
            return Err(not_found(name, &paths));
        };
        let source = std::fs::read_to_string(&found).map_err(|e| {
            Error::new(
                ErrorKind::InvalidOperation,
                format!("{}: {e}", found.display()),
            )
        })?;
        // minijinja keeps a loaded file by name: it is fresh while the name still finds the same
        // file, unchanged, under the search path of the render that asks again.
        let name = name.to_owned();
        let modified = mtime(&found);
        let base_dir = base_dir.clone();
        let fresh = move || {
            let paths = SEARCH.with_borrow(Clone::clone);
            let paths = paths.as_deref().unwrap_or(std::slice::from_ref(&base_dir));
            find(paths, &name).is_some_and(|again| again == found && mtime(&again) == modified)
        };
        Ok(Some(TemplateSource::new(source).with_uptodate_check(fresh)))
    });
}

fn find(paths: &[PathBuf], name: &str) -> Option<PathBuf> {
    paths.iter().map(|p| p.join(name)).find(|p| p.is_file())
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// ansible-core's refusal, `'name' not found in search path: '/dir'`, with `paths` when there
/// are several. A not-found kind, so `ignore missing` and a list of names go on.
fn not_found(name: &str, paths: &[PathBuf]) -> Error {
    let quoted: Vec<String> = paths.iter().map(|p| format!("'{}'", p.display())).collect();
    let plural = if paths.len() == 1 { "" } else { "s" };
    let said = format!(
        "'{name}' not found in search path{plural}: {}",
        quoted.join(", ")
    );
    MISSED.set(Some(said.clone()));
    Error::new(ErrorKind::TemplateNotFound, said)
}

/// The template files the missing includes were asked from, by the refusal's words: a file
/// `lookup('template')` renders is a link of its own in the error chain.
static ASKED_IN: std::sync::Mutex<Vec<(String, PathBuf)>> = std::sync::Mutex::new(Vec::new());

/// Notes that the refusal `text` came from an include of `file`.
pub(crate) fn note_asked_in(text: &str, file: &Path) {
    let mut asked = ASKED_IN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !asked.iter().any(|(t, _)| t == text) {
        asked.push((text.to_owned(), file.to_path_buf()));
    }
}

/// The template file the refusal `text` came from, when a lookup rendered it.
pub(crate) fn asked_in(text: &str) -> Option<PathBuf> {
    let asked = ASKED_IN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    asked
        .iter()
        .find(|(t, _)| t == text)
        .map(|(_, f)| f.clone())
}

/// Whether `text` is the refusal of an include the loader did not find.
pub(crate) fn is_not_found(text: &str) -> bool {
    text.starts_with("Error rendering template: '") && text.contains("' not found in search path")
}
