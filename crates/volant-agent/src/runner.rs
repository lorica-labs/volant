// SPDX-License-Identifier: GPL-3.0-or-later
//! Runs the tasks of one batch in order and streams results back.

use std::cell::{Cell, RefCell};
use std::io;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Instant;

use volant_protocol::{BatchOutcome, FromAgent, LogLevel, Task, ToAgent};

use crate::modules::{self, Run};

pub fn run_batch<F>(
    id: u64,
    tasks: &[Task],
    control: &Receiver<io::Result<crate::blobs::Incoming>>,
    send: &mut F,
) -> io::Result<()>
where
    F: FnMut(&FromAgent) -> io::Result<()>,
{
    // `modules::run` only takes a plain `Fn() -> bool`, so a forwarded read error found by
    // `is_cancelled` is stashed here rather than returned; `stop` below turns it back into
    // a logged, non-zero exit instead of a silent `Cancelled` outcome.
    let broken = Cell::new(None);
    // A batch is the unit the python servers' restart allowance is counted in.
    crate::python::batch_started();
    let remote_tmp = crate::blobs::remote_tmp();
    // Shared rather than borrowed twice: `is_cancelled` answers a blob message that arrives
    // mid-batch, which means writing a frame from inside the cancellation check.
    let send = RefCell::new(send);
    let cancelled = || is_cancelled(control, id, &broken, &remote_tmp, &send);
    // The results a later task's `only_if` reads, kept only for those.
    let mut kept: Vec<Option<volant_protocol::TaskResult>> = vec![None; tasks.len()];
    for (index, task) in tasks.iter().enumerate() {
        if cancelled() {
            return stop(id, index, &broken, &send);
        }
        let started = Instant::now();
        let held = task.only_if.as_ref().is_none_or(|only| {
            kept.get(only.task)
                .and_then(Option::as_ref)
                .is_some_and(|r| found(r, &only.checksum))
        });
        let (run, mut ran) = match &task.job {
            _ if !held => (
                Run::Done(volant_protocol::TaskResult(serde_json::Map::from_iter([
                    ("skipped".to_string(), serde_json::Value::Bool(true)),
                    (
                        volant_protocol::NOT_RUN.to_string(),
                        serde_json::Value::Bool(true),
                    ),
                ]))),
                modules::ran_natively(),
            ),
            // The reference's `ActionBase.run` refuses the pair before the module starts.
            Some(_)
                if task.args.get(volant_protocol::modules::CHECK_MODE_ARG)
                    == Some(&serde_json::Value::Bool(true)) =>
            {
                (
                    Run::Done(volant_protocol::TaskResult::failed_with(
                        "check mode and async cannot be used on same task.",
                    )),
                    modules::ran_natively(),
                )
            }
            Some(job) => (
                Run::Done(crate::jobs::start(task, job)),
                modules::ran_natively(),
            ),
            None => modules::run(task, &cancelled),
        };
        let result = match run {
            Run::Done(result) => result,
            Run::Cancelled => return stop(id, index, &broken, &send),
        };
        ran.micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let failed = result.failed();
        if tasks.iter().any(|later| {
            later
                .only_if
                .as_ref()
                .is_some_and(|only| only.task == index)
        }) {
            kept[index] = Some(result.clone());
        }
        (send.borrow_mut())(&FromAgent::TaskResult {
            batch: id,
            index,
            result,
            ran: Some(ran),
        })?;
        if failed && !task.ignore_errors {
            return (send.borrow_mut())(&FromAgent::BatchDone {
                batch: id,
                outcome: BatchOutcome::Failed { at: index },
            });
        }
    }
    (send.borrow_mut())(&FromAgent::BatchDone {
        batch: id,
        outcome: BatchOutcome::Completed,
    })
}

/// Whether `result` is a `stat` that found a regular file whose SHA-1 is `checksum`.
fn found(result: &volant_protocol::TaskResult, checksum: &str) -> bool {
    let stat = result.0.get("stat");
    let field = |key: &str| stat.and_then(|s| s.get(key));
    !result.failed()
        && field("exists") == Some(&serde_json::Value::Bool(true))
        && field("isdir") == Some(&serde_json::Value::Bool(false))
        && field("checksum").and_then(serde_json::Value::as_str) == Some(checksum)
}

