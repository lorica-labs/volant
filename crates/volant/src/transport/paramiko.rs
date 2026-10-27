// SPDX-License-Identifier: GPL-3.0-or-later
//! ansible-core 2.19.12's `paramiko_ssh` connection, deprecated there and removed in 2.21. It runs
//! on the `ssh` transport: each of the plugin's options becomes the closest `ssh` option, and
//! `~/.ssh/config` is left out as paramiko leaves it out.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Map, Value};

use super::choose::{remote_tmp_of, ssh_address};
use super::control::SSH_PROGRAM;
use super::defaults::{
    ConnectionDefaults, ConnectionKeywords, HostOwn, Secret, SshTarget, port_of, timeout_of,
};
use super::docker::flag;
use super::names::{
    PARAMIKO_HOST, PARAMIKO_HOST_KEY_CHECKING, PARAMIKO_PASSWORD, PARAMIKO_PORT,
    PARAMIKO_PRIVATE_KEY, PARAMIKO_PROXY_COMMAND, PARAMIKO_TIMEOUT, PARAMIKO_USE_RSA_SHA2,
    PARAMIKO_USER, first_set, secret_text,
};

/// The names the plugin answers to, as written and as the controller resolves them.
pub(super) const PLUGINS: [&str; 5] = [
    "paramiko",
    "paramiko_ssh",
    "_paramiko_ssh",
    "ansible.builtin.paramiko_ssh",
    "ansible.builtin._paramiko_ssh",
];

/// What `ansible.cfg` and the environment say for the plugin; a host variable speaks over each.
/// Each value is the plugin's own setting, else the environment's generic one where the rest of
/// the engine does not read it already.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParamikoSettings {
    /// `ANSIBLE_PARAMIKO_REMOTE_USER`, else `[paramiko_connection] remote_user`.
    pub remote_user: Option<String>,
    /// `ANSIBLE_REMOTE_PARAMIKO_PORT`, `ANSIBLE_REMOTE_PORT`, else `[paramiko_connection]
    /// remote_port`.
    pub port: Option<u16>,
    /// `[defaults] remote_port`, which the two above beat.
    pub defaults_port: Option<u16>,
    /// `ANSIBLE_PARAMIKO_PRIVATE_KEY_FILE`, else `[paramiko_connection] private_key_file`.
    pub private_key_file: Option<PathBuf>,
    /// `ANSIBLE_PARAMIKO_HOST_KEY_CHECKING`, `ANSIBLE_SSH_HOST_KEY_CHECKING`,
    /// `ANSIBLE_HOST_KEY_CHECKING`, else `[paramiko_connection] host_key_checking`.
    pub host_key_checking: Option<bool>,
    pub host_key_auto_add: Option<bool>,
    pub record_host_keys: Option<bool>,
    pub look_for_keys: Option<bool>,
    pub use_rsa_sha2_algorithms: Option<bool>,
    pub proxy_command: Option<String>,
    pub pty: Option<bool>,
    /// `ANSIBLE_PARAMIKO_TIMEOUT`, `ANSIBLE_SSH_TIMEOUT`, else `[paramiko_connection] timeout`.
    pub timeout: Option<Duration>,
    /// `[ssh_connection] timeout`, which the one above beats.
    pub ssh_timeout: Option<Duration>,
}

