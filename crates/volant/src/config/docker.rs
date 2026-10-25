// SPDX-License-Identifier: GPL-3.0-or-later
//! `[docker_connection]` and the `ANSIBLE_DOCKER_*` variables, the settings community.docker
//! 4.8.5's `community.docker.docker` reads besides host variables.

use std::time::Duration;

use crate::transport::DockerSettings;

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
