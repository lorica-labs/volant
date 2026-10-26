// SPDX-License-Identifier: GPL-3.0-or-later
//! The `community.general.lxd` and `community.general.incus` connections: the agent's link
//! through `lxc exec` or `incus exec`, for an LXD or Incus instance, local or on a remote server,
//! with the options of community.general 11.4.2. The two plugins share one backend; Incus always
//! passes a project and words its refusals with the remote and the project.
//!
//! For a user other than root the plugin wraps every command in its become method, `/bin/su
//! <user> -c` by default, and so does this. `su -c` takes one command, so a target reached that
//! way gets the agent through the `stdin` strategy only; the `copy` strategy, whose agent takes
//! arguments, is for root.

use std::path::PathBuf;

use serde_json::{Map, Value};

use super::choose::remote_tmp_of;
use super::defaults::{ConnectionDefaults, ConnectionKeywords};
use super::exec::{ExecTarget, Lines};
use super::names::{CHROOT_EXECUTABLE, INCUS, LXD, PODMAN_USER, first_set};

/// Which of the two plugins.
#[derive(Debug, Clone, Copy)]
pub(super) enum Flavor {
    Lxd,
    Incus,
}

impl Flavor {
    /// The plugin's full name for a resolved connection name.
    pub(super) fn of(plugin: &str) -> Option<Flavor> {
        match plugin {
            "community.general.lxd" => Some(Flavor::Lxd),
            "community.general.incus" => Some(Flavor::Incus),
            _ => None,
        }
    }

    fn program(self) -> &'static str {
        match self {
            Flavor::Lxd => "lxc",
            Flavor::Incus => "incus",
        }
    }

    /// The word in `ESTABLISH <KIND> CONNECTION`, as each plugin capitalises it.
    fn kind(self) -> &'static str {
        match self {
            Flavor::Lxd => "LXD",
            Flavor::Incus => "Incus",
        }
    }

    /// The plugin's own variable for `option`: 0 host, 1 remote, 2 project, 3 executable.
    fn variable(self, option: usize) -> &'static str {
        match self {
            Flavor::Lxd => LXD[option],
            Flavor::Incus => INCUS[option],
        }
    }

    fn become_variable(self) -> &'static str {
        match self {
            Flavor::Lxd => "lxd_become_method",
            Flavor::Incus => "incus_become_method",
        }
    }
}

/// The exec target of a host whose connection is lxd or incus.
pub(super) fn target(
    flavor: Flavor,
    host_name: &str,
    vars: &Map<String, Value>,
    defaults: &ConnectionDefaults,
    keywords: &ConnectionKeywords,
) -> anyhow::Result<ExecTarget> {
    let text = |name: &str| {
        first_set(vars, &[name])
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let address = text(flavor.variable(0))
        .or_else(|| text("ansible_host"))
        .unwrap_or_else(|| host_name.to_string());
    // The instance is the address up to its first dot: `web.example.com` is `web`.
    let instance = address.split('.').next().unwrap_or_default().to_string();
    let remote = text(flavor.variable(1)).unwrap_or_else(|| "local".to_string());
    let project = text(flavor.variable(2)).or(match flavor {
        Flavor::Lxd => None,
        Flavor::Incus => Some("default".to_string()),
    });
    let shell = text(flavor.variable(3))
        .or_else(|| {
            first_set(vars, &CHROOT_EXECUTABLE)
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "/bin/sh".to_string());
    let user = first_set(vars, &PODMAN_USER)
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| keywords.user.clone())
        .or_else(|| defaults.remote_user.clone())
        .unwrap_or_else(|| "root".to_string());
    let method = text(flavor.become_variable()).unwrap_or_else(|| "/bin/su".to_string());
    let found = program(flavor.program(), std::env::var_os("PATH").as_deref());
    let exe = found.as_ref().map_or_else(
        || flavor.program().to_string(),
        |path| path.display().to_string(),
    );
    let command = |words: &[&str]| {
        let mut argv = vec![exe.clone()];
        if let Some(project) = &project {
            argv.extend(["--project".to_string(), project.clone()]);
        }
        argv.extend(words.iter().map(ToString::to_string));
        argv
    };
    let target = format!("{remote}:{instance}");
    let mut prefix = command(&["exec", &target, "--"]);
    let root = user == "root";
    if !root {
        prefix.extend([method.clone(), user.clone(), "-c".to_string()]);
    }
    let failures = failures(flavor, &instance, &remote, project.as_deref());
    Ok(ExecTarget {
        unavailable: found
            .is_none()
            .then(|| format!("{} command not found in PATH", flavor.program())),
        quiet_stderr: false,
        failures,
        env: Vec::new(),
        secrets: Vec::new(),
        kept: None,
        info: (!root).then(|| {
            format!(
                "INFO: Running as non-root user: {user}, trying to run '{} exec' with become method: {method}",
                flavor.program()
            )
        }),
        copy: root.then(|| super::CopyInto {
            command: command(&["file", "push", "--mode", "0755", "{src}", &format!("{target}/{{dest}}")]),
            feed: super::Feed::LocalFile,
            arch: vec![command(&["config", "get", &target, "image.architecture"])],
        }),
        user_probe: None,
        prefix,
        name: instance,
        kind: flavor.kind().into(),
        lines: Lines::Exec,
        shell,
        user: Some(user),
        remote_tmp: remote_tmp_of(host_name, vars, defaults)?,
        connect_timeout: defaults.connect_timeout,
    })
}

/// The plugin's words for what `lxc exec` or `incus exec` refused, keyed by the runtime's own.
fn failures(
    flavor: Flavor,
    instance: &str,
    remote: &str,
    project: Option<&str>,
) -> Vec<(String, String)> {
    let words = |reason: &str| match flavor {
        Flavor::Lxd => format!("{reason}: {instance}"),
        Flavor::Incus => format!(
            "{reason}: {instance} (remote={remote}, project={})",
            project.unwrap_or("default")
        ),
    };
    let keyed = |runtime: &str, reason: &str| (runtime.to_string(), words(reason));
    match flavor {
        Flavor::Lxd => vec![
            keyed("is not running", "instance not running"),
            keyed("Instance not found", "instance not found"),
            keyed("error: not found", "instance not found"),
        ],
        Flavor::Incus => vec![
            keyed("Instance is not running", "instance not running"),
            keyed("Instance not found", "instance not found"),
            keyed("User does not have permission ", "instance access denied"),
            keyed("User does not have entitlement ", "instance access denied"),
        ],
    }
}

/// The first `name` on `path` that can run.
pub(super) fn program(name: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .map(|dir| dir.join(name))
        .find(|file| {
            file.metadata()
                .is_ok_and(|meta| meta.is_file() && super::docker::executable(&meta))
        })
}

#[cfg(all(test, unix))]
mod tests;
