// SPDX-License-Identifier: GPL-3.0-or-later
//! How the controller reaches a host. Every transport ends up as a process whose stdin and
//! stdout carry protocol frames.

use std::path::Path;
use std::process::Stdio;

use anyhow::bail;
use tokio::process::Command;

use crate::agent::AgentLink;
use crate::inventory::Host;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// Run the agent on the controller machine itself.
    Local,
}

impl Transport {
    /// Reads `ansible_connection`. Ansible's default is `ssh`, which this release does not have yet.
    pub fn for_host(host: &Host) -> anyhow::Result<Transport> {
        match host
            .vars
            .get("ansible_connection")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("ssh")
        {
            "local" => Ok(Transport::Local),
            "ssh" => bail!(
                "host '{}': connection 'ssh' is not available in this release; set ansible_connection=local",
                host.name
            ),
            other => bail!(
                "host '{}': connection '{other}' is not supported",
                host.name
            ),
        }
    }

    pub async fn connect(&self, agent: &Path) -> anyhow::Result<AgentLink> {
        match self {
            Transport::Local => {
                let child = Command::new(agent)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| anyhow::anyhow!("starting agent {}: {e}", agent.display()))?;
                AgentLink::new(child)
            }
        }
    }
}
