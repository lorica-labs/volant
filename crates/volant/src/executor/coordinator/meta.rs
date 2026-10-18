// SPDX-License-Identifier: GPL-3.0-or-later
//! Carrying out a `meta` effect once the coordinator has seen it.

use std::collections::HashSet;
use std::sync::Arc;

use crate::stats::Stats;

use super::publish::Coordinator;
use super::types::{MetaEffect, Round};
use crate::executor::RunState;
use crate::executor::prepare::PlayPlan;
/// What carrying out a `meta` effect reads besides the books the round keeps.
pub(super) struct MetaContext<'a> {
    /// The host that ran the action and the step it ran at.
    pub(super) host: &'a str,
    pub(super) at: usize,
    /// The batch as the inventory cut it: measured, a clear in a later batch leaves an earlier batch's failures.
    pub(super) whole: &'a [String],
    /// The play's `hosts:` pattern, which a refreshed inventory is asked about again.
    pub(super) pattern: &'a str,
    pub(super) plan: &'a PlayPlan,
    /// Ends the run when a refresh cannot read its sources.
    pub(super) abort: &'a crate::executor::Abort,
    /// The step a host that was unreachable before the play picks it up at.
    pub(super) first: usize,
}

/// Carries out what a `meta` action asked of the batch, and answers the warnings it raised. Runs
/// in the order the events arrive, which puts it in front of the `TaskDone` of the step: the
/// verdict the other hosts wait for is published after it.
pub(super) fn apply_meta(
    effect: MetaEffect,
    round: &mut Round,
    coordinator: &mut Coordinator<'_>,
    cx: &MetaContext<'_>,
    state: &mut RunState,
    stats: &mut Stats,
) -> Vec<String> {
    let mut warned = Vec::new();
    match effect {
        MetaEffect::EndHost => {
            coordinator.removed.insert(cx.host.to_string());
        }
        MetaEffect::EndBatch | MetaEffect::EndPlay => {
            // Every host of the batch that is still in it, the one that asked included. The
            // verdict goes out in the same publish, because a host that read the shrunken list
            // before it would take the runner for a host that vanished without one.
            let staying: Vec<String> = coordinator
                .batch_hosts
                .iter()
                .filter(|h| !state.failed_hosts.contains(*h))
                .cloned()
                .collect();
            coordinator.removed.extend(staying);
            coordinator.ending.insert(cx.at);
            coordinator.run_once.insert(cx.at, false);
            round.end_play |= effect == MetaEffect::EndPlay;
        }
        MetaEffect::ClearFacts => {
            state
                .vars
                .lock()
                .expect("vars lock")
                .clear_gathered(cx.whole);
        }
        MetaEffect::ClearHostErrors => {
            // The hosts that are out of the play now stay out of this round, forgiven or not:
            // the reference revives an unreachable host in a round of its own and never a
            // failed one.
            coordinator.removed.extend(
                cx.whole
                    .iter()
                    .filter(|h| state.failed_hosts.contains(*h))
                    .cloned(),
            );
            for h in cx.whole {
                // An earlier play's unreachable host stays unreachable for every play after,
                // whatever is cleared: its mark on the exit code stays with it.
                let kept = state.unreachable_hosts.contains(h);
                stats.clear_errors(h, kept);
                if !kept {
                    state.failed_hosts.remove(h);
                }
            }
            for (h, resume) in round.resume.drain() {
                if cx.whole.contains(&h) {
                    round.dark.remove(&h);
                    state.failed_hosts.remove(&h);
                    round.revived.push((h, resume));
                }
            }
            for h in std::mem::take(&mut round.dormant) {
                state.failed_hosts.remove(&h);
                round.revived.push((h, cx.first));
            }
            coordinator.failed_here.clear();
            coordinator.last_failure = None;
        }
        MetaEffect::RefreshInventory => {
            let reload = state.vars.lock().expect("vars lock").reload();
            if let Some((sources, config, bridge)) = reload {
                // Every source is read again, scripts and plugins run anew. What a source says
                // while it is read is warned about; an error that ends the run at its start ends
                // it here with the same message and code. The fact cache is not read again.
                let loaded =
                    crate::inventory::load_sources_with(&sources, &config, &|| Arc::clone(&bridge));
                for note in loaded.notes {
                    if let crate::inventory::Note::Warning(text) = note {
                        warned.push(text);
                    }
                }
                let fresh = loaded.inventory;
                if let Some(fatal) = loaded.error {
                    cx.abort.raise_with(fatal.code, fatal.message);
                    return warned;
                }
                let fresh = state
                    .vars
                    .lock()
                    .expect("vars lock")
                    .refresh_inventory(&fresh);
                // The hosts of the batch the play's pattern no longer finds leave the play where
                // it stands, not failed; a host the file gained joins the next play. The verdict
                // goes out in the same publish as the dismissal, for the reason the end of a
                // batch does.
                let found: Vec<String> = fresh
                    .resolve(cx.pattern)
                    .hosts
                    .into_iter()
                    .map(|host| host.name)
                    .collect();
                let kept: HashSet<String> = found.iter().cloned().collect();
                state
                    .vars
                    .lock()
                    .expect("vars lock")
                    .set_play_hosts_all(found);
                let leaving: Vec<String> = coordinator
                    .batch_hosts
                    .iter()
                    .filter(|h| !kept.contains(*h))
                    .cloned()
                    .collect();
                coordinator.removed.extend(leaving.iter().cloned());
                coordinator.dismissed.extend(leaving);
                coordinator.run_once.insert(cx.at, false);
                state.refreshed = Some(fresh);
            }
        }
    }
    coordinator.publish(&state.failed_hosts, &cx.plan.steps().steps);
    warned
}
