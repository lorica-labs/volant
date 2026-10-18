// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::Value;
use tokio::sync::watch;
use volant_protocol::{BatchOutcome, FromAgent, Task, TaskResult, ToAgent};

use crate::executor::CANCEL_GRACE;
use crate::python::Union;

use super::blob::{blob_failure, blob_preflight};
use super::channel::AgentChannel;
use super::output::{
    TTY_KEY, add_output_lines, drop_internal_keys, give_terminals, summarise_exception,
};

/// Sends one batch to the agent and collects what comes back, by position in `tasks`.
///
/// Anything the agent asked to show on the way lands in `logs`, already prefixed with the host,
/// for the caller to put on the coordinator's queue under this batch's `no_log`.
#[expect(
    clippy::too_many_arguments,
    reason = "the batch, the link it goes over and the blob it needs there first"
)]
pub(in crate::executor) async fn run_agent_batch<C: AgentChannel>(
    link: &mut C,
    host: &str,
    id: u64,
    tasks: Vec<Task>,
    blob: Option<&Union>,
    files: &[(String, crate::action_plugins::FileBlob)],
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    logs: &mut Vec<String>,
) -> (Vec<Option<TaskResult>>, Result<BatchOutcome, String>) {
    let started = std::time::Instant::now();
    let placed = blob_preflight(link, host, &tasks, blob, files, logs).await;
    if let Some(ledger) = link.timings() {
        ledger.blob_micros += crate::profile::micros(started);
    }
    if let Err(err) = placed {
        return blob_failure(err, tasks.len());
    }
    send_batch(link, host, id, tasks, stop, stop_broken, logs).await
}

/// [`run_agent_batch`] from the point where what the batch needs is on the host.
///
/// A batch with no task in it is not sent: every step of it may have failed on its interpreter
/// already, and an agent answers an empty batch `Completed` with nothing else.
pub(super) async fn send_batch<C: AgentChannel>(
    link: &mut C,
    host: &str,
    id: u64,
    mut tasks: Vec<Task>,
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
    logs: &mut Vec<String>,
) -> (Vec<Option<TaskResult>>, Result<BatchOutcome, String>) {
    // Asked for only when a task wants a terminal: working the line out can cost an `ssh -G`.
    let wants = |t: &Task| t.args.get(TTY_KEY) == Some(&Value::Bool(true));
    let close = if tasks.iter().any(wants) {
        link.tty_close().await
    } else {
        None
    };
    give_terminals(&mut tasks, close);
    let mut received: Vec<Option<TaskResult>> = vec![None; tasks.len()];
    if tasks.is_empty() {
        return (received, Ok(BatchOutcome::Completed));
    }
    let started = std::time::Instant::now();
    let modules: Vec<String> = tasks.iter().map(|t| t.module.clone()).collect();
    // The agent's own time, taken off the round trip so what is left is the wire's.
    let mut agent: u64 = 0;
    if let Err(err) = link.ask(&ToAgent::RunBatch { id, tasks }).await {
        return (received, Err(format!("sending batch: {err}")));
    }
    let ended = loop {
        let msg = tokio::select! {
            msg = link.answer() => msg,
            res = stop.changed(), if !*stop_broken => {
                if res.is_err() {
                    *stop_broken = true;
                    continue;
                }
                link.stop_batch(id, CANCEL_GRACE).await;
                break Ok(BatchOutcome::Cancelled { at: 0 });
            }
        };
        match msg {
            // A frame of another batch is a late answer to one this link gave up on, and
            // belongs to nothing awaited here.
            Ok(Some(FromAgent::TaskResult { batch, .. } | FromAgent::BatchDone { batch, .. }))
                if batch != id => {}
            Ok(Some(FromAgent::TaskResult {
                index,
                mut result,
                ran,
                ..
            })) => {
                if let Some(ledger) = link.timings() {
                    // The host's own figure, so added without trusting it to fit.
                    agent = agent.saturating_add(ran.as_ref().map_or(0, |r| r.micros));
                    let module = modules.get(index).cloned().unwrap_or_default();
                    ledger.ran.push((index, module, ran));
                }
                if let Some(slot) = received.get_mut(index) {
                    drop_internal_keys(&mut result);
                    add_output_lines(&mut result);
                    summarise_exception(&mut result);
                    *slot = Some(result);
                }
            }
            // Only `stop_batch` above asks for a cancel, and it consumes the answer. One that
            // arrives here was never asked for: an agent whose stdin reader died reads its own
            // end of input as a cancel.
            Ok(Some(FromAgent::BatchDone {
                outcome: BatchOutcome::Cancelled { .. },
                ..
            })) => break Err("the agent cancelled a batch nobody cancelled".to_string()),
            Ok(Some(FromAgent::BatchDone { outcome, .. })) => break Ok(outcome),
            // Collected rather than printed: a line the agent asked to show belongs on the
            // coordinator's queue with everything else this batch shows, which is the only
            // place the `no_log` policy can see it. `FromAgent::Log` carries no task index, so
            // the caller decides for the whole batch.
            Ok(Some(FromAgent::Log { message, .. })) => logs.push(format!("[{host}] {message}")),
            Ok(Some(
                FromAgent::Ready { .. }
                | FromAgent::BlobState { .. }
                | FromAgent::PutFileDone { .. },
            )) => {}
            Ok(None) => {
                break Err(format!(
                    "agent stopped before the batch finished{}",
                    link.stderr_note()
                ));
            }
            Err(err) => break Err(format!("reading from the agent: {err}")),
        }
    };
    if let Some(ledger) = link.timings() {
        ledger.wire_micros += crate::profile::micros(started).saturating_sub(agent);
    }
    (received, ended)
}
