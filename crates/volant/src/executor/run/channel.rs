// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;
use std::time::Duration;

use volant_protocol::{FromAgent, ToAgent};

use crate::agent::{AgentLink, AgentSource, BlobMemory};
use crate::executor::{LinkKey, Reboots};
use crate::transport::{ConnectionDefaults, Escalation, Transport};

use super::links::{connect, retire_host_links};

/// What running a batch needs of a link: the memory the link carries, the two directions of the
/// wire, and the way a batch is stopped.
///
/// A trait rather than the link itself so a batch can be run against an agent that answers what
/// a test scripted - a real link is a child process, which is why none of this was testable. The three ways it goes wrong - a payload refused, a payload
/// sent twice, a batch sent for a payload nobody confirmed - are all failures nothing else
/// would show until a host ran the wrong thing.
pub(in crate::executor) trait AgentChannel {
    fn memory(&mut self) -> &mut BlobMemory;
    async fn ask(&mut self, msg: &ToAgent) -> std::io::Result<()>;
    /// Sends the blob frame a `put_blob` is followed by.
    async fn ask_bytes(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    async fn answer(&mut self) -> std::io::Result<Option<FromAgent>>;
    /// Asks for a batch to stop and waits for the agent to say it has, at most `grace`.
    async fn stop_batch(&mut self, id: u64, grace: Duration) -> bool;
    /// Whether this channel stands in for a host that never answered.
    fn stand_in(&self) -> bool {
        false
    }
    /// The interpreters the agent behind this channel found, empty when it has none to tell.
    fn interpreters(&self) -> Vec<String> {
        Vec::new()
    }
    /// Where the batches over this channel leave their timings, when anything reads them.
    fn timings(&mut self) -> Option<&mut crate::profile::Ledger> {
        None
    }
    /// Where to look for what a stopped agent said; see `AgentLink::stderr_note`.
    fn stderr_note(&self) -> String {
        String::new()
    }
    /// The line a command given a terminal ends its stderr with; see `SshTarget::tty_close`.
    async fn tty_close(&mut self) -> Option<String> {
        None
    }
    /// Gives the link up after a question went unanswered: its answer may still arrive, and the
    /// next task would read it as its own. The link is dead afterwards.
    fn abandon(&mut self) {}
}

/// How a plugin that expects its host to go away gets a link to it again.
pub(in crate::executor) trait Relink<C> {
    /// Replaces `link` with a fresh one to the same host, under the same user, after dropping
    /// every other link kept for that host: a link the host outlived is of no use to anybody.
    /// `link` is left as it was when no fresh one could be opened. `connect_timeout` bounds the
    /// connection's own setup, as the reference's `connect_timeout` sets the connection plugin's.
    async fn relink(
        &mut self,
        link: &mut C,
        connect_timeout: Option<Duration>,
    ) -> Result<(), String>;

    /// How long to wait before the next try, after `failed` of them: the reference's doubling
    /// from one second, capped at twelve (`reboot.py`, which adds up to a second at random).
    fn pause(&self, failed: u32) -> Duration {
        Duration::from_secs(1 << failed.saturating_sub(1).min(4)).min(Duration::from_secs(12))
    }
}

/// [`Relink`] for the driver's links: `key` is the one the batch runs on, taken out of `links`
/// while the plugin holds it.
pub(in crate::executor) struct Relinker<'a> {
    pub(in crate::executor) links: &'a mut HashMap<LinkKey, AgentLink>,
    /// Each link's key, with the host's reboot count when the link was last proved alive.
    pub(in crate::executor) checked: &'a mut HashMap<LinkKey, u64>,
    pub(in crate::executor) reboots: &'a Reboots,
    pub(in crate::executor) key: &'a LinkKey,
    pub(in crate::executor) escalation: Option<&'a Escalation>,
    pub(in crate::executor) agents: &'a AgentSource,
    pub(in crate::executor) defaults: &'a ConnectionDefaults,
}

impl Relink<AgentLink> for Relinker<'_> {
    async fn relink(
        &mut self,
        link: &mut AgentLink,
        connect_timeout: Option<Duration>,
    ) -> Result<(), String> {
        // Before anything else, so that every other driver checks its own link to this host
        // again whether or not this try comes up.
        self.reboots.bump(&self.key.host);
        // Closed off-task, as a replaced escalated link is: their agents died with the host.
        for old in retire_host_links(self.links, self.checked, &self.key.host) {
            tokio::spawn(old.shutdown());
        }
        // Checked again by the next batch if this never comes back up: the link left in place is
        // the one the host outlived.
        self.checked.remove(self.key);
        // The host's shared connection went down with it, perhaps without a word: a master left
        // on a dead TCP connection would take this try's session and hold it past any timeout.
        // Stopped and reopened through the one transport, so both name the same socket.
        let transport = with_connect_timeout(&self.key.transport, connect_timeout);
        transport.stop_shared().await;
        let fresh = connect(&transport, self.agents, self.defaults, self.escalation)
            .await
            .map_err(|e| e.to_string())?;
        let old = std::mem::replace(link, fresh);
        tokio::spawn(old.shutdown());
        Ok(())
    }
}

/// `transport` with ssh's `ConnectTimeout` set to `timeout`, in the whole seconds it takes and at
/// least one; a local transport has no connection to set up.
pub(super) fn with_connect_timeout(transport: &Transport, timeout: Option<Duration>) -> Transport {
    let mut transport = transport.clone();
    if let (Some(timeout), Transport::Ssh(target)) = (timeout, &mut transport) {
        target.connect_timeout = Duration::from_secs((timeout.as_secs_f64().ceil() as u64).max(1));
    }
    transport
}

impl AgentChannel for AgentLink {
    fn memory(&mut self) -> &mut BlobMemory {
        self.blobs()
    }

    async fn ask(&mut self, msg: &ToAgent) -> std::io::Result<()> {
        self.send(msg).await
    }

    async fn ask_bytes(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.send_bytes(bytes).await
    }

    async fn answer(&mut self) -> std::io::Result<Option<FromAgent>> {
        self.recv().await
    }

    async fn stop_batch(&mut self, id: u64, grace: Duration) -> bool {
        self.cancel(id, grace).await
    }

    fn interpreters(&self) -> Vec<String> {
        AgentLink::interpreters(self).to_vec()
    }

    fn stand_in(&self) -> bool {
        self.is_stand_in()
    }

    fn timings(&mut self) -> Option<&mut crate::profile::Ledger> {
        Some(self.ledger())
    }

    fn stderr_note(&self) -> String {
        AgentLink::stderr_note(self)
    }

    async fn tty_close(&mut self) -> Option<String> {
        AgentLink::tty_close(self).await
    }

    fn abandon(&mut self) {
        AgentLink::abandon(self);
    }
}
