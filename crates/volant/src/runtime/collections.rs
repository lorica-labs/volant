// SPDX-License-Identifier: GPL-3.0-or-later
//! Collections installed into the managed runtime, and where the search finds them.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::install::{Installed, python_command};

/// Where the runtime keeps the collections `volant runtime install --collections` fetched.
pub(crate) fn dir(runtime: &Path) -> PathBuf {
    runtime.join("collections")
}

/// The collections directory of the runtime this release pins, whether or not it exists yet.
pub(crate) fn active_dir() -> Option<PathBuf> {
    let id = super::resolve::pinned().ok()?.id();
    Some(dir(&super::runtimes_dir().ok()?.join(id)))
}

/// `paths` with the runtime's collections directory last, so a collection the user keeps in their
/// own paths shadows the one Volant fetched.
pub(crate) fn after(mut paths: Vec<PathBuf>, runtime_dir: Option<PathBuf>) -> Vec<PathBuf> {
    if let Some(extra) = runtime_dir
        && !paths.contains(&extra)
    {
        paths.push(extra);
    }
    paths
}

/// The `ANSIBLE_COLLECTIONS_PATH` a helper needs to see the runtime's collections, or `None`
/// while none were installed, which leaves the helper to the user's own configuration.
pub(crate) fn helper_env() -> Option<String> {
    let extra = active_dir().filter(|dir| dir.is_dir())?;
    let paths = crate::config::Config::load().ok()?.collections_path;
    let joined = std::env::join_paths(after(paths, Some(extra))).ok()?;
    joined.into_string().ok()
}

/// The arguments of `--collections` as names and requirements files: an argument ending in
/// `.yml` or `.yaml` that `is_file` names is a file, anything else is a collection name.
pub(crate) fn split(
    args: &[String],
    is_file: impl Fn(&Path) -> bool,
) -> (Vec<String>, Vec<PathBuf>) {
    let (mut names, mut files) = (Vec::new(), Vec::new());
    for arg in args {
        let path = Path::new(arg);
        let yaml = path
            .extension()
            .is_some_and(|ext| ext == "yml" || ext == "yaml");
        if yaml && is_file(path) {
            files.push(path.to_path_buf());
        } else {
            names.push(arg.clone());
        }
    }
    (names, files)
}

/// What to give galaxy: collection names, or a requirements file.
pub(crate) enum Source<'a> {
    Names(&'a [String]),
    File(&'a Path),
}

/// Installs collections into the runtime with `python -m ansible.cli.galaxy`, never the
/// `ansible-galaxy` script, whose first line names the directory the runtime was built in.
pub(crate) fn install(runtime: &Installed, source: &Source) -> Result<(), String> {
    let mut command = python_command(&runtime.python);
    command
        .args(["-m", "ansible.cli.galaxy", "collection", "install", "-p"])
        .arg(dir(&runtime.dir))
        .env("ANSIBLE_HOME", runtime.dir.join("ansible-home"));
    match source {
        Source::Names(names) => {
            if names.is_empty() {
                return Err("no collection named".to_owned());
            }
            if let Some(bad) = names.iter().find(|name| name.starts_with('-')) {
                return Err(format!(
                    "'{bad}' is not a collection name: write namespace.name, optionally with a version"
                ));
            }
            command.args(*names);
        }
        Source::File(file) => {
            command.arg("-r").arg(file);
        }
    }
    let status = command
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| format!("cannot run {}: {e}", runtime.python.display()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "ansible-galaxy could not install the collections ({status}): check the names and the \
             network, then run the command again"
        ))
    }
}
