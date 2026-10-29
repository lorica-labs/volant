// SPDX-License-Identifier: GPL-3.0-or-later
//! The items of a `copy` or `template` loop run side by side: each round puts the next sub-task
//! of every item still running in one batch, so a loop over files the host already holds costs
//! one trip rather than one or two per item.
//!
//! Only for what [`run_plugin_item`](super::item::run_plugin_item) would run the same way: no
//! `timeout`, no `async`, no reconnection, no file put through the connection. A plugin that
//! asks for any of those here fails its item, naming the step, and the driver never groups a
//! task whose plugin can.

use tokio::sync::watch;
use volant_protocol::{BatchOutcome, NOT_RUN, OnlyIf, TaskResult};

use crate::action_plugins::{FileBlob, Plugin, Step, Sub};
use crate::executor::prepare::Item;
use crate::executor::run::batch::send_batch;
use crate::executor::run::blob::blob_preflight;
use crate::executor::run::channel::AgentChannel;
use crate::executor::run::interpreter::{mark_unsent, sub_task, unreachable_ending};
use crate::playbook::PlayTask;
use crate::python::Union;

/// One item in the run: its plugin, the answer it is handed next, and the answer of a sub-task
/// sent ahead of its asking (see [`Step::RunThen`]).
struct Running<'p> {
    item: usize,
    plugin: Box<dyn Plugin + 'p>,
    last: Option<TaskResult>,
    ahead: Option<(Sub, TaskResult)>,
    sent: bool,
    /// A step held over to the next round, which it could not share (see [`shares_a_file`]).
    held: Option<Step>,
}

