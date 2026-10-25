// SPDX-License-Identifier: GPL-3.0-or-later
//! The settings the collections' container connections read besides host variables:
//! `[docker_connection]` and `ANSIBLE_DOCKER_*` for community.docker 4.8.5's
//! `community.docker.docker`, `ANSIBLE_PODMAN_*` for containers.podman 1.18.0's
//! `containers.podman.podman`, whose `[defaults] podman_extra_args` the `[defaults]` reader takes.

use std::time::Duration;

use crate::transport::{DockerSettings, PodmanSettings};

use super::settings::{boolean, integer, timeout_secs};

/// One key of `[docker_connection]`; a key the plugin does not read is left alone.
pub(super) fn ini(
    docker: &mut DockerSettings,
    key: &str,
    value: &str,
    origin: &str,
) -> anyhow::Result<()> {
    match key {
        "extra_cli_args" => docker.extra_cli_args = Some(value.trim().to_string()),
        "privileged" => {
            docker.privileged = Some(boolean("DOCKER_PRIVILEGED", origin, value.trim())?);
        }
        "working_dir" => docker.working_dir = Some(value.trim().to_string()),
        "timeout" => {
            docker.timeout = Some(Duration::from_secs(timeout_secs(
                integer("DOCKER_TIMEOUT", origin, value)?,
                origin,
            )?));
        }
        _ => {}
    }
    Ok(())
}

/// The environment, which wins over the file.
pub(super) fn env(docker: &mut DockerSettings) -> anyhow::Result<()> {
    if let Ok(flag) = std::env::var("ANSIBLE_DOCKER_PRIVILEGED") {
        docker.privileged = Some(boolean(
            "DOCKER_PRIVILEGED",
            "env: ANSIBLE_DOCKER_PRIVILEGED",
            flag.trim(),
        )?);
    }
    if let Ok(dir) = std::env::var("ANSIBLE_DOCKER_WORKING_DIR") {
        docker.working_dir = Some(dir);
    }
    if let Ok(seconds) = std::env::var("ANSIBLE_DOCKER_TIMEOUT") {
        let origin = "env: ANSIBLE_DOCKER_TIMEOUT";
        docker.timeout = Some(Duration::from_secs(timeout_secs(
            integer("DOCKER_TIMEOUT", origin, &seconds)?,
            origin,
        )?));
    }
    Ok(())
}

/// `ANSIBLE_PODMAN_EXECUTABLE` and `ANSIBLE_PODMAN_EXTRA_ARGS`, which win over the file.
pub(super) fn podman_env(podman: &mut PodmanSettings) {
    if let Ok(executable) = std::env::var("ANSIBLE_PODMAN_EXECUTABLE") {
        podman.executable = Some(executable);
    }
    if let Ok(args) = std::env::var("ANSIBLE_PODMAN_EXTRA_ARGS") {
        podman.extra_args = Some(args);
    }
}
