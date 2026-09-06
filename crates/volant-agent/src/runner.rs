// SPDX-License-Identifier: GPL-3.0-or-later
//! Runs the tasks of one batch in order and streams results back.

use std::cell::Cell;
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
    let cancelled = || is_cancelled(control, id, &broken);
    for (index, task) in tasks.iter().enumerate() {
        if cancelled() {
            return stop(id, index, &broken, send);
        }
        let result = match modules::run(task, &cancelled) {
            Run::Done(result) => result,
            Run::Cancelled => return stop(id, index, &broken, send),
        };
        let failed = result.failed();
        send(&FromAgent::TaskResult {
            batch: id,
            index,
            result,
        })?;
        if failed && !task.ignore_errors {
            return send(&FromAgent::BatchDone {
                batch: id,
                outcome: BatchOutcome::Failed { at: index },
            });
        }
    }
    send(&FromAgent::BatchDone {
        batch: id,
        outcome: BatchOutcome::Completed,
    })
}

/// A `Cancel` for this batch, or a controller that went away, ends the batch normally. A
/// forwarded stdin read error also ends it, but is not a deliberate cancel: `stop` reports
/// it as a logged error instead of `BatchOutcome::Cancelled`.
fn is_cancelled(
    control: &Receiver<io::Result<ToAgent>>,
    id: u64,
    broken: &Cell<Option<io::Error>>,
) -> bool {
    match control.try_recv() {
        Ok(Ok(ToAgent::Cancel { id: cancelled })) => cancelled == id,
        Ok(Ok(other)) => {
            eprintln!("volant-agent: ignoring unexpected message during batch {id}: {other:?}");
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
fn stop<F>(id: u64, index: usize, broken: &Cell<Option<io::Error>>, send: &mut F) -> io::Result<()>
where
    F: FnMut(&FromAgent) -> io::Result<()>,
{
    if let Some(err) = broken.take() {
        send(&FromAgent::Log {
            level: LogLevel::Error,
            message: format!("reading frame from controller: {err}"),
        })?;
        return Err(err);
    }
    send(&FromAgent::BatchDone {
        batch: id,
        outcome: BatchOutcome::Cancelled { at: index },
    })
}