/// Runs every item's plugin to its result, a round of sub-tasks per batch. The results come back
/// in item order. A batch the agent did not finish ends the whole run, as it ends a lone item.
#[expect(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "run_plugin_item's arguments for many plugins at once, and its loop"
)]
pub(in crate::executor) async fn run_plugin_group<C: AgentChannel>(
    link: &mut C,
    host: &str,
    batch_id: &mut u64,
    plugins: Vec<(usize, Box<dyn Plugin + '_>)>,
    task: &PlayTask,
    items: &[Item],
    union: Option<&Union>,
    interpreters: &[String],
    asked: Option<&str>,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    logs: &mut Vec<String>,
) -> Result<Vec<(usize, TaskResult)>, Result<BatchOutcome, String>> {
    let mut running: Vec<Running> = plugins
        .into_iter()
        .map(|(item, plugin)| Running {
            item,
            plugin,
            last: None,
            ahead: None,
            sent: false,
            held: None,
        })
        .collect();
    let mut done = Vec::new();
    while !running.is_empty() {
        let mut tasks = Vec::new();
        let mut files = Vec::new();
        // For each task of the batch: the running item it is for, and the sub-task it answers
        // ahead of its asking when it was sent that way.
        let mut wants: Vec<(usize, Option<Sub>)> = Vec::new();
        let mut k = 0;
        while k < running.len() {
            let item = &items[running[k].item];
            let step = if let Some(step) = running[k].held.take() {
                step
            } else {
                let last = running[k].last.take();
                running[k].plugin.next(last)
            };
            match step {
                Step::Done(mut result) => {
                    if !running[k].sent {
                        mark_unsent(&mut result);
                    }
                    done.push((running.remove(k).item, unreachable_ending(result)?));
                }
                Step::Run(sub) => {
                    if let Some((then, answer)) = running[k].ahead.take()
                        && then.module == sub.module
                        && then.args == sub.args
                        && sub.files.is_empty()
                    {
                        running[k].last = Some(answer);
                        continue;
                    }
                    if shares_a_file(&sub, &files) {
                        running[k].held = Some(Step::Run(sub));
                        k += 1;
                        continue;
                    }
                    match sub_task(task, item, &sub, union, interpreters, asked) {
                        Ok(built) => {
                            running[k].sent |= sub.module != "raw";
                            files.extend(sub.files.iter().cloned());
                            tasks.push(built);
                            wants.push((k, None));
                            k += 1;
                        }
                        Err(failure) => done.push((running.remove(k).item, failure)),
                    }
                }
                Step::RunThen {
                    sub,
                    then,
                    checksum,
                } => match sub_task(task, item, &sub, union, interpreters, asked) {
                    Ok(built) => {
                        running[k].sent = true;
                        let at = tasks.len();
                        tasks.push(built);
                        wants.push((k, None));
                        if let Ok(mut second) =
                            sub_task(task, item, &then, union, interpreters, asked)
                        {
                            second.only_if = Some(OnlyIf { task: at, checksum });
                            tasks.push(second);
                            wants.push((k, Some(then)));
                        }
                        k += 1;
                    }
                    Err(failure) => done.push((running.remove(k).item, failure)),
                },
                other => {
                    let msg = format!("the items of this loop run together, and no {other:?}");
                    done.push((running.remove(k).item, TaskResult::failed_with(msg)));
                }
            }
        }
        if tasks.is_empty() {
            continue;
        }
        *batch_id += 1;
        // What could not be put on the host fails the items of this round, as it fails a lone
        // item: their modules did not run.
        if let Err(err) = blob_preflight(link, host, &tasks, union, &files, logs).await {
            let mut failed: Vec<usize> = wants.iter().map(|(k, _)| *k).collect();
            failed.dedup();
            for k in failed.into_iter().rev() {
                done.push((running.remove(k).item, TaskResult::failed_with(&err)));
            }
            continue;
        }
        let (flat, ended) = send_batch(link, host, *batch_id, tasks, stop, stop_broken, logs).await;
        match ended {
            Ok(BatchOutcome::Completed | BatchOutcome::Failed { .. }) => {}
            _ => return Err(ended),
        }
        let mut lost = Vec::new();
        let answers = flat.into_iter().chain(std::iter::repeat(None));
        for ((k, then), answer) in wants.into_iter().zip(answers) {
            match (then, answer) {
                (Some(then), Some(answer)) if !answer.0.contains_key(NOT_RUN) => {
                    running[k].ahead = Some((then, answer));
                }
                (Some(_), _) => {}
                (None, Some(answer)) => running[k].last = Some(answer),
                (None, None) => lost.push(k),
            }
        }
        for k in lost.into_iter().rev() {
            done.push((
                running.remove(k).item,
                TaskResult::failed_with(
                    "the agent ended a sub-task of this item without its result",
                ),
            ));
        }
    }
    done.sort_by_key(|(item, _)| *item);
    Ok(done)
}

/// Whether `sub` stages a file a sub-task of this round already stages. A staged file is
/// consumed by the one module it was put there for (`copy` moves it into place), so two items
/// writing the same bytes to two places take two rounds, as they take two trips one at a time.
fn shares_a_file(sub: &Sub, staged: &[(String, FileBlob)]) -> bool {
    sub.files
        .iter()
        .any(|(_, blob)| staged.iter().any(|(_, other)| other.hash == blob.hash))
}

#[cfg(test)]
mod tests {
    use serde_json::Map;

    use super::{FileBlob, Sub, shares_a_file};

    /// Two items writing the same bytes stage one blob twice: the second waits for a round of
    /// its own, since the first item's `copy` consumes the staged file.
    #[test]
    fn a_staged_file_is_not_shared_within_a_round() {
        let blob = |hash: &str| FileBlob {
            hash: hash.into(),
            body: crate::action_plugins::BlobBody::B64(String::new()),
        };
        let copy = |hash: &str| Sub {
            module: "copy",
            args: Map::new(),
            files: vec![("src".into(), blob(hash))],
        };
        let staged = vec![("src".to_string(), blob("aa"))];
        assert!(shares_a_file(&copy("aa"), &staged));
        assert!(!shares_a_file(&copy("bb"), &staged));
        assert!(!shares_a_file(&copy("aa"), &[]));
    }
}
