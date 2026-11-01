// SPDX-License-Identifier: GPL-3.0-or-later
//! The place a host holds in the queue for forks.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

use super::Driver;

/// Where the hosts of the batch stand in the queue for forks, in the batch's order.
pub(in crate::executor) struct Turns {
    /// `free` and `host_pinned`: how many hosts have had their turn at their first fork.
    pub(in crate::executor) first: watch::Sender<usize>,
    /// `linear` with fewer forks than hosts: for each host, the number of steps it has held a fork
    /// for or moved past. A host takes a fork for a step once every host before it has, so the
    /// forks go out in the play's host order and not in the order the drivers get there.
    pub(in crate::executor) places: watch::Sender<Vec<usize>>,
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
    /// Records that the host at `mine` is done queueing for every step before `reached`.
    pub(super) fn reach(&self, mine: usize, reached: usize) {
        self.places.send_modify(|places| {
            if places.len() <= mine {
                places.resize(mine + 1, 0);
            }
            places[mine] = places[mine].max(reached);
        });
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
        self.gate.reach(self.mine, usize::MAX);
    }
}

impl Driver<'_> {
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
        self.plan.turns.reach(mine, pos + 1);
        Some(permit)
    }

    /// Whether every host in front of `mine` has queued for the step at `pos`, moved past it, or
    /// left the batch. Before the coordinator first says who is live, nobody has left.
    fn queued_before(&self, places: &[usize], mine: usize, pos: usize) -> bool {
        let live = Arc::clone(&self.progress.borrow().live_hosts);
        (0..mine).all(|host| {
            places.get(host).is_some_and(|&reached| reached > pos)
                || (!live.is_empty() && !live.contains(&self.plan.queue[host]))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host's place only moves forward, and a host that leaves holds nobody up.
    #[test]
    fn a_place_only_moves_forward_and_leaving_frees_it() {
        let turns = Turns::default();
        turns.reach(1, 3);
        turns.reach(1, 2);
        assert_eq!(*turns.places.borrow(), [0, 3]);
        drop(Turn {
            gate: &turns,
            mine: 0,
            passed: true,
        });
        assert_eq!(*turns.places.borrow(), [usize::MAX, 3]);
    }
}
