// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::watch;
use volant_protocol::{BatchOutcome, Task, TaskResult};

use crate::executor::prepare::Item;
use crate::playbook::PlayTask;
use crate::vars::HostVars;

use super::batch::run_agent_batch;
use super::channel::AgentChannel;
use super::output::python_falsy;
use super::plugin::wait_or_stop;

/// The key a started job's id comes back under, in a result rather than among a host's variables.
const JOB_ID: &str = "ansible_job_id";

/// Seconds between two looks at a job when `poll` is not written: the reference's
/// `DEFAULT_POLL_INTERVAL`.
const DEFAULT_POLL: u64 = 15;

/// Whether this task's job is waited for here rather than only started.
pub(in crate::executor) fn polls(task: &PlayTask) -> bool {
    task.async_val > 0 && task.poll.unwrap_or(DEFAULT_POLL) > 0
}

/// The failure of a task that asks for `async` from an action that does not support it, as
/// ansible-core 2.19.12's `ActionBase.run` raises it, when the task runs and per host. `async: 0`
/// runs the task as any other.
pub(super) fn async_refusal(task: &PlayTask) -> Option<TaskResult> {
    if task.async_val == 0 || crate::action_plugins::supports_async(&task.module) {
        return None;
    }
    let mut refused = TaskResult::failed_with(format!(
        "This action ({}) does not support async.",
        task.module
    ));
    refused.0.insert("changed".into(), Value::Bool(false));
    refused
        .0
        .insert("exception".into(), json!("(traceback unavailable)"));
    Some(refused)
}

/// The `FAILED - RETRYING` line of an attempt, sent at once rather than with the task's result.
pub(in crate::executor) async fn announce_retry(
    tx: &tokio::sync::mpsc::Sender<crate::executor::coordinator::Event>,
    host: &str,
    index: usize,
    name: String,
    task: &PlayTask,
    left: u32,
    result: TaskResult,
) {
    let _ = tx
        .send(crate::executor::coordinator::Event::Retrying {
            host: host.to_string(),
            index,
            name,
            left,
            result,
            censored: task.censors(),
        })
        .await;
}

/// What waiting for a started job came to.
pub(in crate::executor) enum Polled {
    /// The job's last status, or the failure the reference makes of a job that did not finish.
    Done(TaskResult),
    /// The run was interrupted between two looks.
    Stopped,
    /// A look went out in a batch that ended without its result: the link was lost.
    Ended(Result<BatchOutcome, String>),
}

