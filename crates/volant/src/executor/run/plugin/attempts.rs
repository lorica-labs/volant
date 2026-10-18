// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::Duration;

use serde_json::{Map, Value};
use tokio::sync::watch;
use volant_protocol::modules::{CHECK_MODE_ARG, DIFF_ARG, short_name};
use volant_protocol::{BatchOutcome, Task, TaskResult};

use crate::executor::prepare::Item;
use crate::playbook::PlayTask;
use crate::python::{ModulePayload, Union};
use crate::template::Templar;
use crate::vars::HostVars;

use super::item::run_plugin_item;
use crate::executor::run::async_job::{Polled, announce_retry, async_dir, poll_job, polls};
use crate::executor::run::channel::{AgentChannel, Relink};
use crate::executor::run::output::TTY_KEY;
use crate::executor::run::retry::{Attempt, Retry, RetryLine, judge_attempt};

/// What starting an action plugin reads besides the item it runs and the warnings it raises.
pub(in crate::executor) struct PluginStart<'a> {
    pub(in crate::executor) kind: crate::action_plugins::Kind,
    /// The variables of the host the module runs on - the delegate's when there is one.
    pub(in crate::executor) running_vars: &'a HostVars,
    pub(in crate::executor) delegated: bool,
    /// Whether the task escalates: the link its sub-tasks go over is the escalated one.
    pub(in crate::executor) escalated: bool,
    /// Whether that link runs the agent on the controller itself.
    pub(in crate::executor) local: bool,
    pub(in crate::executor) templar: &'a Templar,
    pub(in crate::executor) origin: &'a crate::compile::Origin,
    pub(in crate::executor) playbook_dir: &'a std::path::Path,
    /// Where an `async` task's `ASYNC` and `RETRYING` lines go, and the step they belong to.
    pub(in crate::executor) tx: &'a tokio::sync::mpsc::Sender<crate::executor::coordinator::Event>,
    pub(in crate::executor) index: usize,
}

/// One item of a task an action plugin backs, attempt after attempt under `retries`/`until`.
///
/// Each attempt starts the plugin afresh, so the whole sequence of sub-tasks runs again from its
/// first, and a file the plugin stages is read and sent again: the agent consumed the one the
/// last attempt staged. `until` reads the result the sequence ended with, once `changed_when`
/// and `failed_when` have judged it, which is the rule a module's retry follows. Measured on
/// ansible-core 2.19.12: `retries: 3` with no `until` on a `copy` that succeeds is `attempts=1`,
/// and `retries: 2` on one that fails prints `FAILED - RETRYING ... (2 retries left)` then
/// `(1 retries left)` and fails with `"attempts": 2`.
///
/// With no `retry`, one attempt, and the result comes back unjudged, the way the plugin path
/// has always handed it to the reporting loop. `Ok(None)` is a run stopped while it waited
/// between two attempts: nothing more runs, and the item has no result. `lefts` receives each
/// retry line.
#[expect(
    clippy::too_many_arguments,
    reason = "run_plugin_item's arguments, plus the retry plan and the lines it prints"
)]
pub(in crate::executor) async fn run_plugin_attempts<C: AgentChannel, R: Relink<C>>(
    link: &mut C,
    relink: &mut R,
    host: &str,
    batch_id: &mut u64,
    start: &PluginStart<'_>,
    task: &PlayTask,
    item: &Item,
    retry: Option<&Retry>,
    union: Option<&Union>,
    interpreters: &[String],
    asked: Option<&str>,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    logs: &mut Vec<String>,
    warnings: &mut Vec<String>,
    lefts: &mut Vec<RetryLine>,
) -> Result<Option<TaskResult>, Result<BatchOutcome, String>> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        // The warnings of the attempt whose result is shown, not one copy per attempt.
        warnings.clear();
        let raw = {
            let mut plugin = crate::action_plugins::start(
                start.kind,
                crate::action_plugins::Context {
                    args: &item.args,
                    args_untrusted: &item.args_untrusted,
                    running_vars: start.running_vars,
                    delegated: start.delegated,
                    escalated: start.escalated,
                    local: start.local,
                    item_vars: &item.vars,
                    templar: start.templar,
                    origin: start.origin,
                    playbook_dir: start.playbook_dir,
                    warnings: &mut *warnings,
                    environment: Some(&item.environment),
                    timeout: task.timeout,
                    check_mode: item.check_mode == Some(true),
                    diff: item.diff == Some(true),
                    async_val: task.async_val,
                },
            );
            run_plugin_item(
                link,
                relink,
                host,
                batch_id,
                plugin.as_mut(),
                task,
                item,
                union,
                interpreters,
                asked,
                stop,
                stop_broken,
                logs,
            )
            .await?
        };
        // A job an action plugin started is waited for as any other, before `until` reads it.
        let raw = if polls(task) {
            match poll_job(
                link,
                host,
                batch_id,
                task,
                item,
                raw,
                stop,
                stop_broken,
                logs,
                start.tx,
                start.index,
            )
            .await
            {
                Polled::Done(result) => result,
                Polled::Stopped => return Ok(None),
                Polled::Ended(ended) => return Err(ended),
            }
        } else {
            raw
        };
        let Some(retry) = retry else {
            return Ok(Some(raw));
        };
        match judge_attempt(task, item, raw, attempt, retry, start.templar) {
            Attempt::Done(r) => return Ok(Some(r)),
            // Shown as it happens when a job is waited for: its `ASYNC` lines came out as they
            // were decided, and the reference prints each attempt's retry line before the next.
            Attempt::Again(left, failed) if polls(task) => {
                announce_retry(
                    start.tx,
                    host,
                    start.index,
                    crate::executor::prepare::retry_name(task, &item.vars, start.templar),
                    task,
                    left,
                    failed,
                )
                .await;
            }
            Attempt::Again(left, failed) => lefts.push((left, failed)),
        }
        if let Some(failed) = retry.sleep_failure() {
            return Ok(Some(failed));
        }
        if wait_or_stop(retry.delay, stop, stop_broken).await.is_none() {
            return Ok(None);
        }
    }
}

