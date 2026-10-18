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

use super::control::{control_path_for, user_sets_control};
use super::defaults::{
    BecomeMethod, ConnectError, ConnectionDefaults, ConnectionKeywords, ConnectionNotFound,
    Escalation, KNOWN_CONNECTIONS, SshTarget, Transport, as_map, port_of, setting_args,
};
use super::escalation::check_local_escalation;
use super::escalation::{local_argv, preamble};
use super::shell::validate_remote_tmp;

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
        let connection = setting_text("ansible_connection")
            .or_else(|| keywords.connection.clone())
            .or_else(|| defaults.connection.clone());
        match connection.as_deref().unwrap_or("ssh") {
            "local" => Ok(Transport::Local),
            "ssh" => Ok(Transport::Ssh(SshTarget {
                address: setting_text("ansible_host").unwrap_or_else(|| host_name.to_string()),
                port: port_of(vars).or(keywords.port),
                user: setting_text("ansible_user")
                    .or_else(|| keywords.user.clone())
                    .or_else(|| defaults.remote_user.clone()),
                private_key: setting_text("ansible_ssh_private_key_file")
                    .map(PathBuf::from)
                    .or_else(|| defaults.private_key.clone()),
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
                connect_timeout: defaults.connect_timeout,
                // Blank reads as unset, which is what the configuration arms do with the same
                // value: an empty `remote_tmp` puts the agent cache at `/volant-agent-<version>`,
                // the upload fails on permissions, and the host is reported unreachable over a
                // directory nobody wrote. Trimmed for the same reason they trim.
                remote_tmp: match setting_text("ansible_remote_tmp")
                    .map(|tmp| tmp.trim().to_string())
                    .filter(|tmp| !tmp.is_empty())
                {
                    Some(tmp) => {
                        validate_remote_tmp(&tmp)
                            .map_err(|e| anyhow::anyhow!("host '{host_name}': {e}"))?;
                        tmp
                    }
                    // The default already went through the same check when the configuration
                    // that produced it was loaded; a host variable is the one source that
                    // reaches here unchecked.
                    None => defaults.remote_tmp.clone(),
                },
                control_path: None,
                dumped: None,
                use_tty: host_setting(vars, "ansible_ssh_use_tty").is_none_or(
                    |value| match value {
                        Value::Bool(on) => *on,
                        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
                        // The reference reads a word it does not know as false.
                        Value::String(text) => {
                            crate::yaml::bool_from_str(text.trim()).unwrap_or(false)
                        }
                        other => crate::template::truthy(other),
                    },
                ),
            })),
            // ansible-core's own plugins this release does not drive, and a collection's, are
            // refused; any other name is no plugin at all, which the reference fails the task for.
            other if KNOWN_CONNECTIONS.contains(&other) || other.contains('.') => {
                bail!("host '{host_name}': connection '{other}' is not supported")
            }
            other => Err(ConnectionNotFound(other.to_string()).into()),
        }
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
                    // `su` reads its password from a terminal, which this side only reaches
                    // through `ssh -tt`; a local link has none to give it.
                    Some(e) if e.method == BecomeMethod::Su => {
                        return Err(ConnectError::Become(
                            "become_method 'su' is not supported yet on a local connection"
                                .to_string(),
                        ));
                    }
                    Some(e) => Some((e, check_local_escalation(&agent, e).await?)),
                    None => None,
                };
                let argv = local_argv(&agent, escalated);
                let child = Command::new(&argv[0])
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
        }
    }
}
