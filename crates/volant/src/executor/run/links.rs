// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;
use std::time::Duration;

use crate::agent::{AgentLink, AgentSource};
use crate::executor::{LinkKey, RunOptions};
use crate::transport::{ConnectError, ConnectionDefaults, Escalation, Transport};

/// The connection for the next batch, under `key`'s target user and over `key`'s transport: the
/// one kept from an earlier play if its agent still answers, a fresh one otherwise. A kept
/// connection gets exactly one liveness check per play, and one more each time a task of this
/// run rebooted its host ([`crate::executor::Reboots`]) - the driver that rebooted it dropped only its own links,
/// and another host delegating to it would otherwise reuse one the reboot killed. A failed check
/// gets exactly one reconnection; a failed reconnection is the host's `UNREACHABLE`.
///
/// The transport comes in on the key rather than being resolved here, so the connection a batch
/// opens and the identity it is filed under can never be built from two different views of the
/// host's variables.
pub(in crate::executor) async fn reuse_or_connect<'a>(
    links: &'a mut HashMap<LinkKey, AgentLink>,
    checked: &mut HashMap<LinkKey, u64>,
    key: &LinkKey,
    escalation: Option<&Escalation>,
    agents: &AgentSource,
    options: &RunOptions,
) -> Result<&'a mut AgentLink, ConnectError> {
    // Why the kept connection was not reused, kept only in case the reconnection below fails
    // too: on its own it is not a failure (a single reconnection is the designed recovery), but
    // discarding it silently would leave a reconnect failure reporting only its own cause.
    let reboots = options.reboots.of(&key.host);
    let (stale, retired) = check_kept(
        links,
        checked,
        key,
        reboots,
        options.defaults.connect_timeout,
    )
    .await;
    // Closed off-task, as `Relinker::relink` closes them.
    for old in retired {
        tokio::spawn(old.shutdown());
    }
    if !links.contains_key(key) {
        let link = match connect(&key.transport, agents, &options.defaults, escalation).await {
            Ok(link) => link,
            Err(ConnectError::Unreachable(msg)) => {
                return Err(ConnectError::Unreachable(match stale {
                    Some(reason) => format!(
                        "the kept connection failed ({reason}), and reconnecting failed too: {msg}"
                    ),
                    None => msg,
                }));
            }
            // A refused escalation is the host's answer about this user, not a connection that
            // failed, so it travels on untouched: adding the stale note would turn a failed
            // task into something that reads like an unreachable host.
            Err(err) => return Err(err),
        };
        links.insert(key.clone(), link);
    }
    Ok(links.get_mut(key).expect("connected just above"))
}

/// What checking a kept link needs of it, so the check can be driven by a fake in tests.
pub(super) trait KeptLink {
    async fn handshake(&mut self) -> anyhow::Result<()>;
    async fn shutdown(self);
}

impl KeptLink for AgentLink {
    async fn handshake(&mut self) -> anyhow::Result<()> {
        AgentLink::handshake(self).await
    }

    async fn shutdown(self) {
        AgentLink::shutdown(self).await;
    }
}

/// Takes every link kept for `host` out of `links`, and forgets that they were checked, for the
/// caller to close.
pub(super) fn retire_host_links<L>(
    links: &mut HashMap<LinkKey, L>,
    checked: &mut HashMap<LinkKey, u64>,
    host: &str,
) -> Vec<L> {
    let keys: Vec<LinkKey> = links.keys().filter(|k| k.host == host).cloned().collect();
    keys.iter()
        .filter_map(|k| {
            checked.remove(k);
            links.remove(k)
        })
        .collect()
}

/// Checks the link kept for `key` if the host rebooted since it was last proved alive (or it
/// never was). A link that answers stays in `links`. One that fails, at once or by timing out,
/// is closed, and then the host's shared connection is stopped and every other link kept for the
/// host is retired, as `Relinker::relink` does.
///
/// Both halves matter. The host's plain and escalated links ride one master, and each is
/// checked only when it is next used, so the link that was idle through a reboot may still point
/// at a master long gone while the socket holds a newer one on a connection the host dropped
/// without a word; failing at once on the old one says nothing about the new one, so the socket
/// is stopped on every failure. And a sibling left in `links` would be checked in its turn, fail,
/// and stop the master this reconnection is about to open; retired, it reconnects through that
/// master instead, unchecked.
///
/// Returns why the kept link was not reused, if it failed, and the retired links.
pub(super) async fn check_kept<L: KeptLink>(
    links: &mut HashMap<LinkKey, L>,
    checked: &mut HashMap<LinkKey, u64>,
    key: &LinkKey,
    reboots: u64,
    timeout: Duration,
) -> (Option<String>, Vec<L>) {
    if checked.insert(key.clone(), reboots) == Some(reboots) {
        return (None, Vec::new());
    }
    let Some(mut link) = links.remove(key) else {
        // No kept link, so nothing to check: this key connects through the socket as it is.
        // That is safe only if the socket's master was proven alive since the host's last
        // reboot, by a sibling on the same transport kept and checked (or connected) at this
        // count; a host this run never rebooted keeps whatever master it has. Otherwise the
        // master may ride a connection the reboot dropped, and it is stopped first, with the
        // host's other links retired so none of them later stops the master this key opens.
        let proven = links
            .keys()
            .any(|k| k.transport == key.transport && checked.get(k) == Some(&reboots));
        if reboots > 0 && key.transport.is_shared() && !proven {
            let retired = retire_host_links(links, checked, &key.host);
            key.transport.stop_shared().await;
            return (None, retired);
        }
        return (None, Vec::new());
    };
    let stale = match tokio::time::timeout(timeout, link.handshake()).await {
        Ok(Ok(())) => {
            links.insert(key.clone(), link);
            return (None, Vec::new());
        }
        Ok(Err(err)) => format!("{err:#}"),
        Err(_) => "no answer from the kept connection".to_string(),
    };
    link.shutdown().await;
    let retired = retire_host_links(links, checked, &key.host);
    key.transport.stop_shared().await;
    (Some(stale), retired)
}

/// Opens one link, escalated when `escalation` is given. Every way this can fail is a host the
/// run cannot reach and comes back as `ConnectError::Unreachable`, except a `sudo` that refused,
/// which comes back as `ConnectError::Become` because the host itself answered.
pub(super) async fn connect(
    transport: &Transport,
    agents: &AgentSource,
    defaults: &ConnectionDefaults,
    escalation: Option<&Escalation>,
) -> Result<AgentLink, ConnectError> {
    let mut link = transport.connect(agents, escalation).await?;
    let timeout = defaults.connect_timeout;
    tokio::time::timeout(timeout, link.handshake())
        .await
        .map_err(|_| {
            ConnectError::Unreachable(format!(
                "no answer from the agent after {} seconds",
                timeout.as_secs()
            ))
        })?
        .map_err(|e| ConnectError::Unreachable(format!("{e:#}")))?;
    Ok(link)
}