/// A `Cancel` for this batch, or a controller that went away, ends the batch normally. A
/// forwarded stdin read error also ends it, but is not a deliberate cancel: `stop` reports
/// it as a logged error instead of `BatchOutcome::Cancelled`.
fn is_cancelled<F>(
    control: &Receiver<io::Result<crate::blobs::Incoming>>,
    id: u64,
    broken: &Cell<Option<io::Error>>,
    remote_tmp: &str,
    send: &RefCell<&mut F>,
) -> bool
where
    F: FnMut(&FromAgent) -> io::Result<()>,
{
    match control.try_recv() {
        Ok(Ok((ToAgent::Cancel { id: cancelled }, _))) => cancelled == id,
        Ok(Ok((other, frame))) => {
            // A blob message is answered here rather than dropped: the controller waits for a
            // `BlobState` after every `put_blob`, and one that arrived mid-batch used to be
            // logged to a stderr it only sometimes sees and then discarded, leaving it waiting
            // for a state that never came.
            match crate::blobs::answer(remote_tmp, &other, &frame, &mut *send.borrow_mut()) {
                Ok(true) => return false,
                Ok(false) => {}
                Err(err) => {
                    broken.set(Some(err));
                    return true;
                }
            }
            // The message's kind, never its contents. The agent's stderr is inherited straight
            // from the controller's, so a `RunBatch` printed whole here would put every
            // argument of every task of that batch on the operator's terminal - and the agent
            // is the one party that cannot know which task said `no_log`.
            let kind = match other {
                ToAgent::Hello { .. } => "hello",
                ToAgent::RunBatch { .. } => "run_batch",
                ToAgent::Cancel { .. } => "cancel",
                ToAgent::HasBlob { .. } => "has_blob",
                ToAgent::PutBlob { .. } => "put_blob",
                ToAgent::PutBlobPart { .. } => "put_blob_part",
                ToAgent::EndBlob { .. } => "end_blob",
                ToAgent::ReadFile { .. } => "read_file",
                ToAgent::PutFile { .. } => "put_file",
                ToAgent::Warm { .. } => "warm",
            };
            eprintln!("volant-agent: ignoring unexpected {kind} during batch {id}");
            false
        }
        Ok(Err(err)) => {
            broken.set(Some(err));
            true
        }
        Err(TryRecvError::Empty) => false,
        Err(TryRecvError::Disconnected) => true,
    }
}

/// Ends the batch at `index`: a stashed read error is logged at `LogLevel::Error` and
/// returned so the process exits non-zero, matching the same error outside a batch;
/// otherwise this is a deliberate cancel and is reported as `BatchOutcome::Cancelled`.
fn stop<F>(
    id: u64,
    index: usize,
    broken: &Cell<Option<io::Error>>,
    send: &RefCell<&mut F>,
) -> io::Result<()>
where
    F: FnMut(&FromAgent) -> io::Result<()>,
{
    if let Some(err) = broken.take() {
        (send.borrow_mut())(&FromAgent::Log {
            level: LogLevel::Error,
            message: format!("reading frame from controller: {err}"),
        })?;
        return Err(err);
    }
    (send.borrow_mut())(&FromAgent::BatchDone {
        batch: id,
        outcome: BatchOutcome::Cancelled { at: index },
    })
}

#[cfg(test)]
mod only_if_tests {
    use serde_json::json;

    /// `only_if` holds for a `stat` of a regular file with that sum, and for nothing else.
    #[test]
    fn the_condition_reads_the_stat_of_a_regular_file() {
        let stat = |value: serde_json::Value| {
            volant_protocol::TaskResult(value.as_object().unwrap().clone())
        };
        let file = json!({"stat": {"exists": true, "isdir": false, "checksum": "ab"}});
        assert!(super::found(&stat(file.clone()), "ab"));
        assert!(!super::found(&stat(file), "cd"));
        assert!(!super::found(
            &stat(json!({"stat": {"exists": false}})),
            "ab"
        ));
        assert!(!super::found(
            &stat(json!({"stat": {"exists": true, "isdir": true}})),
            "ab"
        ));
        assert!(!super::found(
            &stat(
                json!({"failed": true, "stat": {"exists": true, "isdir": false, "checksum": "ab"}})
            ),
            "ab"
        ));
    }
}
