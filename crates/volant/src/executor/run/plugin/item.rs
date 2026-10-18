// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::Duration;

use serde_json::{Map, Value};
use tokio::sync::watch;
use volant_protocol::{BatchOutcome, Task, TaskResult};

use crate::executor::CANCEL_GRACE;
use crate::executor::prepare::Item;
use crate::playbook::PlayTask;
use crate::python::Union;

use super::attempts::wait_or_stop;
use super::sweep::{sweep_abandoned, sweep_staged};
use crate::executor::run::batch::send_batch;
use crate::executor::run::blob::{PUT_FILE_WAIT, blob_preflight, put_file_within};
use crate::executor::run::channel::{AgentChannel, Relink};
use crate::executor::run::interpreter::{sub_task, unreachable_ending};
use crate::executor::run::judge::timed_out;

/// One item of a task an action plugin backs, run to its end.
///
/// Each sub-task the plugin asks for goes out alone, as a batch of one over the link the task
/// already has, and its result is handed back to the plugin on the next call. `Ok` is the item's
/// result, the one the plugin ended with; nothing the sub-tasks returned on the way leaves this
/// function, so the filtered `setup` a plugin asks for can never reach `record_facts`.
///
/// `Err` is the outcome that ended the host's batch instead - the link lost, the run interrupted -
/// with no result for the item, which the driver then handles the way it does for any batch. A
/// module that failed is not one of those: the plugin is handed its result like any other, so
/// the items behind it still run and `report_task` decides from all of them.
#[expect(
    clippy::too_many_arguments,
    reason = "the item, the plugin driving it, and the link and run state each sub-task goes over"
)]
#[expect(clippy::too_many_lines)]
pub(in crate::executor::run) async fn run_plugin_item<C: AgentChannel, R: Relink<C>>(
    link: &mut C,
    relink: &mut R,
    host: &str,
    batch_id: &mut u64,
    plugin: &mut dyn crate::action_plugins::Plugin,
    task: &PlayTask,
    item: &Item,
    union: Option<&Union>,
    interpreters: &[String],
    asked: Option<&str>,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    logs: &mut Vec<String>,
) -> Result<TaskResult, Result<BatchOutcome, String>> {
    use crate::action_plugins::{Step, gone};
    use tokio::time::Instant;
    // The `timeout` keyword bounds the whole item, as the reference's alarm bounds the whole
    // action: every sub-task, every wait for the host to come back. Past it, the item's result is
    // the timeout's, never whatever the plugin would have made of the time it had left.
    let limit = task.timeout.filter(|t| *t > 0);
    let ends = limit.and_then(|t| Instant::now().checked_add(Duration::from_secs(t)));
    let expired = || ends.is_some_and(|e| Instant::now() >= e);
    let capped =
        |d: Duration| ends.map_or(d, |e| d.min(e.saturating_duration_since(Instant::now())));
    // A sub-task gets what is left of the item's time, in the whole seconds the agent counts in.
    let bounded = |mut built: Task| {
        if let Some(e) = ends {
            let left = e
                .saturating_duration_since(Instant::now())
                .as_secs_f64()
                .ceil();
            built.timeout = Some((left as u64).max(1));
        }
        built
    };
    let timeout_result = || TaskResult::timed_out(limit.unwrap_or(0));
    let mut last = None;
    // Tries in the reconnection in hand, which the pause before the next one grows with: the
    // plugin asks again after a try that failed and after a probe it was not satisfied with, and
    // the reference's loop counts both as failures.
    let mut tries: Option<u32> = None;
    loop {
        if expired() {
            sweep_abandoned(
                plugin,
                link,
                host,
                batch_id,
                task,
                item,
                union,
                interpreters,
                asked,
                stop,
                stop_broken,
                logs,
            )
            .await;
            return Ok(timeout_result());
        }
        let (sub, dropping) = match plugin.next(last.take()) {
            Step::Done(result) => return unreachable_ending(result),
            Step::Wait(ready) => {
                let mut watcher = stop.clone();
                let stopped = async move {
                    if watcher.wait_for(|stopped| *stopped).await.is_err() {
                        std::future::pending::<()>().await;
                    }
                };
                let until = async {
                    match ends {
                        Some(e) => tokio::time::sleep_until(e).await,
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    () = ready.notified() => {}
                    () = until => {}
                    () = stopped => return Err(Ok(BatchOutcome::Cancelled { at: 0 })),
                }
                continue;
            }
            Step::PutFile {
                bytes,
                remote,
                mode,
            } => {
                let put = put_file_within(
                    link,
                    host,
                    &bytes,
                    &remote,
                    mode,
                    logs,
                    capped(PUT_FILE_WAIT),
                )
                .await;
                last = Some(match put {
                    Ok(changed) => {
                        let mut done = Map::new();
                        done.insert("changed".into(), Value::Bool(changed));
                        TaskResult(done)
                    }
                    Err(why) => TaskResult::failed_with(why),
                });
                continue;
            }
            Step::Run(sub) => (sub, false),
            Step::RunDropping(sub) => (sub, true),
            Step::Reconnect {
                probe,
                wait,
                timeout,
                attempt,
            } => {
                if wait_or_stop(capped(wait), stop, stop_broken)
                    .await
                    .is_none()
                {
                    return Err(Ok(BatchOutcome::Cancelled { at: 0 }));
                }
                let deadline = Instant::now() + capped(timeout);
                let failed = tries.map_or(0, |t| t + 1);
                tries = Some(failed);
                // The plugin's own `sleep` is its pause between tries.
                let waiting = crate::action_plugins::waits_for_connection(&task.module);
                if failed > 0
                    && !waiting
                    && wait_or_stop(capped(relink.pause(failed).min(timeout)), stop, stop_broken)
                        .await
                        .is_none()
                {
                    return Err(Ok(BatchOutcome::Cancelled { at: 0 }));
                }
                if expired() {
                    return Ok(timeout_result());
                }
                // A host that was down from the start has no interpreter to name yet: the probe
                // is built again once a try has connected.
                let built = sub_task(task, item, &probe, union, interpreters, asked);
                if let (Err(failure), false) = (&built, link.stand_in()) {
                    return Ok(failure.clone());
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    // Nothing here awaits, so a plugin asking again with no time left would hold
                    // the runtime's thread for as long as it kept asking.
                    tokio::task::yield_now().await;
                    last = Some(gone("the host did not come back in time"));
                    continue;
                }
                // Watched on the side as well, since a try can take as long as `left` and an
                // interruption is not made to wait for it.
                let mut watcher = stop.clone();
                let stopped = async move {
                    if watcher.wait_for(|stopped| *stopped).await.is_err() {
                        std::future::pending::<()>().await;
                    }
                };
                let sent_before = *batch_id;
                let once = async {
                    relink.relink(link, attempt).await?;
                    let built = match built {
                        Ok(built) => built,
                        Err(failure) => {
                            match sub_task(task, item, &probe, union, &link.interpreters(), asked) {
                                Ok(built) => built,
                                Err(_) => return Ok(Some(failure)),
                            }
                        }
                    };
                    // What is left once the connection is up, not before it.
                    let built = bounded(built);
                    *batch_id += 1;
                    let (mut flat, ended) =
                        send_batch(link, host, *batch_id, vec![built], stop, stop_broken, logs)
                            .await;
                    let answer: Result<Option<TaskResult>, String> = match ended {
                        Ok(BatchOutcome::Cancelled { .. }) => Ok(None),
                        Ok(_) => flat.pop().flatten().map(Some).ok_or_else(|| {
                            "the agent ended the probe without its result".to_string()
                        }),
                        Err(err) => Err(err),
                    };
                    answer
                };
                let tried = tokio::select! {
                    tried = tokio::time::timeout(left, once) => tried,
                    () = stopped => return Err(Ok(BatchOutcome::Cancelled { at: 0 })),
                };
                last = Some(match tried {
                    Ok(Ok(Some(result))) => result,
                    Ok(Ok(None)) => return Err(Ok(BatchOutcome::Cancelled { at: 0 })),
                    // The connection's own error, as the plugin's loop words it: the reference
                    // names the error, not the task that failed on it.
                    Ok(Err(why)) if waiting => {
                        gone(why.strip_prefix("Task failed: ").unwrap_or(&why))
                    }
                    Ok(Err(why)) => gone(&why),
                    Err(_) => {
                        // A probe that went out may still be running on the host, and the agent
                        // drops any `RunBatch` that arrives while a batch runs: left there, it
                        // would swallow the next sub-task and leave it unanswered. Cancelled, or,
                        // when the agent does not say it stopped, the link replaced like one the
                        // host outlived.
                        // Both waits past the deadline still give way to an interruption.
                        if *batch_id != sent_before {
                            let id = *batch_id;
                            let mut watcher = stop.clone();
                            let stopped = async move {
                                if watcher.wait_for(|stopped| *stopped).await.is_err() {
                                    std::future::pending::<()>().await;
                                }
                            };
                            let settle = async {
                                if link.stop_batch(id, CANCEL_GRACE).await {
                                    return Ok(());
                                }
                                relink.relink(link, attempt).await
                            };
                            tokio::select! {
                                settled = settle => settled.map_err(|why| {
                                    Err(format!("replacing a link busy with a probe: {why}"))
                                })?,
                                () = stopped => return Err(Ok(BatchOutcome::Cancelled { at: 0 })),
                            }
                        }
                        gone(&format!("no answer after {} seconds", left.as_secs_f64()))
                    }
                });
                continue;
            }
        };
        tries = None;
        let built = match sub_task(task, item, &sub, union, interpreters, asked) {
            Ok(built) => built,
            // The ping of a host that was down from the start: the plugin's own loop goes on to
            // reconnect, and names the interpreter once the host answers.
            Err(_) if dropping && link.stand_in() => {
                last = Some(gone("the host has not answered yet"));
                continue;
            }
            Err(failure) => return Ok(failure),
        };
        *batch_id += 1;
        let tasks = vec![built];
        // What the sub-task needs on the host and could not be put there fails the item as it
        // stands, and the plugin never sees it: the module did not run, and a plugin handed the
        // error as the module's result would dress it as one (`copy` adds `diff` and the local
        // checksum). The reference's action fails the same way on a transfer that raised.
        if let Err(err) = blob_preflight(link, host, &tasks, union, &sub.files, logs).await {
            return Ok(TaskResult::failed_with(err));
        }
        // The transfer ran on the item's time. Past its end the module is not sent, so a `copy`
        // whose upload outlived `timeout` leaves `dest` alone and reports the timeout; before
        // it, the module gets what is left after the transfer.
        if expired() {
            sweep_staged(
                link,
                host,
                batch_id,
                &tasks[0],
                union,
                stop,
                stop_broken,
                logs,
            )
            .await;
            return Ok(timeout_result());
        }
        let tasks: Vec<Task> = tasks.into_iter().map(bounded).collect();
        let (mut flat, ended) =
            send_batch(link, host, *batch_id, tasks, stop, stop_broken, logs).await;
        let result = flat.pop().flatten();
        match ended {
            Ok(BatchOutcome::Completed | BatchOutcome::Failed { .. }) => {}
            // The link went, which a sub-task that may take it down reports to the plugin
            // instead of ending the host's run - with the sub-task's own result when it arrived
            // before the link went.
            Err(err) if dropping => {
                last = Some(result.unwrap_or_else(|| gone(&err)));
                continue;
            }
            _ => return Err(ended),
        }
        // A batch the agent ended cleanly with no result in it has lost a sub-task's result
        // between two turns. Handed to the plugin as nothing, it would be read as the module's
        // own answer; the item fails naming it instead.
        let Some(result) = result else {
            return Ok(TaskResult::failed_with(format!(
                "the agent ended the '{}' sub-task without its result",
                sub.module
            )));
        };
        // The `timeout` keyword bounds the reference's whole action, and running out of it ends
        // the action there: the item's result is the timeout, never something the plugin made of
        // it.
        if timed_out(&result) {
            return Ok(if limit.is_some() {
                timeout_result()
            } else {
                result
            });
        }
        last = Some(result);
    }
}