/// A started job, asked about every `poll` seconds until it says it finished or `async` seconds
/// are spent, then its status file removed: ansible-core 2.19.12's
/// `TaskExecutor._poll_async_result`, one `async_status` batch per look over the task's own link.
/// The `ASYNC POLL`, `ASYNC OK` and `ASYNC FAILED` lines go on the queue as they are decided.
#[expect(
    clippy::too_many_arguments,
    reason = "the job, the link its looks go over, and the queue its lines go on"
)]
#[expect(clippy::too_many_lines)]
pub(in crate::executor) async fn poll_job<C: AgentChannel>(
    link: &mut C,
    host: &str,
    batch_id: &mut u64,
    task: &PlayTask,
    item: &Item,
    started: TaskResult,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    logs: &mut Vec<String>,
    tx: &tokio::sync::mpsc::Sender<crate::executor::coordinator::Event>,
    index: usize,
) -> Polled {
    let truthy =
        |result: &TaskResult, key: &str| result.0.get(key).is_some_and(|v| !python_falsy(v));
    // A start that failed or was skipped is the task's result, with no line: only a started
    // job is polled.
    if truthy(&started, "failed") || truthy(&started, "skipped") {
        return Polled::Done(started);
    }
    let show = |line: String| {
        tx.send(crate::executor::coordinator::Event::Display {
            host: host.to_string(),
            index,
            line,
            debug: true,
        })
    };
    let Some(jid) = started
        .0
        .get(JOB_ID)
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        let _ = show(format!("ASYNC FAILED on {host}: jid=None")).await;
        return Polled::Done(TaskResult::failed_with(
            "No job id was returned by the async task",
        ));
    };
    let ask = |mode: &str| {
        let mut args = Map::new();
        args.insert("jid".into(), Value::String(jid.clone()));
        args.insert("mode".into(), Value::String(mode.into()));
        args.insert("_async_dir".into(), Value::String(async_dir(&item.vars)));
        Task {
            module: "async_status".into(),
            args,
            environment: item.environment.clone(),
            ..Task::default()
        }
    };
    let poll = task.poll.unwrap_or(DEFAULT_POLL);
    let mut left = task.async_val;
    let mut last = None;
    while left > 0 {
        if wait_or_stop(Duration::from_secs(poll), stop, stop_broken)
            .await
            .is_none()
        {
            return Polled::Stopped;
        }
        *batch_id += 1;
        let (mut flat, ended) = run_agent_batch(
            link,
            host,
            *batch_id,
            vec![ask("status")],
            None,
            &[],
            stop,
            stop_broken,
            logs,
        )
        .await;
        if !matches!(
            ended,
            Ok(BatchOutcome::Completed | BatchOutcome::Failed { .. })
        ) {
            return Polled::Ended(ended);
        }
        let Some(status) = flat.pop().flatten() else {
            return Polled::Done(TaskResult::failed_with(format!(
                "the agent answered no status for job {jid}"
            )));
        };
        // Every answer the agent gives is a parsed one, so a failure ends the wait as the
        // reference's does for a result it could parse.
        if ["finished", "failed", "skipped"]
            .iter()
            .any(|key| truthy(&status, key))
        {
            last = Some(status);
            break;
        }
        left = left.saturating_sub(poll);
        let _ = show(format!(
            "ASYNC POLL on {host}: jid={} started={} finished={}",
            python_repr(status.0.get(JOB_ID)),
            python_repr(status.0.get("started")),
            python_repr(status.0.get("finished")),
        ))
        .await;
        last = Some(status);
    }
    let result = match last {
        Some(status) if truthy(&status, "finished") => {
            // Removed the way the reference removes it, its answer unread.
            *batch_id += 1;
            let _ = run_agent_batch(
                link,
                host,
                *batch_id,
                vec![ask("cleanup")],
                None,
                &[],
                stop,
                stop_broken,
                logs,
            )
            .await;
            status
        }
        last => {
            let mut failed = TaskResult::failed_with(format!(
                "async task did not complete within the requested time - {}s",
                task.async_val
            ));
            // The status as the action plugin answered it, before the executor's `changed`
            // default the agent adds: measured, the reference's `async_result` has none.
            let mut status = last.map(|status| status.0).unwrap_or_default();
            if status.get("changed") == Some(&Value::Bool(false)) {
                status.remove("changed");
            }
            failed
                .0
                .insert("async_result".into(), Value::Object(status));
            failed.0.insert("changed".into(), Value::Bool(false));
            failed
        }
    };
    let shown = result.0.get(JOB_ID).or_else(|| {
        result
            .0
            .get("async_result")
            .and_then(|inner| inner.get(JOB_ID))
    });
    let verdict = if truthy(&result, "failed") {
        "FAILED"
    } else {
        "OK"
    };
    let _ = show(format!(
        "ASYNC {verdict} on {host}: jid={}",
        python_repr(shown)
    ))
    .await;
    Polled::Done(result)
}

/// A value as Python's `%s` prints it: `True`, `None`, a string bare.
fn python_repr(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".into(),
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::Bool(false)) => "False".into(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

/// The reference's default `async_dir`, which its `sh` shell plugin reads.
const DEFAULT_ASYNC_DIR: &str = "~/.ansible_async";

/// Where a host keeps its jobs' status files: `ansible_async_dir`, then `ANSIBLE_ASYNC_DIR`,
/// then `async_dir` of `ansible.cfg`, then the default, as the shell plugin's option is
/// resolved. Expanded on the host.
pub(super) fn async_dir(vars: &HostVars) -> String {
    let set = |value: Option<String>| value.filter(|dir| !dir.trim().is_empty());
    set(crate::vars::host_setting(vars, "ansible_async_dir")
        .and_then(Value::as_str)
        .map(str::to_string))
    .or_else(|| set(std::env::var("ANSIBLE_ASYNC_DIR").ok()))
    .or_else(|| set(crate::config::configured_async_dir()))
    .unwrap_or_else(|| DEFAULT_ASYNC_DIR.to_string())
}
