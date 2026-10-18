// SPDX-License-Identifier: GPL-3.0-or-later
//! What a run carries from play to play: the reboots counted, the abort, the connections kept and the hosts that failed.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use crate::agent::AgentLink;
use crate::inventory::Inventory;
use crate::template::Templar;
use crate::transport::Transport;
use crate::vars::VarStore;

/// How many times a task of this run has sent each host away, by the name its links are filed
/// under. A driver's kept link to a host belongs to that driver alone, so the one that reboots
/// the host can drop only its own; every other driver reads this before reusing a link, and
/// checks again one it checked before the count moved.
#[derive(Debug, Default)]
pub struct Reboots(Mutex<HashMap<String, u64>>);

impl Reboots {
    pub(crate) fn of(&self, host: &str) -> u64 {
        self.0
            .lock()
            .expect("reboots lock")
            .get(host)
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn bump(&self, host: &str) {
        *self
            .0
            .lock()
            .expect("reboots lock")
            .entry(host.to_string())
            .or_default() += 1;
    }
}

/// Ends the whole run from inside a play, the way a pre-flight refusal ends it before one: exit
/// 1, the reason on its own, no recap, and nothing `ignore_errors` or a `rescue` can catch.
/// Raising flips `stop`, so every driver stops as it does on an interruption; `run_play` then
/// returns the reason as its error.
///
/// Measured by reading only: ansible-core 2.19.12's strategy raises `AnsibleError` from
/// `_process_pending_results` for a handler it cannot find, with `ERROR_ON_MISSING_HANDLER` on
/// by default, which leaves the play loop and the run.
#[derive(Debug)]
pub struct Abort {
    stop: watch::Sender<bool>,
    reason: Mutex<Option<String>>,
    code: std::sync::atomic::AtomicI32,
}

impl Abort {
    pub fn new(stop: watch::Sender<bool>) -> Self {
        Self {
            stop,
            reason: Mutex::new(None),
            code: std::sync::atomic::AtomicI32::new(1),
        }
    }

    /// The exit code of the reason kept: 1 unless [`Abort::raise_with`] gave another.
    pub fn code(&self) -> i32 {
        self.code.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// [`Abort::raise`] ending the run with `code`, when it is the first reason kept.
    pub fn raise_with(&self, code: i32, reason: String) {
        let first = self.reason.lock().expect("abort lock").is_none();
        if first {
            self.code.store(code, std::sync::atomic::Ordering::SeqCst);
        }
        self.raise(reason);
    }

    /// Stops the run without a reason: the operator's interruption.
    pub fn interrupt(&self) {
        self.stop.send_replace(true);
    }

    /// Stops the run and keeps `reason`, unless another host already gave one.
    ///
    /// An interruption that came first stays one: the run then ends as interrupted, with its own
    /// code, and a reason raised after it is dropped.
    pub fn raise(&self, reason: String) {
        let mut kept = self.reason.lock().expect("abort lock");
        if kept.is_none() && *self.stop.borrow() {
            return;
        }
        kept.get_or_insert(reason);
        self.stop.send_replace(true);
    }

    pub(super) fn reason(&self) -> Option<String> {
        self.reason.lock().expect("abort lock").clone()
    }
}

/// Which agent a kept connection belongs to. The escalated user is part of the identity
/// because two connections to one host under two users are two different agents, and so is the
/// transport: connection settings are ordinary variables, so two tasks of one host can resolve
/// different addresses, ports, users or keys, and a link reused across that difference would
/// send a task to the wrong machine.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LinkKey {
    pub host: String,
    pub become_user: Option<String>,
    /// The program, method and flags of an escalated link; see `EscalationProgram`.
    pub become_program: Option<crate::transport::EscalationProgram>,
    pub transport: Transport,
}

/// What outlives a play: the templar, the variable store hosts write into, the hosts that
/// are out of the run, and the agents still connected.
pub struct RunState {
    pub templar: Arc<Templar>,
    pub vars: Arc<Mutex<VarStore>>,
    pub failed_hosts: HashSet<String>,
    /// The hosts of `failed_hosts` that went unreachable and stayed so at the end of a batch.
    /// Measured on ansible-core 2.19.12: such a host is out of every play after, and
    /// `meta: clear_host_errors` brings it back for the play it runs in and no further, while a
    /// host that failed is back in the plays after the one that cleared it.
    pub unreachable_hosts: HashSet<String>,
    /// The inventory a `meta: refresh_inventory` read again, for the plays behind the one that
    /// asked: the command line takes it before the next play's hosts are resolved.
    pub refreshed: Option<Inventory>,
    pub verbosity: u8,
    /// Agents kept alive between plays, the way Ansible keeps its ssh connections open. Only
    /// the connecting user's own link per host is kept; see `keep_links`.
    pub links: HashMap<LinkKey, AgentLink>,
    /// The module payloads this run sends, built once before the first connection from every
    /// Python module the compiled plays name. `None` for a run of native tasks alone, which is
    /// why a controller without ansible-core still runs those.
    pub python: Option<Arc<crate::python::Union>>,
}

impl RunState {
    /// Closes every kept connection, concurrently: each `shutdown` waits up to a couple of
    /// seconds for its own agent, and a run of N hosts closing them one at a time would pay
    /// N times that before the recap ever shows. Draining the map makes a second call a no-op,
    /// so the recap paths can each ask for it without closing one agent twice.
    pub async fn shutdown_links(&mut self) {
        let mut closing = tokio::task::JoinSet::new();
        for (_, link) in self.links.drain() {
            closing.spawn(link.shutdown());
        }
        while closing.join_next().await.is_some() {}
    }
}

/// How a play ended, for the run that contains it.
pub struct PlayEnd {
    /// Every live host of a batch failed, so the run is over: no further batch of this play, no
    /// further play, and the recap prints where it stands. Measured on ansible-core 2.19.12,
    /// three shapes of the same rule - two hosts failing in a one-batch play, one host failing
    /// in a `serial: 1` batch, and a whole batch going unreachable - and the run stops at all
    /// three, at exit 2 for the failures and 4 for the unreachable batch.
    pub stop_run: bool,
}
