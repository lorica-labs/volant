// SPDX-License-Identifier: GPL-3.0-or-later
//! The `community.docker.docker` connection: the agent's link through `docker exec -i`, with the
//! options of community.docker 4.8.5 read from the sources its plugin reads them from.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value};

use super::choose::remote_tmp_of;
use super::defaults::{ConnectionDefaults, ConnectionKeywords};
use super::exec::ExecTarget;
use super::names::{
    DOCKER_EXTRA_ARGS, DOCKER_EXTRA_ENV, DOCKER_HOST, DOCKER_PRIVILEGED, DOCKER_TIMEOUT,
    DOCKER_USER, DOCKER_WORKING_DIR, first_set,
};

/// The plugin's full name, which a short `docker` resolves to.
pub(super) const PLUGIN: &str = "community.docker.docker";

/// What `ansible.cfg` and the environment say for the collections' connection plugins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginSettings {
    pub docker: DockerSettings,
    pub podman: super::podman::PodmanSettings,
    pub chroot: super::chroot::ChrootSettings,
}

/// What `ansible.cfg` and the environment say for the plugin; a host variable speaks over each.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DockerSettings {
    /// `[docker_connection] extra_cli_args`.
    pub extra_cli_args: Option<String>,
    /// `ANSIBLE_DOCKER_PRIVILEGED`, else `[docker_connection] privileged`.
    pub privileged: Option<bool>,
    /// `ANSIBLE_DOCKER_WORKING_DIR`, else `[docker_connection] working_dir`.
    pub working_dir: Option<String>,
    /// `ANSIBLE_DOCKER_TIMEOUT`, else `[docker_connection] timeout`. Unset, the connection
    /// timeout of `-T`, `ANSIBLE_TIMEOUT` and `[defaults] timeout` applies.
    pub timeout: Option<Duration>,
}

/// The options of one host's link, once read.
#[derive(Debug, Default)]
struct Options {
    container: String,
    user: Option<String>,
    extra_args: Option<String>,
    env: Vec<(String, String)>,
    working_dir: Option<String>,
    privileged: bool,
}

/// The exec target of a host whose connection is `community.docker.docker`.
pub(super) fn target(
    host_name: &str,
    vars: &Map<String, Value>,
    defaults: &ConnectionDefaults,
    keywords: &ConnectionKeywords,
) -> anyhow::Result<ExecTarget> {
    let text = |names: &[&str]| first_set(vars, names).and_then(text_of);
    let options = Options {
        container: text(&DOCKER_HOST).unwrap_or_else(|| host_name.to_string()),
        user: text(&DOCKER_USER)
            .or_else(|| keywords.user.clone())
            .or_else(|| defaults.remote_user.clone()),
        extra_args: text(&DOCKER_EXTRA_ARGS)
            .or_else(|| defaults.plugins.docker.extra_cli_args.clone()),
        env: extra_env(first_set(vars, &DOCKER_EXTRA_ENV))
            .map_err(|e| anyhow::anyhow!("host '{host_name}': {e}"))?,
        working_dir: text(&DOCKER_WORKING_DIR)
            .or_else(|| defaults.plugins.docker.working_dir.clone()),
        privileged: first_set(vars, &DOCKER_PRIVILEGED)
            .map(flag)
            .or(defaults.plugins.docker.privileged)
            .unwrap_or(false),
    };
    let timeout = match first_set(vars, &DOCKER_TIMEOUT) {
        Some(value) => Some(seconds(value).ok_or_else(|| {
            anyhow::anyhow!("host '{host_name}': ansible_docker_timeout must be a whole number of seconds, found {value}")
        })?),
        None => defaults.plugins.docker.timeout,
    };
    let found = program(std::env::var_os("PATH").as_deref());
    let docker = found
        .as_ref()
        .map_or_else(|| "docker".to_string(), |path| path.display().to_string());
    Ok(ExecTarget {
        unavailable: found
            .is_none()
            .then(|| "docker command not found in PATH".to_string()),
        copy: Some(copy_into(&docker, &options)),
        user_probe: options
            .user
            .is_none()
            .then(|| inspect_argv(&docker, &options)),
        prefix: exec_argv(&docker, &options),
        name: options.container,
        kind: "DOCKER".into(),
        lines: super::exec::Lines::Exec,
        shell: "/bin/sh".into(),
        user: options.user,
        remote_tmp: remote_tmp_of(host_name, vars, defaults)?,
        connect_timeout: timeout.unwrap_or(defaults.connect_timeout),
    })
}