/// The `ssh` target of a host whose connection is `paramiko_ssh`.
pub(super) fn target(
    host_name: &str,
    vars: &Map<String, Value>,
    defaults: &ConnectionDefaults,
    keywords: &ConnectionKeywords,
) -> anyhow::Result<SshTarget> {
    let settings = &defaults.plugins.paramiko;
    let text = |names: &[&str]| {
        first_set(vars, names)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let address = PARAMIKO_HOST
        .iter()
        .find(|name| first_set(vars, &[name]).is_some())
        .and_then(|name| text(&[name]).map(|address| (*name, address)));
    let private_key = text(&PARAMIKO_PRIVATE_KEY)
        .map(PathBuf::from)
        .or_else(|| settings.private_key_file.clone())
        .or_else(|| defaults.private_key.clone());
    let host_key_checking = first_set(vars, &PARAMIKO_HOST_KEY_CHECKING)
        .map(flag)
        .or(settings.host_key_checking)
        .unwrap_or(defaults.host_key_checking);
    let options = Options {
        host_key_checking,
        host_key_auto_add: settings.host_key_auto_add.unwrap_or(false),
        record_host_keys: settings.record_host_keys.unwrap_or(true),
        only_the_key: !settings.look_for_keys.unwrap_or(true) && private_key.is_some(),
        rsa_sha2: first_set(vars, &PARAMIKO_USE_RSA_SHA2)
            .map(flag)
            .or(settings.use_rsa_sha2_algorithms)
            .unwrap_or(true),
        proxy_command: text(&PARAMIKO_PROXY_COMMAND)
            .or_else(|| settings.proxy_command.clone())
            .filter(|command| !command.is_empty()),
    };
    Ok(SshTarget {
        address: ssh_address(host_name, address)?,
        port: port_of(vars, &PARAMIKO_PORT)
            .or(keywords.port)
            .or(settings.port)
            .or(settings.defaults_port),
        user: text(&PARAMIKO_USER)
            .or_else(|| keywords.user.clone())
            .or_else(|| settings.remote_user.clone())
            .or_else(|| defaults.remote_user.clone()),
        own: Box::new(HostOwn {
            private_key,
            password: first_set(vars, &PARAMIKO_PASSWORD)
                .map(|value| {
                    secret_text(value).map(Secret::new).ok_or_else(|| {
                        anyhow::anyhow!(
                            "host '{host_name}': the connection password must be text or a number"
                        )
                    })
                })
                .transpose()?,
            executable: SSH_PROGRAM.to_string(),
            paramiko: true,
        }),
        common_args: options.ssh_args(),
        extra_args: Vec::new(),
        host_key_checking,
        connect_timeout: timeout_of(host_name, vars, &PARAMIKO_TIMEOUT)?
            .or(settings.timeout)
            .or(settings.ssh_timeout)
            .unwrap_or(defaults.connect_timeout),
        remote_tmp: remote_tmp_of(host_name, vars, defaults)?,
        control_path: None,
        dumped: None,
        use_tty: settings.pty.unwrap_or(true),
    })
}

/// The plugin's options that become `ssh` options of their own.
struct Options {
    host_key_checking: bool,
    host_key_auto_add: bool,
    record_host_keys: bool,
    /// `look_for_keys` off with a key given: that key and no other.
    only_the_key: bool,
    rsa_sha2: bool,
    proxy_command: Option<String>,
}

impl Options {
    /// `-F /dev/null` first, since paramiko reads no OpenSSH configuration, then one `-o` per
    /// option that differs from what `ssh` does by itself. With host key checking off, the
    /// target's own `StrictHostKeyChecking=no` covers the two host key options.
    fn ssh_args(&self) -> Vec<String> {
        let mut options = Vec::new();
        if self.host_key_checking && self.host_key_auto_add {
            options.push("StrictHostKeyChecking=accept-new".to_string());
        }
        // `ssh` records a host key in the first of these files and checks it against all of
        // them, so the usual files are still read and nothing is written.
        if self.host_key_checking && !self.record_host_keys {
            options.push(
                "UserKnownHostsFile=/dev/null ~/.ssh/known_hosts ~/.ssh/known_hosts2".to_string(),
            );
        }
        if self.only_the_key {
            options.push("IdentitiesOnly=yes".to_string());
        }
        // The option is there for servers that know RSA keys only as `ssh-rsa`. `ssh` dropped
        // that algorithm from its defaults, so it is added back; `ssh` still prefers the SHA-2
        // signatures with a server that offers them.
        if !self.rsa_sha2 {
            options.push("HostKeyAlgorithms=+ssh-rsa".to_string());
            options.push("PubkeyAcceptedAlgorithms=+ssh-rsa".to_string());
        }
        if let Some(command) = &self.proxy_command {
            options.push(format!("ProxyCommand={command}"));
        }
        let mut args = vec!["-F".to_string(), "/dev/null".to_string()];
        for option in options {
            args.extend(["-o".to_string(), option]);
        }
        args
    }
}

/// ansible-core's deprecation of the plugin, once per run.
pub(super) fn warn_deprecated() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        crate::playbook::print_deprecation_line(
            "The paramiko connection plugin is deprecated. This feature will be removed from ansible-core version 2.21.",
        );
    });
}

#[cfg(test)]
mod tests;
