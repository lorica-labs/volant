// SPDX-License-Identifier: GPL-3.0-or-later
//! The batch a task backed by an action plugin makes.

use std::path::Path;
use std::time::Duration;

use tokio::sync::mpsc;
use volant_protocol::{BatchOutcome, TaskResult};

use super::super::RunOptions;
use super::super::coordinator::Event;
use super::super::prepare::Item;
use super::super::run::{
    ItemRetry, PluginStart, Relinker, Replayed, RetryLine, broke_before, finish, refusal,
    run_plugin_attempts, running_host_vars,
};
use super::Driver;
use crate::action_plugins::{Kind, LinkFacts};
use crate::agent::AgentLink;
use crate::compile::Origin;
use crate::playbook::PlayTask;
use crate::python::Union;
use crate::template::Templar;
use crate::transport::{Escalation, Transport};
use crate::vars::HostVars;

/// The one step of a batch an action plugin backs, or the controller error that ends the host
/// when the batch holds anything else: the plugin branch runs one step, so a second one would be
/// reported by nobody and never run.
pub(super) fn plugin_step<T>(batch: &[(usize, T)]) -> Result<&(usize, T), String> {
    match batch {
        [only] => Ok(only),
        _ => Err(format!(
            "a task backed by an action plugin has to be alone in its batch, and this one holds steps {:?}",
            batch.iter().map(|(index, _)| index).collect::<Vec<_>>()
        )),
    }
}

/// What a batch whose one task an action plugin backs needs to run it.
pub(super) struct PluginBatch {
    pub(super) kind: Kind,
    /// The interpreter the host the module runs on asked for, read off its variables.
    pub(super) asked: Option<String>,
    /// That host's name and variables, when it is a delegate.
    pub(super) delegate: Option<(String, HostVars)>,
}

/// What the items of a task an action plugin backs read while they run, one after the other.
pub(super) struct PluginItems<'a> {
    pub(super) task: &'a PlayTask,
    pub(super) origin: &'a Origin,
    pub(super) items: &'a [Item],
    pub(super) templar: &'a Templar,
    pub(super) pause: Option<Duration>,
    pub(super) plans: Option<&'a [ItemRetry]>,
    pub(super) kind: Kind,
    pub(super) transport: &'a Transport,
    pub(super) escalation: Option<&'a Escalation>,
    pub(super) delegate: Option<&'a (String, HostVars)>,
    pub(super) playbook_dir: &'a Path,
    pub(super) options: &'a RunOptions,
    pub(super) tx: &'a mpsc::Sender<Event>,
    pub(super) index: usize,
    pub(super) replayed: Option<&'a Replayed<'a>>,
    pub(super) name: &'a str,
    pub(super) union: Option<&'a Union>,
    pub(super) interpreters: &'a [String],
    pub(super) asked: Option<&'a str>,
    /// Whether `received` holds results the conditions have been applied to.
    pub(super) decided: bool,
}

/// How the items of a plugin task ended: stopped by the run, broken by `break_when` at an item,
/// and the batch's outcome.
pub(super) struct ItemsEnded {
    pub(super) stopped: bool,
    pub(super) broken_at: Option<usize>,
    pub(super) outcome: Result<BatchOutcome, String>,
}

/// Runs each item of a plugin task in turn, its attempts finished before the next starts,
/// filling `received` and showing each item's warnings under the task.
#[expect(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the state the driver's loop owns, lent for the items' run, and the loop it ran"
)]
pub(super) async fn run_plugin_items(
    look: PluginItems<'_>,
    driver: &mut Driver<'_>,
    owned: &mut AgentLink,
    relink: &mut Relinker<'_>,
    received: &mut [Option<TaskResult>],
    checked_to: &mut usize,
    lefts: &mut [Vec<RetryLine>],
    batch_id: &mut u64,
    logs: &mut Vec<String>,
) -> ItemsEnded {
    let mut stopped = false;
    let mut broken_at = None;
    let mut outcome = Ok(BatchOutcome::Completed);
    for (ii, item) in look.items.iter().enumerate() {
        let (at, passed) = broke_before(
            look.task,
            look.items,
            received,
            checked_to,
            ii,
            look.templar,
        );
        if driver.pause_for(look.pause, passed).await.is_none() {
            stopped = true;
            break;
        }
        if at.is_some() {
            broken_at = at;
            break;
        }
        if item.skipped.is_some() {
            continue;
        }
        if let Some(refused) = refusal(look.task) {
            received[ii] = Some(refused);
            continue;
        }
        let retry = match look.plans.map(|p| &p[ii]) {
            Some(Err(failed)) => {
                received[ii] = Some(failed.clone());
                continue;
            }
            Some(Ok(retry)) => retry.as_ref(),
            None => None,
        };
        let mut warnings = Vec::new();
        let delegate = look.delegate;
        let start = PluginStart {
            link: &LinkFacts::of(look.transport, look.escalation, delegate),
            kind: look.kind,
            running_vars: running_host_vars(delegate, std::slice::from_ref(item)),
            templar: look.templar,
            origin: look.origin,
            playbook_dir: look.playbook_dir,
            host_named_paths: look.options.host_named_paths,
            host_named_dest_dirs: look.options.host_named_dest_dirs.as_deref(),
            tx: look.tx,
            index: look.index,
        };
        let ran = if let Some(replayed) = look.replayed {
            replayed.attempts(ii, item, retry, look.templar, &mut lefts[ii])
        } else {
            run_plugin_attempts(
                owned,
                relink,
                look.name,
                batch_id,
                &start,
                look.task,
                item,
                retry,
                look.union,
                look.interpreters,
                look.asked,
                &mut driver.stop,
                &mut driver.stop_broken,
                logs,
                &mut warnings,
                &mut lefts[ii],
            )
            .await
        };
        match ran {
            // Stopped between two attempts: nothing more runs, as for every other
            // wait the stop ends.
            Ok(None) => {
                stopped = true;
                break;
            }
            // An item that does not retry comes back as the module answered, and
            // the other items' `decided` results have had their conditions.
            Ok(Some(result)) if look.decided && retry.is_none() => {
                received[ii] = Some(finish(look.task, item, result, look.templar));
            }
            Ok(Some(result)) => received[ii] = Some(result),
            // The link lost or the run interrupted: this item has no result and the
            // ones behind it do not run, exactly as for any batch the agent did not
            // finish.
            Err(ended) => {
                outcome = ended;
                break;
            }
        }
        // Shown under this task, the way every other warning is. Not censored: the
        // plugins raise nothing but the name of an argument they dropped.
        for message in warnings {
            let _ = look
                .tx
                .send(Event::Warning {
                    host: look.name.to_string(),
                    index: look.index,
                    message,
                    censored: false,
                })
                .await;
        }
    }

    ItemsEnded {
        stopped,
        broken_at,
        outcome,
    }
}
