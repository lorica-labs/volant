// SPDX-License-Identifier: GPL-3.0-or-later
//! The settings a play shares across its hosts.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};
use tokio::sync::{Notify, watch};

use crate::compile::Compiled;

/// The hosts running one step that carries a `throttle`. A host starts when fewer than its own
/// limit are running, which is how the reference counts: the limit belongs to each host's
/// rendering of the keyword, not to the step.
#[derive(Default)]
pub(in crate::executor) struct Gate {
    pub(in crate::executor) running: Mutex<usize>,
    pub(in crate::executor) freed: Notify,
}

/// Everything a host driver needs about the play, shared read-only.
pub(in crate::executor) struct PlayPlan {
    /// The play flattened into numbered steps, with the block spans that say where each one
    /// sits. Every host walks the same list, which is what lets the coordinator name a step to
    /// all of them at once.
    ///
    /// It is a watch rather than a value because of the one thing in a play that is not known
    /// when it is compiled: at a flush point the coordinator inserts the handler steps behind
    /// the flush and publishes the new list here. Every index a driver holds is still the index
    /// it was holding, because no driver reads the list again between reporting a flush point
    /// and the splice behind it - see [`Compiled::splice`] and `wait_for_splice`.
    pub(in crate::executor) plan: watch::Receiver<Arc<Compiled>>,
    /// Whether a host that failed still runs the handlers it notified: the play's own keyword,
    /// or the run's `--force-handlers` when the play says nothing.
    pub(in crate::executor) force_handlers: bool,
    /// Whether the hosts meet in front of every step: a play with `max_fail_percentage` decides
    /// after each task whether the batch goes on, and a host that ran ahead would run a task the
    /// reference never starts.
    pub(in crate::executor) lockstep: bool,
    /// How the hosts of the play move through it relative to one another.
    pub(in crate::executor) strategy: Strategy,
    /// One gate per step carrying a `throttle`, made by the first host to reach it; each host
    /// brings its own limit to it, as each does in the reference. A step's
    /// index names it for as long as the play runs: a splice only moves the steps behind a point
    /// no host has passed, so no gate is made for an index before the list has settled there.
    pub(in crate::executor) throttles: Mutex<HashMap<usize, Arc<Gate>>>,
    /// `free` and `host_pinned`: the hosts of the batch in the order they take their first fork,
    /// and how many of them have had their turn. A host queues for the fork only when the one before it has, so the
    /// forks go out in the batch's order and not in the order the scheduler starts the drivers.
    pub(in crate::executor) queue: Vec<String>,
    pub(in crate::executor) turn: watch::Sender<usize>,
    pub(in crate::executor) play_vars: Map<String, Value>,
    /// The `vars_files` maps of each host, in the order the play lists the files.
    pub(in crate::executor) vars_files: HashMap<String, Vec<Map<String, Value>>>,
    /// Every host the play resolved, whatever batch it belongs to and whether it has failed or
    /// not: `ansible_play_hosts_all`. Fixed for the whole play, batches included.
    pub(in crate::executor) all_play_hosts: Vec<String>,
    pub(in crate::executor) r#become: Option<bool>,
    pub(in crate::executor) become_user: Option<String>,
    /// The module payloads this run sends, built once before the first connection. Shared rather
    /// than copied: it is the base64 of a 631 KB zip and every host of every batch reads it.
    pub(in crate::executor) python: Option<Arc<crate::python::Union>>,
    /// `--step`: what the operator answered to each task of the batch.
    pub(in crate::executor) step: Option<crate::executor::debugger::Stepper>,
}

/// The play's `strategy`. `Debug` walks as `Linear` does and opens the debugger prompt on a
/// failed task; `Free` lets every host walk its steps without waiting for the others; and
/// `HostPinned` is `Free` with a host kept by one fork from its first step to its last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::executor) enum Strategy {
    Linear,
    Free,
    HostPinned,
    Debug,
}

impl Strategy {
    /// The strategy a play names. The pre-flight has refused every other name by now.
    pub(in crate::executor) fn named(name: Option<&str>) -> Self {
        match name {
            Some("free") => Self::Free,
            Some("host_pinned") => Self::HostPinned,
            Some("debug") => Self::Debug,
            _ => Self::Linear,
        }
    }

    /// Whether the hosts walk the play without meeting at every step.
    pub(in crate::executor) fn free(self) -> bool {
        matches!(self, Self::Free | Self::HostPinned)
    }

    pub(in crate::executor) fn pinned(self) -> bool {
        self == Self::HostPinned
    }
}

impl PlayPlan {
    /// The step list as it stands. An `Arc` clone, so the watch is never held across an `await`.
    pub(in crate::executor) fn steps(&self) -> Arc<Compiled> {
        self.plan.borrow().clone()
    }
}
