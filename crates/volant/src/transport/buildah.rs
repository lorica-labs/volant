// SPDX-License-Identifier: GPL-3.0-or-later
//! The `containers.podman.buildah` connection: the agent's link through `buildah run`, for a
//! working container made by `buildah from`, with the options of containers.podman 1.18.0.
//!
//! As for the podman plugin, the container and the user are the play context's, read from
//! `ansible_host` and `ansible_user`. The program is the `buildah` on `PATH`; the plugin has no
//! option for it. `buildah mount` is never run: it needs `buildah unshare` for a rootless user,
//! so a container with no shell gets the agent through `buildah copy`.

use std::path::PathBuf;

use serde_json::{Map, Value};

use super::choose::remote_tmp_of;
use super::defaults::{ConnectionDefaults, ConnectionKeywords};
use super::exec::{ExecTarget, Lines};
use super::names::{PODMAN_HOST, PODMAN_USER, first_set};

/// The plugin's full name, which a short `buildah` resolves to.
pub(super) const PLUGIN: &str = "containers.podman.buildah";

/// The exec target of a host whose connection is `containers.podman.buildah`.
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
    // `root` is the working container's own user, which needs no flag.
    let user = text(&PODMAN_USER)
        .or_else(|| keywords.user.clone())
        .or_else(|| defaults.remote_user.clone())
        .filter(|user| user != "root");
    let found = program(std::env::var_os("PATH").as_deref());
    let buildah = found
        .as_ref()
        .map_or_else(|| "buildah".to_string(), |path| path.display().to_string());
    let mut prefix = vec![buildah.clone(), "run".to_string()];
    if let Some(user) = &user {
        prefix.extend(["--user".to_string(), user.clone()]);
    }
    prefix.extend([container.clone(), "--".to_string()]);
    let mut copy = vec![
        buildah.clone(),
        "copy".to_string(),
        "--chmod".to_string(),
        "0755".to_string(),
    ];
    if let Some(user) = &user {
        copy.extend(["--chown".to_string(), user.clone()]);
    }
    copy.extend([container.clone(), "{src}".to_string(), "{dest}".to_string()]);
    Ok(ExecTarget {
        unavailable: found
            .is_none()
            .then(|| "buildah command not found in PATH".to_string()),
        quiet_stderr: true,
        copy: Some(super::CopyInto {
            command: copy,
            feed: super::Feed::LocalFile,
            arch: vec![vec![
                buildah,
                "inspect".to_string(),
                "--format".to_string(),
                "{{.OCIv1.Architecture}}".to_string(),
                container.clone(),
            ]],
        }),
        user_probe: None,
        prefix,
        name: container,
        kind: "BUILDAH".into(),
        lines: Lines::Run,
        shell: "/bin/sh".into(),
        user,
        remote_tmp: remote_tmp_of(host_name, vars, defaults)?,
        connect_timeout: defaults.connect_timeout,
    })
}

/// The first `buildah` on `path` that can run.
fn program(path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .map(|dir| dir.join("buildah"))
        .find(|file| {
            file.metadata()
                .is_ok_and(|meta| meta.is_file() && super::docker::executable(&meta))
        })
}

#[cfg(all(test, unix))]
mod tests;
