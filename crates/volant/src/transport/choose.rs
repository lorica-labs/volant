// SPDX-License-Identifier: GPL-3.0-or-later
//! Choosing the transport a host or a task uses, and opening its link.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::bail;
use serde_json::{Map, Value};
use tokio::process::Command;

use crate::agent::{AgentLink, AgentSource};
use crate::inventory::Host;
use crate::vars::host_setting;

use super::control::{SSH_PROGRAM, control_path_for, user_sets_control};
use super::defaults::{
    BecomeMethod, ConnectError, ConnectionDefaults, ConnectionFailsTask, ConnectionKeywords,
    Escalation, HostOwn, KNOWN_CONNECTIONS, PythonTarget, Secret, SshTarget, Transport, as_map,
    port_of, setting_args, timeout_of,
};
use super::escalation::check_local_escalation;
use super::escalation::{local_argv, local_su, preamble};
use super::names::{self, first_set, secret_text};
use super::resolve::{ConnectionNames, Resolution};
use super::shell::validate_remote_tmp;

/// The destination `ssh` gets for a host: `ansible_ssh_host` or `ansible_host`, given with the
/// name that set it, or the inventory name when neither is set. Both can come from data the
/// playbook reads, and the address sits in the argument list where `ssh` reads options, so a
/// value that starts with `-` would be taken as one and the remote command as the host. A value with whitespace or a control character is no address
/// either. ansible-core 2.19.12 passes the value on unchecked, so this is stricter than the
/// reference on purpose. `user@host` and IPv6 literals pass.
pub(super) fn ssh_address(
    host_name: &str,
    ansible_host: Option<(&str, String)>,
) -> anyhow::Result<String> {
    let (variable, address) = match ansible_host {
        Some(set) => set,
        None => ("inventory_hostname", host_name.to_string()),
    };
    if address.starts_with('-') || address.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!(
            "host '{host_name}': {variable} must be a host name or address, found '{}'",
            address.escape_debug()
        );
    }
    Ok(address)
}

/// The host's `ansible_remote_tmp`, or the configured one. Blank reads as unset, which is what
/// the configuration arms do with the same value: an empty `remote_tmp` puts the agent cache at
/// `/volant-agent-<version>`, the upload fails on permissions, and the host is reported
/// unreachable over a directory nobody wrote. Trimmed for the same reason they trim. The default
/// already went through the same check when the configuration that produced it was loaded; a
/// host variable is the one source that reaches here unchecked.
pub(super) fn remote_tmp_of(
    host_name: &str,
    vars: &Map<String, Value>,
    defaults: &ConnectionDefaults,
) -> anyhow::Result<String> {
    match host_setting(vars, "ansible_remote_tmp")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|tmp| !tmp.is_empty())
    {
        Some(tmp) => {
            validate_remote_tmp(tmp).map_err(|e| anyhow::anyhow!("host '{host_name}': {e}"))?;
            Ok(tmp.to_string())
        }
        None => Ok(defaults.remote_tmp.clone()),
    }
}

/// The plugin a connection name stands for: its full name when the controller's ansible-core
/// was asked, else the name as written. `local` and `ssh` are ansible-core's own and never asked.
/// A name the controller cannot be asked about, because the run has no ansible-core to ask, is
/// answered as before resolution existed: ansible-core's own plugins this release does not drive,
/// and any dotted name, are kept so they are refused by name, and any other name is no plugin at
/// all, which the reference fails the task for.
///
/// The flag says whether the controller found the plugin, which it can then run in the helper.
fn plugin_of(
    host_name: &str,
    name: &str,
    names: &ConnectionNames,
) -> anyhow::Result<(String, bool)> {
    if matches!(name, "local" | "ssh") {
        return Ok((name.to_string(), false));
    }
    match names.resolve(host_name, name).unwrap_or(None) {
        Some(Resolution::Found { fqcn, .. }) => Ok((fqcn, true)),
        Some(Resolution::Unusable(why)) => {
            bail!("host '{host_name}': connection '{name}' cannot be used: {why}")
        }
        None if KNOWN_CONNECTIONS.contains(&name) || name.contains('.') => {
            Ok((name.to_string(), false))
        }
        Some(Resolution::Missing { warning }) => {
            Err(ConnectionFailsTask::not_found(name, warning).into())
        }
        None => Err(ConnectionFailsTask::not_found(name, None).into()),
    }
}

impl Transport {
    /// The connection one host uses, read from its effective variables: the merged view under
    /// the full precedence rather than the inventory object. `ansible_connection` and its
    /// neighbours are ordinary variables, so an extra var, a `group_vars` file loaded beside the
    /// playbook, a task's own `vars:` and a `set_fact` all reach here, and they are read again
    /// for every task - measured on ansible-core 2.19.12, a `set_fact` of `ansible_host` in the
    /// middle of a play is the address the next task connects to.
    pub fn for_vars(
        host_name: &str,
        vars: &Map<String, Value>,
        defaults: &ConnectionDefaults,
    ) -> anyhow::Result<Transport> {
        Self::for_task(host_name, vars, defaults, &ConnectionKeywords::default())
    }

