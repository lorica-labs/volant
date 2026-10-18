// SPDX-License-Identifier: GPL-3.0-or-later
//! What one host's driver carries from step to step, and every wait it opens on the others.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::compile::{Compiled, StepKind, after, after_pending};

use super::Driver;
use crate::executor::run::wait_or_stop;
/// What became of a `run_once` step, from the point of view of a host that did not run it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RunOnce {
    /// The one runner finished the step without failing; whatever it registered or set as a
    /// fact is in the store for every host of the batch.
    Done,
    /// It failed. Measured on ansible-core 2.19.12: every other host leaves the play here, with
    /// no line of its own and no recap entry, and the run exits 2 - with a `rescue` around the
    /// task as well, which keeps the runner itself in the play.
    Failed,
    /// It left the play without a verdict, which is what an unreachable host does. Measured:
    /// exit 4 and no recap line for the hosts that were waiting, so their departure adds
    /// nothing of its own.
    Gone,
}

impl Driver<'_> {
    /// Waits `delay`, or gives up when the run is interrupted.
    pub(super) async fn sleep_between(&mut self, delay: Duration) -> Option<()> {
        wait_or_stop(delay, &mut self.stop, &mut self.stop_broken).await
    }

    /// A loop's `pause`, `times` over; `None` when the run was interrupted.
    pub(super) async fn pause_for(&mut self, pause: Option<Duration>, times: usize) -> Option<()> {
        if let Some(pause) = pause {
            for _ in 0..times {
                self.sleep_between(pause).await?;
            }
        }
        Some(())
    }

    /// Waits until every other live host of the batch has finished the step in front of `pos`.
    ///
    /// This is what a step the batch meets in front of opens on, and its exit condition is its
    /// own: alone in the batch there is nobody left to wait for, which is also how the wait ends
    /// when every other host has died.
    ///
    /// `None` when the run was interrupted.
    pub(super) async fn wait_for_barrier(&mut self, pos: usize) -> Option<()> {
        loop {
            if self.stopped() {
                return None;
            }
            let p = self.progress.borrow().clone();
            // Alone in the batch there is nobody to wait for, except under `max_fail_percentage`:
            // the coordinator decides there, once this host has reported the step behind it,
            // whether the batch is lost, and a lone host that raced ahead would run a step the
            // reference never starts.
            let alone = p.live_hosts.len() <= 1 && !self.plan.lockstep;
            if p.live_hosts.is_empty() || alone || p.completed_through.is_some_and(|c| c + 1 >= pos)
            {
                return Some(());
            }
            tokio::select! {
                changed = self.progress.changed() => {
                    // The coordinator is gone, so no further progress can be published
                    // and waiting on it would never end.
                    if changed.is_err() {
                        return Some(());
                    }
                }
                res = self.stop.changed(), if !self.stop_broken => {
                    if res.is_err() {
                        self.stop_broken = true;
                    } else {
                        return None;
                    }
                }
            }
        }
    }

    /// Waits until every live host has finished the `any_errors_fatal` step at `at`, and says
    /// whether one of them failed there. A failure answers at once: nothing a host that has not
    /// finished yet could do changes it.
    ///
    /// The verdict is read from the same snapshot as the live set, and the coordinator writes it
    /// before the failing host leaves that set, so a snapshot with every live host past `at` and
    /// no verdict is one where nobody failed. A host the coordinator no longer counts live has
    /// failed already and waits for nobody.
    ///
    /// `None` when the run was interrupted.
    async fn fatal_verdict(&mut self, at: usize) -> Option<bool> {
        loop {
            if self.stopped() {
                return None;
            }
            let p = self.progress.borrow().clone();
            // Under `max_fail_percentage` the batch may be lost at this very step, which the
            // coordinator decides once every live host has finished it: this host has to know
            // that before it picks a rescue.
            let decided = !self.plan.lockstep || p.aborted;
            if p.fatal.contains(&at) && (decided || p.completed_through.is_some_and(|c| c >= at)) {
                return Some(true);
            }
            if p.live_hosts.len() <= 1
                || !p.live_hosts.iter().any(|h| h == self.name)
                || p.completed_through.is_some_and(|c| c >= at)
            {
                return Some(false);
            }
            tokio::select! {
                changed = self.progress.changed() => {
                    if changed.is_err() {
                        return Some(false);
                    }
                }
                res = self.stop.changed(), if !self.stop_broken => {
                    if res.is_err() {
                        self.stop_broken = true;
                    } else {
                        return None;
                    }
                }
            }
        }
    }

    /// Waits for the one runner of the `run_once` step at `pos` to say what it made of it.
    ///
    /// `None` when the run was interrupted, the way every other wait in this file answers it.
    ///
    /// The verdict is read before the live host list, and that order is the whole of it: the
    /// coordinator publishes a failed result and the shrunken list in the same `Progress`, so a
    /// host that asked "is the runner still live?" first would see a runner that is merely gone
    /// and leave at exit 4 where the reference exits 2.
    pub(super) async fn wait_for_run_once(&mut self, runner: &str, pos: usize) -> Option<RunOnce> {
        loop {
            if self.stopped() {
                return None;
            }
            let p = self.progress.borrow().clone();
            match p.run_once.get(&pos) {
                Some(true) => return Some(RunOnce::Failed),
                Some(false) => return Some(RunOnce::Done),
                None => {}
            }
            // The host this step was handed to has left the batch without a verdict, which is
            // what an unreachable host does. Asked of that one host and not of the list's
            // length: with three hosts waiting on a runner that died, a test for "anybody else
            // still live" would be true for ever and this wait would never end.
            if !p.live_hosts.iter().any(|h| h == runner) {
                return Some(RunOnce::Gone);
            }
            tokio::select! {
                changed = self.progress.changed() => {
                    // The coordinator is gone, so no verdict can ever be published.
                    if changed.is_err() {
                        return Some(RunOnce::Gone);
                    }
                }
                res = self.stop.changed(), if !self.stop_broken => {
                    if res.is_err() {
                        self.stop_broken = true;
                    } else {
                        return None;
                    }
                }
            }
        }
    }

    /// Whether `advance` from `pos` would walk past a splice point - a flush or an include - and
    /// so block waiting for the splice.
    ///
    /// A driver may only wait for a splice once it owes the coordinator nothing in front of it:
    /// the coordinator walks the step list in order and holds at each index until every host is
    /// done with it or gone, so a host waiting at a splice point while a step behind it is still
    /// unreported waits for an index the coordinator can never reach. The batch collection loop
    /// asks this before moving on from a step it has queued and not yet run.
    pub(super) fn steps_over_a_splice_point(&self, compiled: &Compiled, pos: usize) -> bool {
        let next = match self.cleanup {
            Some(end) => match after_pending(compiled, pos, end) {
                Some((next, _)) => next,
                None => return false,
            },
            None => after(compiled, pos),
        };
        (pos + 1..next.min(compiled.steps.len())).any(|i| self.splices(compiled, i))
    }

    /// Whether the step at `i` is a flush point a free host passes without waiting: no handler
    /// exists for the splice to put behind it.
    pub(super) fn nothing_to_splice(&self, compiled: &Compiled, i: usize) -> bool {
        self.plan.strategy.free()
            && matches!(compiled.steps[i].kind, StepKind::Flush { explicit: false })
            && compiled.handlers.is_empty()
    }

    /// Whether the step at `i` is a splice point this host has to wait at.
    pub(super) fn splices(&self, compiled: &Compiled, i: usize) -> bool {
        crate::compile::is_splice_point(&compiled.steps[i].kind)
            && !self.nothing_to_splice(compiled, i)
    }

    /// Waits until the hosts in front of this one in a `host_pinned` batch have queued for their
    /// forks. `None` when the run is interrupted.
    pub(super) async fn wait_turn(&mut self, mine: usize) -> Option<()> {
        let mut turn = self.plan.turn.subscribe();
        tokio::select! {
            reached = turn.wait_for(|reached| *reached >= mine) => reached.ok().map(|_| ()),
            () = async {
                if self.stop.wait_for(|stopped| *stopped).await.is_err() {
                    std::future::pending::<()>().await;
                }
            } => None,
        }
    }

    /// A permit of `gate`, or `None` when the run is interrupted before one is free.
    pub(super) async fn acquire(&mut self, gate: Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
        loop {
            if self.stopped() {
                return None;
            }
            tokio::select! {
                permit = Arc::clone(&gate).acquire_owned() => return permit.ok(),
                res = self.stop.changed(), if !self.stop_broken => {
                    if res.is_err() {
                        self.stop_broken = true;
                    } else {
                        return None;
                    }
                }
            }
        }
    }

    /// The permit that keeps step `at` to its `throttle` hosts at a time; `Some(None)` for a step
    /// with none, and `None` when the run is interrupted while the host waits for its turn.
    pub(super) async fn throttled(
        &mut self,
        at: usize,
        limit: Option<u64>,
    ) -> Option<Option<OwnedSemaphorePermit>> {
        let Some(limit) = limit.filter(|limit| *limit > 0) else {
            return Some(None);
        };
        let gate =
            {
                let mut gates = self.plan.throttles.lock().expect("throttle lock");
                let permits = usize::try_from(limit).unwrap_or(usize::MAX);
                Arc::clone(gates.entry(at).or_insert_with(|| {
                    Arc::new(Semaphore::new(permits.min(Semaphore::MAX_PERMITS)))
                }))
            };
        self.acquire(gate).await.map(Some)
    }

    /// The step this host moves to after finishing `pos`, past the sections it has no reason to
    /// enter. A host draining an `always` section after a failure finishes that section instead,
    /// and `cleanup` - the index that section ends at - moves with it to the next one.
    ///
    /// Returns the length of the step list when the play is over for this host, which is what the
    /// driver's own bound reads as "done", and `None` when the run was interrupted on the way.
    pub(super) async fn advance(&mut self, pos: usize) -> Option<usize> {
        let compiled = self.plan.steps();
        // An `any_errors_fatal` step is finished for this host only when it is finished for the
        // batch. A host that failed there makes this one fail at the same step: it does not step
        // over anything, since the failure decides where it goes.
        if compiled.steps.get(pos).is_some_and(|s| s.task.fails_all())
            && self.fatal_verdict(pos).await?
        {
            self.peer_fatal = Some(pos);
            return Some(pos + 1);
        }
        let next = match self.cleanup {
            Some(end) => after_pending(&compiled, pos, end).map(|(next, end)| {
                self.cleanup = Some(end);
                next
            }),
            None => Some(after(&compiled, pos)),
        };
        let Some(next) = next else {
            return Some(compiled.steps.len());
        };
        // Every flush point between here and there grows the list behind it, and every index past
        // it - this destination, and the end of the section this host may be draining - moves by
        // however much it grew.
        let grown = self.stepped_over(pos + 1..next).await?;
        if let Some(end) = self.cleanup.as_mut() {
            *end += grown;
        }
        Some(next + grown)
    }

    /// Tells the coordinator about every step of `range` this host stepped over, stopping at each
    /// splice point on the way, and answers how much longer the list is for it.
    ///
    /// A host has to stop at a splice point it steps over as surely as at one it runs. The
    /// coordinator inserts steps behind a flush or an include once every host has reported that
    /// index, and a host that read the list again before that would be holding an index into a
    /// list that has changed underneath it - the exact shape this file's barrier invariant exists
    /// to prevent. Measured on ansible-core 2.19.12: a `meta: flush_handlers` written inside a
    /// `rescue:` nobody entered runs nothing and shows nothing, and so does an `include_tasks`
    /// written there, which is what puts a live host in this position.
    pub(super) async fn stepped_over(&mut self, range: std::ops::Range<usize>) -> Option<usize> {
        let mut grown = 0;
        let mut from = range.start;
        loop {
            let compiled = self.plan.steps();
            let end = range.end + grown;
            let splice =
                (from..end.min(compiled.steps.len())).find(|&i| self.splices(&compiled, i));
            let Some(at) = splice else {
                self.skipped(from..end).await;
                return Some(grown);
            };
            self.skipped(from..at + 1).await;
            let before = compiled.steps.len();
            drop(compiled);
            // The bare wait, and not the one that moves `cleanup`: the growth of this splice is
            // part of what this function answers, and the caller moves `cleanup` by the whole of
            // that answer. Moving it here as well would move it twice and end the host's drain
            // past the section it is draining.
            self.wait_past_splice(at).await?;
            grown += self.plan.steps().steps.len() - before;
            // Back at the splice point's successor, which is now the first of the steps it just
            // grew by. This host steps over those too - they sit in the section the flush or the
            // include sat in, and it is not in that section - but it still owes the coordinator a
            // word about each of them, or the barrier behind them opens on a host that never said
            // it had passed them.
            from = at + 1;
        }
    }

    /// Waits until the coordinator has put the handler steps in behind the flush point at `at`,
    /// and moves the end of the `always` section this host may be draining by however much the
    /// splice grew the list - the one index it carries that sits past `at`.
    ///
    /// `before` is how long the list was when the host still owed `at` a `TaskDone`, and the
    /// caller has to read it before sending that word. Reading it here would be a race: the
    /// coordinator publishes the splice as soon as the last host reports the index, so the list
    /// can already have grown by the time this runs, and the growth would then measure as nothing
    /// at all - leaving `cleanup` where it was and ending the host's drain one step early. Rare,
    /// load dependent, and it reports success while a cleanup step nobody ran goes missing.
    ///
    /// `None` only when the run was interrupted. A coordinator that is gone answers `Some(())`:
    /// no splice will ever land, so `cleanup` stays where it is and the host carries on with the
    /// list it has, which is what `wait_past_splice` answers `false` for.
    pub(super) async fn wait_for_splice(&mut self, at: usize, before: usize) -> Option<()> {
        if self.wait_past_splice(at).await?
            && let Some(end) = self.cleanup.as_mut()
        {
            *end += self.plan.steps().steps.len() - before;
        }
        Some(())
    }

    /// The wait alone, for the caller whose `cleanup` must not move here.
    ///
    /// This is the whole of the plan's one dynamic primitive on the driver's side: between
    /// reporting a splice point and this returning, the host reads nothing from the step list, so
    /// the index it holds cannot be invalidated by the splice.
    ///
    /// `true` when the splice landed, `false` when the coordinator is gone and none ever will
    /// land - which is why this is answered rather than assumed: an index that moves with the
    /// list must not move for a splice that was never published. `None` when the run was
    /// interrupted.
    async fn wait_past_splice(&mut self, at: usize) -> Option<bool> {
        loop {
            if self.stopped() {
                return None;
            }
            if self
                .progress
                .borrow()
                .spliced_through
                .is_some_and(|s| s >= at)
            {
                return Some(true);
            }
            tokio::select! {
                changed = self.progress.changed() => {
                    // The coordinator is gone, so no splice can be published and waiting on one
                    // would never end. Whatever the host has left to do, it does on the list it
                    // has.
                    if changed.is_err() {
                        return Some(false);
                    }
                }
                res = self.stop.changed(), if !self.stop_broken => {
                    if res.is_err() {
                        self.stop_broken = true;
                    } else {
                        return None;
                    }
                }
            }
        }
    }

    /// Tells the coordinator about every step in `range` that this host stepped over, so the
    /// shared progress it publishes counts this host as having reached them.
    ///
    /// It is what keeps a host that steps over a section from waiting on itself: the progress
    /// every barrier reads is the lowest step any live host has finished, so a host that jumped
    /// from 3 to 9 without saying so would hold that figure at 3 while waiting for it to reach 8.
    /// Both halves of that are live now: a host that failed steps over the rest of the body on
    /// its way to a cleanup, and a host that failed nothing steps over every `rescue` it walks
    /// past - the second with the whole play still waiting on it.
    pub(super) async fn skipped(&self, range: std::ops::Range<usize>) {
        for index in range {
            self.task_done(index).await;
        }
    }
}
