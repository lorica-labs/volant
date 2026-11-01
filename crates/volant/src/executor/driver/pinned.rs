// SPDX-License-Identifier: GPL-3.0-or-later
//! The place a host holds in the queue for forks.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

use super::{Driver, PlayPlan};

/// The host's place in the queue for forks: none under `free`, where each host runs at its own
/// pace.
pub(super) fn place(plan: &PlayPlan, name: &str) -> Option<usize> {
    plan.queue
        .iter()
        .position(|h| h == name)
        .filter(|_| !plan.strategy.free())
}

/// Where the hosts of the batch stand in the queue for forks, in the batch's order.
pub(in crate::executor) struct Turns {
    /// `free` and `host_pinned`: how many hosts have had their turn at their first fork.
    pub(in crate::executor) first: watch::Sender<usize>,
    /// `linear` with fewer forks than hosts: where each host stands. A host takes a fork for a
    /// step once every host before it has, so the forks go out in the play's host order and not
    /// in the order the drivers get there.
    pub(in crate::executor) places: watch::Sender<Vec<Place>>,
}

/// Where one host stands in the queue for forks.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub(in crate::executor) struct Place {
    /// How many steps it is done with: run, skipped or failed.
    passed: usize,
    /// The first step of a batch it has collected and not yet taken a fork for. Collecting walks
    /// past steps the batch will run later, so `passed` alone would let the hosts behind it in.
    held: Option<usize>,
}

impl Place {
    /// The first step this host may still take a fork for.
    fn next(self) -> usize {
        self.held.map_or(self.passed, |held| held.min(self.passed))
    }
}

impl Default for Turns {
    fn default() -> Self {
        Self {
            first: watch::channel(0).0,
            places: watch::channel(Vec::new()).0,
        }
    }
}

impl Turns {
    /// Changes the place of the host at `mine`.
    fn at(&self, mine: usize, change: impl FnOnce(&mut Place)) {
        self.places.send_modify(|places| {
            if places.len() <= mine {
                places.resize(mine + 1, Place::default());
            }
            change(&mut places[mine]);
        });
    }

    /// Records that the host at `mine` is done with every step before `passed`.
    pub(super) fn reach(&self, mine: usize, passed: usize) {
        self.at(mine, |place| place.passed = place.passed.max(passed));
    }
}

/// A host's place in the queue for forks. Dropping it hands the place to the next host, so a host
/// that leaves before it queued - interrupted, or panicked - holds nobody up.
pub(super) struct Turn<'a> {
    pub(super) gate: &'a Turns,
    pub(super) mine: usize,
    pub(super) passed: bool,
}

impl Turn<'_> {
    pub(super) fn pass(&mut self) {
        if !std::mem::replace(&mut self.passed, true) {
            let next = self.mine + 1;
            self.gate
                .first
                .send_modify(|reached| *reached = (*reached).max(next));
        }
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.pass();
        self.gate.at(self.mine, |place| {
            place.passed = usize::MAX;
            place.held = None;
        });
    }
}

impl Driver<'_> {
    /// Says, when the host has a place in the queue, that it is done with the step at `index`.
    pub(super) fn passed(&self, index: usize) {
        if let Some(mine) = self.place {
            self.plan.turns.reach(mine, index + 1);
        }
    }

    /// Says, when the host has a place in the queue, that its batch starts at `pos`: until it
    /// takes its fork, the hosts behind it wait for it there.
    pub(super) fn hold(&self, pos: usize) {
        if let Some(mine) = self.place {
            self.plan.turns.at(mine, |place| {
                place.held = Some(place.held.map_or(pos, |held| held.min(pos)));
            });
        }
    }

    /// A fork for the batch at `pos`, `Some(None)` when the forks are gone. With a place in the
    /// queue, taken once every live host before this one has taken its own for that step or moved
    /// past it, as the reference hands its forks out in the play's host order. A throttled batch
    /// waits for its throttle first and is not ordered, so the two waits never hold each other up.
    /// `None` when the run is interrupted on the way.
    pub(super) async fn fork_for(
        &mut self,
        forks: &Arc<Semaphore>,
        pos: usize,
        ordered: bool,
    ) -> Option<Option<OwnedSemaphorePermit>> {
        let Some(mine) = self.place else {
            return Some(Arc::clone(forks).acquire_owned().await.ok());
        };
        let mut places = self.plan.turns.places.subscribe();
        while ordered && !self.queued_before(&places.borrow_and_update(), mine, pos) {
            tokio::select! {
                _ = places.changed() => {}
                // The coordinator is gone: nobody leaves the batch any more, and waiting on it
                // would never end.
                changed = self.progress.changed() => if changed.is_err() {
                    break;
                },
                res = self.stop.changed(), if !self.stop_broken => {
                    if res.is_err() {
                        self.stop_broken = true;
                    } else {
                        return None;
                    }
                }
            }
        }
        let permit = Arc::clone(forks).acquire_owned().await.ok();
        self.plan.turns.at(mine, |place| {
            place.passed = place.passed.max(pos + 1);
            place.held = None;
        });
        Some(permit)
    }

    /// Whether every host in front of `mine` has queued for the step at `pos`, moved past it, or
    /// left the batch. Before the coordinator first says who is live, nobody has left.
    fn queued_before(&self, places: &[Place], mine: usize, pos: usize) -> bool {
        let live = Arc::clone(&self.progress.borrow().live_hosts);
        (0..mine).all(|host| {
            places.get(host).is_some_and(|place| place.next() > pos)
                || (!live.is_empty() && !live.contains(&self.plan.queue[host]))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host's place only moves forward, a batch in hand keeps it at the batch's first step,
    /// and a host that leaves holds nobody up.
    #[test]
    fn a_place_only_moves_forward_and_leaving_frees_it() {
        let turns = Turns::default();
        turns.reach(1, 3);
        turns.reach(1, 2);
        turns.at(1, |place| place.held = Some(1));
        let next = |turns: &Turns| {
            turns
                .places
                .borrow()
                .iter()
                .map(|p| p.next())
                .collect::<Vec<_>>()
        };
        assert_eq!(next(&turns), [0, 1]);
        drop(Turn {
            gate: &turns,
            mine: 0,
            passed: true,
        });
        assert_eq!(next(&turns), [usize::MAX, 1]);
    }
}