    /// [`Transport::for_vars`] for a task that carries connection keywords: a host variable
    /// beats the task's keyword, which beats the command line and `ansible.cfg`.
    pub fn for_task(
        host_name: &str,
        vars: &Map<String, Value>,
        defaults: &ConnectionDefaults,
        keywords: &ConnectionKeywords,
    ) -> anyhow::Result<Transport> {
        let setting_text = |key: &str| {
            host_setting(vars, key)
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        // The first of a setting's names the host sets, see `names`.
        let first_text = |names: &[&str]| {
            first_set(vars, names)
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        // The same, with the name that set it, for a message that has to name it.
        let first_named = |names: &[&'static str]| {
            names
                .iter()
                .find(|name| first_set(vars, &[name]).is_some())
                .and_then(|name| setting_text(name).map(|text| (*name, text)))
        };
        let connection = setting_text("ansible_connection")
            .or_else(|| keywords.connection.clone())
            .or_else(|| defaults.connection.clone());
        let name = connection.as_deref().unwrap_or("ssh");
        let (plugin, loadable) = plugin_of(host_name, name, &defaults.names)?;
        match plugin.as_str() {
            "local" | "ansible.builtin.local" => Ok(Transport::Local),
            "ssh" | "ansible.builtin.ssh" => Ok(Transport::Ssh(SshTarget {
                address: ssh_address(host_name, first_named(&names::HOST))?,
                port: port_of(vars, &names::PORT).or(keywords.port),
                user: first_text(&names::USER)
                    .or_else(|| keywords.user.clone())
                    .or_else(|| defaults.remote_user.clone()),
                own: Box::new(HostOwn {
                    private_key: first_text(&names::PRIVATE_KEY)
                        .map(PathBuf::from)
                        .or_else(|| defaults.private_key.clone()),
                    password: first_set(vars, &names::PASSWORD)
                        .map(|value| {
                            secret_text(value).map(Secret::new).ok_or_else(|| {
                                anyhow::anyhow!(
                                    "host '{host_name}': the connection password must be text or a number"
                                )
                            })
                        })
                        .transpose()?,
                    executable: first_text(&names::EXECUTABLE)
                        .unwrap_or_else(|| SSH_PROGRAM.to_string()),
                    paramiko: false,
                }),
                common_args: setting_args(
                    host_name,
                    vars,
                    "ansible_ssh_common_args",
                    &defaults.ssh_common_args,
                )?,
                extra_args: setting_args(
                    host_name,
                    vars,
                    "ansible_ssh_extra_args",
                    &defaults.ssh_extra_args,
                )?,
                host_key_checking: defaults.host_key_checking,
                connect_timeout: timeout_of(host_name, vars, &names::TIMEOUT)?.unwrap_or(defaults.connect_timeout),
                remote_tmp: remote_tmp_of(host_name, vars, defaults)?,
                control_path: None,
                dumped: None,
                use_tty: host_setting(vars, "ansible_ssh_use_tty")
                    .map_or(defaults.ssh_use_tty, super::docker::flag),
            })),
            super::docker::PLUGIN => Ok(Transport::Exec(super::docker::target(
                host_name, vars, defaults, keywords,
            )?)),
            super::podman::PLUGIN => Ok(Transport::Exec(super::podman::target(
                host_name, vars, defaults, keywords,
            )?)),
            super::buildah::PLUGIN => Ok(Transport::Exec(super::buildah::target(
                host_name, vars, defaults, keywords,
            )?)),
            super::chroot::PLUGIN => Ok(Transport::Exec(super::chroot::target(
                host_name, vars, defaults,
            )?)),
            super::kubectl::PLUGIN => Ok(Transport::Exec(super::kubectl::target(
                host_name, vars, defaults,
            )?)),
            plugin if super::paramiko::PLUGINS.contains(&plugin) => Ok(Transport::Ssh(
                super::paramiko::target(host_name, vars, defaults, keywords)?,
            )),
            super::ssm::PLUGIN => Ok(Transport::Ssm(Box::new(super::ssm::target(
                host_name, vars, defaults,
            )?))),
            other => match super::lxd::Flavor::of(other) {
                Some(flavor) => Ok(Transport::Exec(super::lxd::target(
                    flavor, host_name, vars, defaults, keywords,
                )?)),
                // A plugin the controller's ansible-core loads runs there, in the helper.
                None if loadable => Ok(Transport::Python(PythonTarget {
                    plugin: other.to_string(),
                })),
                None => bail!("host '{host_name}': connection '{other}' is not supported"),
            },
        }
    }

    /// Whether a host's connection is `local`, read before the run. Nothing is asked of the
    /// controller's ansible-core: a name only it can resolve is no `local`, and asking would print
    /// its redirect lines before the play that uses it.
    pub fn is_local_before_run(
        host_name: &str,
        vars: &Map<String, Value>,
        defaults: &ConnectionDefaults,
    ) -> bool {
        let unasked = ConnectionDefaults {
            names: ConnectionNames::default(),
            ..defaults.clone()
        };
        matches!(
            Self::for_vars(host_name, vars, &unasked),
            Ok(Transport::Local)
        )
    }

    /// The same, from the inventory object's own variables alone. Nothing on the run's own path
    /// calls it: the executor passes the host's effective map and the pre-flight in `cli.rs`
    /// builds its own, both through `for_vars`. What is left is the shorthand the transport's
    /// own tests are written against, and that is the point of keeping it - those tests read an
    /// inventory `Host` and were not touched when the resolution moved onto the effective view,
    /// so they are the evidence that the move changed no rule.
    pub fn for_host(host: &Host, defaults: &ConnectionDefaults) -> anyhow::Result<Transport> {
        Self::for_vars(&host.name, &as_map(&host.vars), defaults)
    }

    /// This transport with its `ssh` runs sharing one connection per inventory host, the
    /// socket under `dir` (from [`super::control_dir`]; `None` shares nothing). `host` is the host
    /// whose driver holds the link, and `delegate` the host it reaches for a delegated task:
    /// a delegated link is kept by its driver for the rest of the play, so twenty hosts
    /// delegating to one would put twenty sessions on one master, past sshd's `MaxSessions`
    /// of 10, if they shared the delegate's own. Options the operator wrote win: a second
    /// `ControlPath` would only fight theirs.
    pub fn shared(self, dir: Option<&Path>, host: &str, delegate: Option<&str>) -> Transport {
        match self {
            Transport::Ssh(mut target) => {
                target.control_path = None;
                if !user_sets_control(&target.common_args, &target.extra_args) {
                    target.control_path =
                        dir.and_then(|dir| control_path_for(dir, host, delegate, &target));
                }
                Transport::Ssh(target)
            }
            local => local,
        }
    }

    /// The user the link logs in or runs as, when one is set.
    pub fn user(&self) -> Option<&str> {
        match self {
            Transport::Local | Transport::Ssm(_) | Transport::Python(_) => None,
            Transport::Ssh(target) => target.user.as_deref(),
            Transport::Exec(target) => target.user.as_deref(),
        }
    }

    /// Whether this transport's `ssh` runs share a connection, through [`Transport::shared`].
    pub fn is_shared(&self) -> bool {
        matches!(self, Transport::Ssh(target) if target.control_path.is_some())
    }

    /// Makes the master of this host's shared connection exit, if there is one, so the next
    /// `ssh` opens a fresh connection instead of riding one the host may have dropped without a
    /// word. Used before reconnecting to a host that went away.
    pub async fn stop_shared(&self) {
        if let Transport::Ssh(target) = self {
            target.stop_master().await;
        }
    }

    pub async fn connect(
        &self,
        agents: &AgentSource,
        escalation: Option<&Escalation>,
    ) -> Result<AgentLink, ConnectError> {
        match self {
            Transport::Local => {
                let agent = agents
                    .local()
                    .map_err(|e| ConnectError::Unreachable(format!("{e:#}")))?;
                let agent = agent.display().to_string();
                let escalated = match escalation {
                    // `su` reads its password from a terminal, so it gets one of its own.
                    Some(e) if e.method == BecomeMethod::Su => {
                        return local_su(&agent, e).await;
                    }
                    Some(e) => Some((e, check_local_escalation(&agent, e).await?)),
                    None => None,
                };
                let argv = local_argv(&agent, escalated);
                let mut command = Command::new(&argv[0]);
                super::control::with_started_open_file_limit(&mut command);
                let child = command
                    .args(&argv[1..])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| {
                        ConnectError::Unreachable(format!("starting agent {agent}: {e}"))
                    })?;
                AgentLink::new_with_preamble(child, preamble(escalated))
                    .await
                    .map_err(|e| ConnectError::Unreachable(format!("{e:#}")))
            }
            Transport::Ssh(target) => target.connect(agents, escalation).await,
            Transport::Exec(target) => target.connect(agents, escalation).await,
            Transport::Ssm(target) => target.connect(agents, escalation).await,
            // The driver sends this host's tasks to the helper and never connects it.
            Transport::Python(target) => Err(ConnectError::Unreachable(format!(
                "no agent runs behind the connection '{}': its tasks run in the Python helper",
                target.plugin
            ))),
        }
    }
}
