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
    // A replay contacts no host: a stand-in that never connects carries the batch, which the
    // driver answers from the recording.
    if options.replay.is_some() {
        if !links.contains_key(key) {
            let link = AgentLink::dead().map_err(|e| ConnectError::Unreachable(e.to_string()))?;
            links.insert(key.clone(), link);
        }
        return Ok(links.get_mut(key).expect("inserted just above"));
    }
    // A host reaching the controller as the connecting user borrows an idle agent there before
    // starting one of its own; see `LocalPool`. It is checked like a link kept from a play before.
    if !links.contains_key(key)
        && LocalPool::shares(key)
        && let Some(link) = options.local.take()
    {
        checked.remove(key);
        links.insert(key.clone(), link);
    }
    // Why the kept connection was not reused, kept only in case the reconnection below fails
    // too: on its own it is not a failure (a single reconnection is the designed recovery), but
    // discarding it silently would leave a reconnect failure reporting only its own cause.
    // The agent a task stopped is gone, though its connection is not: the next batch starts one.
    if links.get(key).is_some_and(AgentLink::died) {
        checked.remove(key);
        if let Some(dead) = links.remove(key) {
            tokio::spawn(dead.shutdown());
        }
    }
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

/// Agents on the controller itself, shared by every host that reaches it over `local` without
/// escalating. Such an agent holds nothing of the host it serves - its stage directory, its
/// background jobs and its warm Python servers belong to the machine and the user - so one host
/// can hand it to the next. Kept per host instead, a play on 500 local hosts held 500 agents and
/// 500 warm Python servers on the controller and ran out of memory where ansible-core, bounded by
/// `forks`, used 1.7 GB. With the pool a run holds the agents of the hosts running at once and at
/// most `cap` idle ones.
pub struct LocalPool {
    idle: std::sync::Mutex<Vec<AgentLink>>,
    cap: usize,
}

impl LocalPool {
    pub fn new(cap: usize) -> Self {
        Self {
            idle: std::sync::Mutex::new(Vec::new()),
            cap,
        }
    }

    /// Whether the link filed under `key` can serve any host: an agent on the controller, run as
    /// the connecting user.
    pub fn shares(key: &LinkKey) -> bool {
        key.transport == Transport::Local && key.become_user.is_none()
    }

    fn take(&self) -> Option<AgentLink> {
        self.idle.lock().expect("local pool lock").pop()
    }

    #[cfg(all(test, unix))]
    pub(in crate::executor) fn idle(&self) -> usize {
        self.idle.lock().expect("local pool lock").len()
    }

    /// Keeps `link` for the next host, or closes it when the pool is full, its agent is gone or
    /// it is the stand-in of a host that never answered.
    pub fn give(&self, link: AgentLink) {
        let mut idle = self.idle.lock().expect("local pool lock");
        if idle.len() < self.cap && !link.died() && !link.is_stand_in() {
            idle.push(link);
        } else {
            drop(idle);
            tokio::spawn(link.shutdown());
        }
    }

    /// Hands the shareable links of a host over to the pool, forgetting that they were checked.
    pub(in crate::executor) fn park(
        &self,
        links: &mut HashMap<LinkKey, AgentLink>,
        checked: &mut HashMap<LinkKey, u64>,
    ) {
        let keys: Vec<LinkKey> = links.keys().filter(|k| Self::shares(k)).cloned().collect();
        for key in keys {
            checked.remove(&key);
            if let Some(link) = links.remove(&key) {
                self.give(link);
            }
        }
    }

    /// Closes every idle agent, concurrently, as `RunState::shutdown_links` closes the kept ones.
    pub async fn shutdown(&self) {
        let idle = std::mem::take(&mut *self.idle.lock().expect("local pool lock"));
        let mut closing = tokio::task::JoinSet::new();
        for link in idle {
            closing.spawn(link.shutdown());
        }
        while closing.join_next().await.is_some() {}
    }
}

/// The fork permit going back at a barrier, and the host's shared local agent with it: a host
/// waiting there holds no agent another host could be running on.
pub(in crate::executor) fn release_at_barrier(
    permit: &mut Option<tokio::sync::OwnedSemaphorePermit>,
    links: &mut HashMap<LinkKey, AgentLink>,
    checked: &mut HashMap<LinkKey, u64>,
    pool: &LocalPool,
) {
    *permit = None;
    pool.park(links, checked);
}

/// [`reuse_or_connect`], except for a host no agent runs on: its batch is carried by a stand-in,
/// since the helper makes the connection for each task itself and the plugin it runs asks
/// nothing of this one.
pub(in crate::executor) async fn link_for<'a>(
    links: &'a mut HashMap<LinkKey, AgentLink>,
    checked: &mut HashMap<LinkKey, u64>,
    key: &LinkKey,
    escalation: Option<&Escalation>,
    agents: &AgentSource,
    options: &RunOptions,
) -> Result<&'a mut AgentLink, ConnectError> {
    if !matches!(key.transport, Transport::Python(_)) {
        return reuse_or_connect(links, checked, key, escalation, agents, options).await;
    }
    if !links.contains_key(key) {
        let link = AgentLink::dead().map_err(|e| ConnectError::Unreachable(e.to_string()))?;
        links.insert(key.clone(), link);
    }
    Ok(links.get_mut(key).expect("inserted just above"))
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
    if link.ready() {
        return Ok(link);
    }
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
