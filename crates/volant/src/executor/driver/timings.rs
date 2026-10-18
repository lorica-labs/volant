// SPDX-License-Identifier: GPL-3.0-or-later
//! Filing what a link measured into the profile.

use std::collections::HashMap;

use crate::agent::AgentLink;
use crate::compile::Compiled;
use crate::profile::{Phase, Profile, TaskRecord};

/// Files what `link` measured since it was last asked: its blob and wire time, and one record
/// per result under the step it belongs to. `flat_steps` maps a flat batch's positions to their
/// steps; the plugin and retry paths send the one step `first`, and leave it empty.
///
/// Called on the two ways a stopped run leaves a batch as well as at its end, so a profile
/// written after an interruption still lists the attempts the run already showed.
pub(super) fn file_timings(
    link: &mut AgentLink,
    profile: &Profile,
    host: &str,
    steps: &Compiled,
    flat_steps: &[usize],
    first: usize,
    prepare_micros: &HashMap<usize, u64>,
) {
    let ledger = link.take_ledger();
    profile.phase(Some(host), Phase::Blob, ledger.blob_micros);
    profile.phase(Some(host), Phase::Wire, ledger.wire_micros);
    for (k, module, ran) in ledger.ran {
        let index = flat_steps.get(k).copied().unwrap_or(first);
        profile.task(TaskRecord {
            host: host.to_string(),
            index,
            task: steps.steps[index].task.name.clone(),
            module,
            ran,
            prepare_micros: prepare_micros.get(&index).copied().unwrap_or(0),
        });
    }
}
