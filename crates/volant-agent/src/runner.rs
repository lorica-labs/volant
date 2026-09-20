// SPDX-License-Identifier: GPL-3.0-or-later
//! Runs the tasks of one batch in order and streams results back.

use std::cell::{Cell, RefCell};
use std::io;
use std::sync::mpsc::{Receiver, TryRecvError};

use volant_protocol::{BatchOutcome, FromAgent, LogLevel, Task, ToAgent};

use crate::modules::{self, Run};

pub fn run_batch<F>(
    id: u64,
    tasks: &[Task],
    control: &Receiver<io::Result<ToAgent>>,
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
    for (index, task) in tasks.iter().enumerate() {
        if cancelled() {
            return stop(id, index, &broken, &send);
        }
        let result = match modules::run(task, &cancelled) {
            Run::Done(result) => result,
            Run::Cancelled => return stop(id, index, &broken, &send),
        };
        let failed = result.failed();
        (send.borrow_mut())(&FromAgent::TaskResult {
            batch: id,
            index,
            result,
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

/// A `Cancel` for this batch, or a controller that went away, ends the batch normally. A
/// forwarded stdin read error also ends it, but is not a deliberate cancel: `stop` reports
/// it as a logged error instead of `BatchOutcome::Cancelled`.
fn is_cancelled<F>(
    control: &Receiver<io::Result<ToAgent>>,
    id: u64,
    broken: &Cell<Option<io::Error>>,
    remote_tmp: &str,
    send: &RefCell<&mut F>,
) -> bool
where
    F: FnMut(&FromAgent) -> io::Result<()>,
{
    match control.try_recv() {
        Ok(Ok(ToAgent::Cancel { id: cancelled })) => cancelled == id,
        Ok(Ok(other)) => {
            // A blob message is answered here rather than dropped: the controller waits for a
            // `BlobState` after every `put_blob`, and one that arrived mid-batch used to be
            // logged to a stderr it only sometimes sees and then discarded, leaving it waiting
            // for a state that never came.
            match crate::blobs::answer(remote_tmp, &other, &mut *send.borrow_mut()) {
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
