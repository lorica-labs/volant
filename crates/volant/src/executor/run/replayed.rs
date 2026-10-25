// SPDX-License-Identifier: GPL-3.0-or-later
//! Tasks answered from a recording instead of a host, for `volant shadow replay`: the same
//! results the batch and plugin paths hand back, judged the same way.

use volant_protocol::{BatchOutcome, TaskResult};

use crate::executor::RunOptions;
use crate::executor::prepare::Item;
use crate::playbook::PlayTask;
use crate::shadow::replay::{Replay, Seen};
use crate::template::Templar;

use super::retry::{Attempt, Retry, RetryLine, judge_attempt};

/// One step of a replay: the task, which time it runs on the host, and the replay answering.
pub(in crate::executor) struct Replayed<'a> {
    pub(in crate::executor) replay: &'a Replay,
    pub(in crate::executor) host: &'a str,
    pub(in crate::executor) task: &'a PlayTask,
    pub(in crate::executor) occurrence: usize,
}

/// What a step sends, or under a replay, which sends nothing, a step that needs no interpreter
/// and no payload.
pub(in crate::executor) fn sent<T, E>(
    python: Result<Option<T>, E>,
    options: &RunOptions,
) -> Result<Option<T>, E> {
    if options.replay.is_some() {
        Ok(None)
    } else {
        python
    }
}

/// A flat batch answered from the recording, shaped as [`super::run_agent_batch`] returns it:
/// `origin` maps each answer to its step in `steps` and its item. A host the recording has as
/// unreachable ends the batch there.
pub(in crate::executor) fn batch(
    replay: &Replay,
    seen: Option<&Seen>,
    host: &str,
    steps: &[(usize, &PlayTask)],
    origin: &[(usize, usize)],
) -> (Vec<Option<TaskResult>>, Result<BatchOutcome, String>) {
    let mut flat = Vec::new();
    for &(step, item) in origin {
        let (pos, task) = steps[step];
        let occurrence = Seen::of(seen, pos);
        let replayed = Replayed {
            replay,
            host,
            task,
            occurrence,
        };
        let (mut one, outcome) = replayed.attempt(item, 1);
        flat.push(one.pop().flatten());
        if outcome.is_err() {
            return (flat, outcome);
        }
    }
    (flat, Ok(BatchOutcome::Completed))
}

/// Under a replay, the recorded result of a `pause`, which neither waits nor asks: the answer a
/// prompt got is the one the recording holds.
pub(in crate::executor) fn paused(
    options: &RunOptions,
    seen: Option<&Seen>,
    host: &str,
    pos: usize,
    task: &PlayTask,
    item: usize,
) -> Option<TaskResult> {
    if volant_protocol::modules::short_name(&task.module) != "pause" {
        return None;
    }
    let replayed = Replayed::reach(options, seen, host, pos, task)?;
    Some(
        replayed
            .answer(item, 1)
            .unwrap_or_else(TaskResult::failed_with),
    )
}

impl<'a> Replayed<'a> {
    /// Step `pos`, `task`, under a replay, with the occurrence [`Seen::mark`] gave it.
    pub(in crate::executor) fn reach(
        options: &'a RunOptions,
        seen: Option<&Seen>,
        host: &'a str,
        pos: usize,
        task: &'a PlayTask,
    ) -> Option<Self> {
        let replay = options.replay.as_deref()?;
        let occurrence = Seen::of(seen, pos);
        Some(Replayed {
            replay,
            host,
            task,
            occurrence,
        })
    }

    /// The answer of one attempt, shaped as [`super::run_agent_batch`] returns one task's.
    pub(in crate::executor) fn attempt(
        &self,
        item: usize,
        attempt: u32,
    ) -> (Vec<Option<TaskResult>>, Result<BatchOutcome, String>) {
        match self.answer(item, attempt) {
            Ok(result) => (vec![Some(result)], Ok(BatchOutcome::Completed)),
            Err(unreachable) => (vec![None], Err(unreachable)),
        }
    }

    /// An item's attempts, as [`super::run_plugin_attempts`] runs them: the raw answer without
    /// `retries`, the judged one with it, the failed attempts' lines in `lefts`.
    pub(in crate::executor) fn attempts(
        &self,
        item: usize,
        unit: &Item,
        retry: Option<&Retry>,
        templar: &Templar,
        lefts: &mut Vec<RetryLine>,
    ) -> Result<Option<TaskResult>, Result<BatchOutcome, String>> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let raw = self.answer(item, attempt).map_err(Err)?;
            let Some(retry) = retry else {
                return Ok(Some(raw));
            };
            match judge_attempt(self.task, unit, raw, attempt, retry, templar) {
                Attempt::Done(result) => return Ok(Some(result)),
                Attempt::Again(left, failed) => lefts.push((left, failed)),
            }
            if let Some(failed) = retry.sleep_failure() {
                return Ok(Some(failed));
            }
        }
    }

    fn answer(&self, item: usize, attempt: u32) -> Result<TaskResult, String> {
        self.replay.answer(
            self.host,
            self.task,
            self.occurrence,
            item,
            attempt as usize,
        )
    }
}
