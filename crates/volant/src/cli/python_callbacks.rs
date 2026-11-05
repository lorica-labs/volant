// SPDX-License-Identifier: GPL-3.0-or-later
//! Whether a run has Python callback plugins, and their helper when it does. A run that names none
//! and whose callback directories hold none starts no Python.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::args::PlaybookArgs;
use crate::config::{Config, expand_home};
use crate::python::CallbackHelper;
use crate::render::PythonCallbacks;

/// What the run asks of its Python callbacks.
pub(super) struct Wanted<'a> {
    /// The stdout callback, when the renderer does not have it.
    pub stdout: Option<&'a str>,
    /// The names of `callbacks_enabled` the renderer does not run.
    pub enabled: Vec<String>,
    /// Whether the run loads more than its stdout callback.
    pub additional: bool,
    /// The directory the ad hoc command takes for a playbook's.
    pub adhoc_dir: Option<&'a Path>,
}

/// The callback plugin directories ansible-core reads with no setting.
const DEFAULT_DIRS: [&str; 2] = [
    "~/.ansible/plugins/callback",
    "/usr/share/ansible/plugins/callback",
];

fn holds_python(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut files| {
        files.any(|file| {
            file.is_ok_and(|file| file.path().extension().is_some_and(|ext| ext == "py"))
        })
    })
}

/// The directories a playbook's plugins are read from: each playbook's, and `--playbook-dir`.
fn playbook_dirs(args: &PlaybookArgs, adhoc_dir: Option<&Path>) -> Vec<PathBuf> {
    args.playbooks
        .iter()
        .filter_map(|file| Some(std::path::absolute(file).ok()?.parent()?.to_path_buf()))
        .chain(adhoc_dir.and_then(|dir| std::path::absolute(dir).ok()))
        .collect()
}

/// The plugin directories hold a callback that loads without being named.
fn found_on_disk(config: &Config, dirs: &[PathBuf]) -> bool {
    let cwd = std::env::current_dir().unwrap_or_default();
    let configured = if config.callbacks.plugin_dirs.is_empty() {
        DEFAULT_DIRS
            .iter()
            .map(|dir| expand_home(dir, &cwd))
            .collect()
    } else {
        config.callbacks.plugin_dirs.clone()
    };
    dirs.iter()
        .map(|dir| dir.join("callback_plugins"))
        .chain(configured)
        .any(|dir| holds_python(&dir))
}

/// `context.CLIARGS`, as far as a callback reads it.
fn cliargs(args: &PlaybookArgs) -> Value {
    let mut cli = json!({
        "verbosity": args.verbose,
        "check": args.check,
        "diff": args.diff,
        "tags": args.tags,
        "skip_tags": args.skip_tags,
        "args": args.playbooks,
        "subset": args.hosts.limit,
        "forks": args.forks,
    });
    if let Some(connection) = &args.connect.connection {
        cli["connection"] = connection.as_str().into();
    }
    cli
}

/// The run's Python callbacks; `None` when there are none, with nothing started unless a plugin
/// might load.
///
/// # Errors
/// The sentence the helper's load ended with.
pub(super) fn python_callbacks(
    config: &Config,
    args: &PlaybookArgs,
    wanted: &Wanted<'_>,
) -> Result<Option<PythonCallbacks>, String> {
    let dirs = playbook_dirs(args, wanted.adhoc_dir);
    if wanted.stdout.is_none()
        && wanted.enabled.is_empty()
        && !(wanted.additional && found_on_disk(config, &dirs))
    {
        return Ok(None);
    }
    let helper = CallbackHelper::start().map_err(|err| match wanted.stdout {
        Some(name) => format!("Could not load '{name}' callback plugin."),
        None => format!("{err:#}"),
    })?;
    let mut python = PythonCallbacks::new(helper);
    let loaded = python.load(&json!({
        "stdout": wanted.stdout,
        "enabled": wanted.enabled,
        "additional": wanted.additional,
        "dirs": dirs,
        "cliargs": cliargs(args),
    }))?;
    Ok((loaded > 0).then_some(python))
}
