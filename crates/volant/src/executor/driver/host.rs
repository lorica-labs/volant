// SPDX-License-Identifier: GPL-3.0-or-later
//! What one host's driver carries from one step to the next.

use tokio::sync::{mpsc, watch};

use super::super::coordinator::{Event, Progress};
use crate::executor::prepare::PlayPlan;

/// Everything one host's driver carries from one step to the next: who it is, what it is
/// walking, and the watches every wait below opens on.
///
/// What a wait needs of this host is the same at every step, so it lives here rather than
/// travelling through each of them. Only `cleanup` moves underneath it.
pub(super) struct Driver<'a> {
    /// Where every word this host owes the coordinator, and every line it shows, goes.
    pub(super) tx: &'a mpsc::Sender<Event>,
    pub(super) name: &'a str,
    pub(super) plan: &'a PlayPlan,
    /// The batch's shared progress, written by the coordinator alone.
    pub(super) progress: watch::Receiver<Progress>,
    pub(super) stop: watch::Receiver<bool>,
    /// Set once the stop watch's sender is gone, so a dropped sender is never read as an
    /// interrupt and the selects below stop polling a branch that would otherwise resolve
    /// immediately forever.
    pub(super) stop_broken: bool,
    /// The index the `always` section this host is draining ends at, when it is draining one.
    /// The one index it carries that sits past a splice point, so the one a splice moves.
    pub(super) cleanup: Option<usize>,
    /// The `any_errors_fatal` step another host failed at, found when this host finished it. Read
    /// at the top of the host's loop, where it becomes a failure at that step.
    pub(super) peer_fatal: Option<usize>,
    /// The host's place in the batch's queue for forks, when it takes one step by step in the
    /// play's host order (`linear` with fewer forks than hosts).
    pub(super) place: Option<usize>,
}

impl Driver<'_> {
    /// Whether the run has been interrupted.
    pub(super) fn stopped(&self) -> bool {
        *self.stop.borrow()
    }
}
