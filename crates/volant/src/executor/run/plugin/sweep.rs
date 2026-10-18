// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::json;
use tokio::sync::watch;
use volant_protocol::{StagedFile, Task};

use crate::executor::CANCEL_GRACE;
use crate::executor::prepare::Item;
use crate::playbook::PlayTask;
use crate::python::Union;

use crate::executor::run::batch::send_batch;
use crate::executor::run::channel::AgentChannel;
use crate::executor::run::interpreter::sub_task;

/// How long each removal of a directory a plugin left behind may take, the item's time being up.
const SWEEP_WAIT: u64 = 10;

/// Runs what a plugin left on the host when its item ran out of time, whatever each answers. A
/// link that breaks on the way is left for the next batch to find.
#[expect(
    clippy::too_many_arguments,
    reason = "run_plugin_item's link and run state, which each removal goes over"
)]
pub(super) async fn sweep_abandoned<C: AgentChannel>(
    plugin: &mut dyn crate::action_plugins::Plugin,
    link: &mut C,
    host: &str,
    batch_id: &mut u64,
    task: &PlayTask,
    item: &Item,
    union: Option<&Union>,
    interpreters: &[String],
    asked: Option<&str>,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    logs: &mut Vec<String>,
) {
    for sub in plugin.abandoned() {
        let Ok(mut built) = sub_task(task, item, &sub, union, interpreters, asked) else {
            continue;
        };
        built.timeout = Some(SWEEP_WAIT);
        *batch_id += 1;
        let (_, ended) =
            send_batch(link, host, *batch_id, vec![built], stop, stop_broken, logs).await;
        if let Err(why) = ended {
            logs.push(format!(
                "removing what an action plugin left on the host: {why}"
            ));
            return;
        }
    }
}

/// Takes off the host the files a sub-task staged and never ran with, the item's deadline having
/// passed during the transfer: a staged file can hold a rendered secret, and would otherwise stay
/// until the link closes. A `stat` stages them under `path`, and the agent removes every staged
/// file once its task ran; the reference's temporary directory goes when its alarm ends the
/// action (`TaskExecutor`'s `cleanup`). Nothing is read of the answer.
#[expect(
    clippy::too_many_arguments,
    reason = "the link, the batch counter and the run's interruption a batch needs"
)]
pub(super) async fn sweep_staged<C: AgentChannel>(
    link: &mut C,
    host: &str,
    batch_id: &mut u64,
    built: &Task,
    union: Option<&Union>,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    logs: &mut Vec<String>,
) {
    let (Some(ran_under), Some(stat)) = (&built.payload, union.and_then(|u| u.payload("stat")))
    else {
        return;
    };
    if built.files.is_empty() {
        return;
    }
    let args = ["follow", "get_checksum", "get_mime", "get_attributes"]
        .into_iter()
        .map(|key| (key.to_string(), json!(false)))
        .collect();
    let sweep = Task {
        module: "stat".into(),
        args,
        timeout: Some(CANCEL_GRACE.as_secs()),
        force_python: stat.force_python,
        payload: Some(stat.under(&ran_under.interpreter)),
        files: built
            .files
            .iter()
            .map(|file| StagedFile {
                arg: "path".into(),
                blob: file.blob.clone(),
            })
            .collect(),
        ..Task::default()
    };
    *batch_id += 1;
    let _ = send_batch(link, host, *batch_id, vec![sweep], stop, stop_broken, logs).await;
}