/// `docker <extra_args> exec [-u USER] [-e K=V]... [-w DIR] [--privileged] -i CONTAINER`, the
/// plugin's own order. The extra arguments are split on single spaces, as the plugin splits
/// them, not as shell words.
fn exec_argv(docker: &str, options: &Options) -> Vec<String> {
    let mut argv = vec![docker.to_string()];
    argv.extend(extra_words(options));
    argv.push("exec".into());
    if let Some(user) = &options.user {
        argv.extend(["-u".into(), user.clone()]);
    }
    for (key, value) in &options.env {
        argv.extend(["-e".into(), format!("{key}={value}")]);
    }
    if let Some(dir) = &options.working_dir {
        argv.extend(["-w".into(), dir.clone()]);
    }
    if options.privileged {
        argv.push("--privileged".into());
    }
    argv.extend(["-i".into(), options.container.clone()]);
    argv
}

/// `docker <extra_args> inspect --format {{.Config.User}} CONTAINER`, which names the user a
/// container runs as when the play names none.
fn inspect_argv(docker: &str, options: &Options) -> Vec<String> {
    let mut argv = vec![docker.to_string()];
    argv.extend(extra_words(options));
    argv.extend([
        "inspect".into(),
        "--format".into(),
        "{{.Config.User}}".into(),
        options.container.clone(),
    ]);
    argv
}

/// `docker cp - CONTAINER:{dir}`, which unpacks a tar stream, and the container's image's
/// architecture through `docker inspect`.
fn copy_into(docker: &str, options: &Options) -> super::CopyInto {
    let command = |words: &[&str]| {
        let mut argv = vec![docker.to_string()];
        argv.extend(extra_words(options));
        argv.extend(words.iter().map(ToString::to_string));
        argv
    };
    super::CopyInto {
        tar_into: command(&["cp", "-", &format!("{}:{{dir}}", options.container)]),
        arch: vec![
            command(&["inspect", "--format", "{{.Image}}", &options.container]),
            command(&["image", "inspect", "--format", "{{.Architecture}}", "{}"]),
        ],
    }
}

fn extra_words(options: &Options) -> Vec<String> {
    match options.extra_args.as_deref() {
        Some(args) if !args.is_empty() => args.split(' ').map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

/// `ansible_docker_extra_env`: a mapping of text to text, refused in the plugin's words.
fn extra_env(value: Option<&Value>) -> Result<Vec<(String, String)>, String> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(key, value)| match value {
                Value::String(text) => Ok((key.clone(), text.clone())),
                other => Err(format!(
                    "Non-string value found for extra_env option. Ambiguous env options must be wrapped in quotes to avoid them being interpreted. Value: {}",
                    python_repr(other)
                )),
            })
            .collect(),
        Some(other) => Err(format!(
            "Config 'extra_env' for 'ansible_collections.community.docker.plugins.connection.docker' connection plugin from 'var: ansible_docker_extra_env' has an invalid value: Invalid value provided for 'dict': {}",
            match other {
                Value::String(text) => format!("'{text}'"),
                other => python_repr(other),
            }
        )),
    }
}

/// How Python prints a scalar the plugin refuses.
fn python_repr(value: &Value) -> String {
    match value {
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => "None".into(),
        other => other.to_string(),
    }
}

/// A boolean setting: a word the reference does not know reads as false.
pub(super) fn flag(value: &Value) -> bool {
    match value {
        Value::Bool(on) => *on,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(text) => crate::yaml::bool_from_str(text.trim()).unwrap_or(false),
        other => crate::template::truthy(other),
    }
}

fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn seconds(value: &Value) -> Option<Duration> {
    let whole = match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }?;
    Some(Duration::from_secs(whole))
}

/// The first `docker` on `path` that can be run.
fn program(path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let runnable = |file: &Path| {
        file.metadata()
            .is_ok_and(|meta| meta.is_file() && executable(&meta))
    };
    std::env::split_paths(path?)
        .map(|dir| dir.join("docker"))
        .find(|file| runnable(file))
}

#[cfg(unix)]
pub(super) fn executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
pub(super) fn executable(_meta: &std::fs::Metadata) -> bool {
    true
}

#[cfg(all(test, unix))]
mod tests;
