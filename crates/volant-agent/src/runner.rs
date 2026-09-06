// SPDX-License-Identifier: GPL-3.0-or-later
//! Runs the tasks of one batch in order and streams results back.

use std::io;
use std::sync::mpsc::{Receiver, TryRecvError};

use volant_protocol::{BatchOutcome, FromAgent, Task, ToAgent};

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
    let cancelled = || is_cancelled(control, id);
    for (index, task) in tasks.iter().enumerate() {
        if cancelled() {
            return send(&FromAgent::BatchDone {
                batch: id,
                outcome: BatchOutcome::Cancelled { at: index },
            });
        }
        let result = match modules::run(task, &cancelled) {
            Run::Done(result) => result,
            Run::Cancelled => {
                return send(&FromAgent::BatchDone {
                    batch: id,
                    outcome: BatchOutcome::Cancelled { at: index },
                });
            }
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

/// A `Cancel` for this batch, a controller that went away, or a broken stdin, all stop the
/// batch. The controller sends one batch at a time, so any other message here is a protocol
/// error and is dropped.
fn is_cancelled(control: &Receiver<io::Result<ToAgent>>, id: u64) -> bool {
    match control.try_recv() {
        Ok(Ok(ToAgent::Cancel { id: cancelled })) => cancelled == id,
        Ok(Ok(_)) => false,
        Ok(Err(_)) => true,
        Err(TryRecvError::Empty) => false,
        Err(TryRecvError::Disconnected) => true,
    }
}