/// Waits `delay`, or gives up when the run is interrupted, before it or during it. A zero delay
/// still looks at the stop first, so a retry loop with no delay stops between two attempts
/// rather than starting the next one.
pub(in crate::executor) async fn wait_or_stop(
    delay: Duration,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
) -> Option<()> {
    // `sleep` rather than a sum with `now`, which would overflow for a delay that large.
    let sleep = tokio::time::sleep(delay);
    tokio::pin!(sleep);
    loop {
        if *stop.borrow() {
            return None;
        }
        if delay.is_zero() {
            return Some(());
        }
        tokio::select! {
            () = &mut sleep => return Some(()),
            res = stop.changed(), if !*stop_broken => {
                if res.is_err() {
                    *stop_broken = true;
                } else {
                    return None;
                }
            }
        }
    }
}

/// One item of one task, as the agent is asked to run it.
///
/// `python` is the module's half of a payload together with the interpreter chosen for the host
/// this item is going to. The two arrive as one because this is where the wire payload is built,
/// and a payload is only ever built where both halves are in hand: the module's half is the same
/// on every host, the interpreter is not, and one sent under an interpreter nobody chose runs
/// something other than what was asked for.
pub(in crate::executor) fn protocol_task(
    task: &PlayTask,
    item: &Item,
    python: Option<(&ModulePayload, &str)>,
) -> Task {
    let mut args = item.args.clone();
    // The terminal is the controller's to give, never a task's to ask for. `raw` is the one
    // playbook module the reference runs through `ssh -tt`: it sends no stdin.
    args.remove(TTY_KEY);
    if short_name(&task.module) == "raw" {
        args.insert(TTY_KEY.into(), Value::Bool(true));
    }
    // What the reference's `async_status` action plugin adds, whatever the task wrote there.
    if short_name(&task.module) == "async_status" {
        args.insert("_async_dir".into(), Value::String(async_dir(&item.vars)));
    }
    Task {
        module: task.module.clone(),
        args: with_modes(item, args),
        // An item is not the task: a failing item never stops the ones behind it, exactly as in
        // Ansible. The task's own failure is decided later, by `report_task`, from every item's
        // result.
        ignore_errors: item.ignore_errors.unwrap_or_else(|| task.ignores_errors())
            || task.loop_items.is_some(),
        timeout: task.timeout,
        environment: item.environment.clone(),
        // One payload for every item of the task: a loop varies the arguments, never the
        // module, and the arguments travel outside the blob.
        payload: python.map(|(module, interpreter)| module.under(interpreter)),
        files: Vec::new(),
        force_python: python.is_some_and(|(module, _)| module.force_python),
        job: (task.async_val > 0).then(|| volant_protocol::AsyncJob {
            limit: task.async_val,
            dir: async_dir(&item.vars),
        }),
    }
}

/// `args` with what ansible-core's `_update_module_args` adds for the task's check and diff
/// modes, sent only when on. Whatever the playbook wrote under those names goes, as the reference
/// overwrites it: a task cannot talk a module out of check mode through its arguments.
pub(in crate::executor::run) fn with_modes(
    item: &Item,
    mut args: Map<String, Value>,
) -> Map<String, Value> {
    for (key, on) in [(CHECK_MODE_ARG, item.check_mode), (DIFF_ARG, item.diff)] {
        args.remove(key);
        if on == Some(true) {
            args.insert(key.into(), Value::Bool(true));
        }
    }
    args
}
