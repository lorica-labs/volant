// SPDX-License-Identifier: GPL-3.0-or-later
//! The `containers.podman.podman` connection: the agent's link through `podman exec -i`, with the
//! options of containers.podman 1.18.0 read as its plugin reads them.
//!
//! The plugin's own source differs from its documentation, and this follows the source: the
//! container is the play context's address (`ansible_host`, else the inventory name), so the
//! documented `ansible_podman_host` is not read; the extra arguments are split as shell words.
//! The plugin runs `podman exec` with no `-i`, so it cannot hand a command its stdin; the agent's
//! link needs one, so this runs `podman exec -i`, the one word its command lines add.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::choose::remote_tmp_of;
use super::defaults::{ConnectionDefaults, ConnectionKeywords};
use super::exec::{ExecTarget, Lines};
use super::names::{PODMAN_EXECUTABLE, PODMAN_EXTRA_ARGS, PODMAN_HOST, PODMAN_USER, first_set};

/// The plugin's full name, which a short `podman` resolves to.
pub(super) const PLUGIN: &str = "containers.podman.podman";

/// What the environment and `ansible.cfg` say for the plugin; a host variable speaks over each.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PodmanSettings {
    /// `ANSIBLE_PODMAN_EXECUTABLE`.
    pub executable: Option<String>,
    /// `ANSIBLE_PODMAN_EXTRA_ARGS`, else `[defaults] podman_extra_args`.
    pub extra_args: Option<String>,
}

/// The exec target of a host whose connection is `containers.podman.podman`.
pub(super) fn target(
    host_name: &str,
    vars: &Map<String, Value>,
    defaults: &ConnectionDefaults,
    keywords: &ConnectionKeywords,
) -> anyhow::Result<ExecTarget> {
    let text = |names: &[&str]| {
        first_set(vars, names)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let container = text(&PODMAN_HOST).unwrap_or_else(|| host_name.to_string());
    let user = text(&PODMAN_USER)
        .or_else(|| keywords.user.clone())
        .or_else(|| defaults.remote_user.clone());
    let extra = match text(&PODMAN_EXTRA_ARGS)
        .or_else(|| defaults.plugins.podman.extra_args.clone())
    {
        Some(args) => shlex::split(&args).ok_or_else(|| {
            anyhow::anyhow!("host '{host_name}': podman_extra_args has an unbalanced quote: {args}")
        })?,
        None => Vec::new(),
    };
    let executable = text(&PODMAN_EXECUTABLE)
        .or_else(|| defaults.plugins.podman.executable.clone())
        .unwrap_or_else(|| "podman".to_string());
    let found = program(&executable, std::env::var_os("PATH").as_deref());
    let podman = found
        .as_ref()
        .map_or_else(|| executable.clone(), |path| path.display().to_string());
    let command = |words: &[&str]| {
        let mut argv = vec![podman.clone()];
        argv.extend(extra.iter().cloned());
        argv.extend(words.iter().map(ToString::to_string));
        argv
    };
    let mut prefix = command(&["exec"]);
    if let Some(user) = &user {
        prefix.extend(["--user".to_string(), user.clone()]);
    }
    prefix.extend(["-i".to_string(), container.clone()]);
    Ok(ExecTarget {
        unavailable: found
            .is_none()
            .then(|| format!("{executable} command not found in PATH")),
        quiet_stderr: false,
        failures: Vec::new(),
        env: Vec::new(),
        secrets: Vec::new(),
        kept: None,
        info: None,
        copy: Some(super::CopyInto {
            command: command(&["cp", "-", &format!("{container}:{{dir}}")]),
            feed: super::Feed::TarStdin,
            arch: vec![
                command(&["inspect", "--format", "{{.Image}}", &container]),
                command(&["image", "inspect", "--format", "{{.Architecture}}", "{}"]),
            ],
        }),
        user_probe: None,
        prefix,
        name: container,
        kind: "PODMAN".into(),
        lines: Lines::Run,
        shell: "/bin/sh".into(),
        user,
        remote_tmp: remote_tmp_of(host_name, vars, defaults)?,
        connect_timeout: defaults.connect_timeout,
    })
}

/// `executable` itself when it names a path, else the first of that name on `path` that can run.
fn program(executable: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let runnable = |file: &Path| {
        file.metadata()
            .is_ok_and(|meta| meta.is_file() && super::docker::executable(&meta))
    };
    if executable.contains('/') {
        let file = PathBuf::from(executable);
        return runnable(&file).then_some(file);
    }
    std::env::split_paths(path?)
        .map(|dir| dir.join(executable))
        .find(|file| runnable(file))
}

#[cfg(all(test, unix))]
mod tests;
