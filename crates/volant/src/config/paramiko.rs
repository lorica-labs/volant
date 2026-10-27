// SPDX-License-Identifier: GPL-3.0-or-later
//! `[paramiko_connection]` and `ANSIBLE_PARAMIKO_*` for ansible-core 2.19.12's
//! `ansible.builtin.paramiko_ssh`, with the generic settings it reads that the rest of the engine
//! does not.

use std::path::PathBuf;
use std::time::Duration;

use crate::transport::ParamikoSettings;

use super::settings::{boolean, integer, timeout_secs};

/// One key of `[paramiko_connection]`. `banner_timeout` is checked and has no use: `ssh` has no
/// such setting.
pub(super) fn ini(
    paramiko: &mut ParamikoSettings,
    key: &str,
    value: &str,
    origin: &str,
) -> anyhow::Result<()> {
    let flag = |name: &str| boolean(name, origin, value.trim()).map(Some);
    match key {
        "remote_user" => paramiko.remote_user = Some(value.trim().to_string()),
        "remote_port" => paramiko.port = Some(port(origin, value)?),
        "private_key_file" => paramiko.private_key_file = Some(PathBuf::from(value.trim())),
        "host_key_checking" => paramiko.host_key_checking = flag("HOST_KEY_CHECKING")?,
        "host_key_auto_add" => paramiko.host_key_auto_add = flag("host_key_auto_add")?,
        "record_host_keys" => paramiko.record_host_keys = flag("record_host_keys")?,
        "look_for_keys" => paramiko.look_for_keys = flag("look_for_keys")?,
        "use_rsa_sha2_algorithms" => {
            paramiko.use_rsa_sha2_algorithms = flag("use_rsa_sha2_algorithms")?;
        }
        "proxy_command" => paramiko.proxy_command = Some(value.trim().to_string()),
        "pty" => paramiko.pty = flag("pty")?,
        "timeout" => paramiko.timeout = Some(seconds(origin, value)?),
        "banner_timeout" => {
            value.trim().parse::<f64>().map_err(|_| {
                anyhow::anyhow!("Invalid value for 'banner_timeout' from '{origin}': {value}")
            })?;
        }
        _ => {}
    }
    Ok(())
}

/// The environment, which wins over the file. Where ansible-core lists more than one variable,
/// the later one wins, so they are read in that order.
pub(super) fn env(paramiko: &mut ParamikoSettings) -> anyhow::Result<()> {
    let read = |name: &str| {
        std::env::var(name)
            .ok()
            .map(|value| (format!("env: {name}"), value))
    };
    if let Some((_, user)) = read("ANSIBLE_PARAMIKO_REMOTE_USER") {
        paramiko.remote_user = Some(user);
    }
    for name in ["ANSIBLE_REMOTE_PORT", "ANSIBLE_REMOTE_PARAMIKO_PORT"] {
        if let Some((origin, value)) = read(name) {
            paramiko.port = Some(port(&origin, &value)?);
        }
    }
    if let Some((_, key)) = read("ANSIBLE_PARAMIKO_PRIVATE_KEY_FILE") {
        paramiko.private_key_file = Some(PathBuf::from(key));
    }
    for name in [
        "ANSIBLE_HOST_KEY_CHECKING",
        "ANSIBLE_SSH_HOST_KEY_CHECKING",
        "ANSIBLE_PARAMIKO_HOST_KEY_CHECKING",
    ] {
        if let Some((origin, value)) = read(name) {
            paramiko.host_key_checking = Some(boolean("HOST_KEY_CHECKING", &origin, value.trim())?);
        }
    }
    for (name, setting) in [
        (
            "ANSIBLE_PARAMIKO_HOST_KEY_AUTO_ADD",
            &mut paramiko.host_key_auto_add,
        ),
        (
            "ANSIBLE_PARAMIKO_RECORD_HOST_KEYS",
            &mut paramiko.record_host_keys,
        ),
        (
            "ANSIBLE_PARAMIKO_LOOK_FOR_KEYS",
            &mut paramiko.look_for_keys,
        ),
        (
            "ANSIBLE_PARAMIKO_USE_RSA_SHA2_ALGORITHMS",
            &mut paramiko.use_rsa_sha2_algorithms,
        ),
        ("ANSIBLE_PARAMIKO_PTY", &mut paramiko.pty),
    ] {
        if let Some((origin, value)) = read(name) {
            *setting = Some(boolean(name, &origin, value.trim())?);
        }
    }
    if let Some((_, command)) = read("ANSIBLE_PARAMIKO_PROXY_COMMAND") {
        paramiko.proxy_command = Some(command);
    }
    for name in ["ANSIBLE_SSH_TIMEOUT", "ANSIBLE_PARAMIKO_TIMEOUT"] {
        if let Some((origin, value)) = read(name) {
            paramiko.timeout = Some(seconds(&origin, &value)?);
        }
    }
    if let Some((origin, value)) = read("ANSIBLE_PARAMIKO_BANNER_TIMEOUT") {
        ini(paramiko, "banner_timeout", &value, &origin)?;
    }
    Ok(())
}

/// A port, a whole number from 0 to 65535.
pub(super) fn port(origin: &str, value: &str) -> anyhow::Result<u16> {
    let n = integer("remote_port", origin, value)?;
    u16::try_from(n)
        .map_err(|_| anyhow::anyhow!("Invalid value for 'remote_port' from '{origin}': {value}"))
}

/// A timeout in whole seconds.
pub(super) fn seconds(origin: &str, value: &str) -> anyhow::Result<Duration> {
    Ok(Duration::from_secs(timeout_secs(
        integer("timeout", origin, value)?,
        origin,
    )?))
}
