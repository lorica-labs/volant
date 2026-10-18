// SPDX-License-Identifier: GPL-3.0-or-later
//! A step's `throttle`: what it says for one host, and the gate that keeps the hosts to it.

use std::sync::Arc;

use serde_json::Value;

use crate::compile::{Step, StepKind};
use crate::executor::prepare::{Gate, Prepared, undefined_name};
use crate::executor::python_type;
use crate::executor::state::Abort;
use crate::playbook::{PlayTask, Throttle, python_repr};
use crate::template::{Templar, TemplateError};
use crate::vars::HostVars;

use super::Driver;

/// A host's place among the `throttle` hosts of a step, given back when it is dropped.
pub(super) struct Turn(Arc<Gate>);

impl Turn {
    /// A turn at `gate` when fewer than `limit` hosts are running there.
    fn take(gate: &Arc<Gate>, limit: usize) -> Option<Self> {
        let mut running = gate.running.lock().expect("throttle count");
        (*running < limit).then(|| {
            *running += 1;
            Self(Arc::clone(gate))
        })
    }
}

/// A turn at `gate`, waiting for one to be given back.
#[cfg(test)]
pub(super) async fn wait_for_turn(gate: &Arc<Gate>, limit: usize) -> Turn {
    loop {
        let freed = gate.freed.notified();
        tokio::pin!(freed);
        freed.as_mut().enable();
        if let Some(turn) = Turn::take(gate, limit) {
            return turn;
        }
        freed.await;
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        *self.0.running.lock().expect("throttle count") -= 1;
        self.0.freed.notify_waiters();
    }
}

/// Why a task's `throttle` gave no number.
pub(super) enum Unconverted {
    /// The task fails on this host, as a keyword the reference cannot convert does.
    Fails(TemplateError),
    /// The run ends: the reference raises from the strategy, with this as its error.
    Aborts(String),
}

/// What a task's `throttle` means for the host rendering `vars`: a limit, or none for a task
/// without one, zero or a negative number. Measured on ansible-core 2.19.12: a string is
/// rendered and read the way `int()` reads it, and `true` is 1; a word, a template that
/// renders to one or to nothing ends the run; a float that is not whole fails the task.
pub(super) fn throttle_of(
    throttle: &Throttle,
    vars: &HostVars,
    templar: &Templar,
) -> Result<Option<u64>, Unconverted> {
    let word = |why: String| Unconverted::Aborts(format!("{CONVERT}{why}"));
    let rendered = match &throttle.value {
        Value::String(raw) => templar
            .render_value(&throttle.value, vars)
            .map_err(|e| word(undefined_name(raw, e.0)))?,
        other => other.clone(),
    };
    let limit = match &rendered {
        Value::Bool(b) => i64::from(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i,
            None => match n.as_f64() {
                Some(f) if f.fract() == 0.0 && f.abs() < 9e15 => f as i64,
                _ => {
                    return Err(Unconverted::Fails(TemplateError(format!(
                        "Task failed: Error processing keyword 'throttle': The value {n} could not be converted to 'int'."
                    ))));
                }
            },
        },
        Value::String(s) => s.trim().parse::<i64>().map_err(|_| {
            word(format!(
                "invalid literal for int() with base 10: {}",
                python_repr(&rendered)
            ))
        })?,
        other => {
            return Err(word(format!(
                "int() argument must be a string, a bytes-like object or a real number, not '{}'",
                python_type(other)
            )));
        }
    };
    Ok(u64::try_from(limit).ok().filter(|limit| *limit > 0))
}

const CONVERT: &str = "Failed to convert the throttle value to an integer: ";

/// The causes under a failed task's error block: the throttle's own, which the reference gives
/// as the keyword and then the conversion, both at the value; any other failure's.
pub(super) fn causes(task: &PlayTask, message: &str) -> Option<Value> {
    let Some(sentence) = message.strip_prefix("Task failed: Error processing keyword 'throttle': ")
    else {
        return super::finalization_causes(task, message);
    };
    let at = task.throttle.as_ref().and_then(|t| t.origin.as_ref());
    Some(serde_json::json!([
        crate::render::cause("Error processing keyword 'throttle'.", at, None),
        crate::render::cause(sentence, at, None)
    ]))
}

impl Driver<'_> {
    /// `prepared` with this host's `throttle` read for it: the limit, if the task has one, or the
    /// failure of this host. `None` when the value ends the run, after the task's banner, which
    /// is the last line the reference prints.
    pub(super) async fn read_throttle(
        &self,
        step: &Step,
        pos: usize,
        prepared: Result<Prepared, TemplateError>,
        templar: &Templar,
        abort: &Abort,
    ) -> Option<(Result<Prepared, TemplateError>, Option<u64>)> {
        let (Ok(ready), Some(asked)) = (&prepared, &step.task.throttle) else {
            return Some((prepared, None));
        };
        let Some(vars) = ready.items().first().map(|i| &i.vars) else {
            return Some((prepared, None));
        };
        if step.kind == StepKind::Meta {
            return Some((prepared, None));
        }
        match throttle_of(asked, vars, templar) {
            Ok(limit) => Some((prepared, limit)),
            Err(Unconverted::Fails(err)) => Some((Err(err), None)),
            Err(Unconverted::Aborts(why)) => {
                self.banner(pos).await;
                let at = asked
                    .origin
                    .as_ref()
                    .map(|o| {
                        format!(
                            "
{}",
                            crate::render::source_context(o)
                        )
                    })
                    .unwrap_or_default();
                abort.raise_plain(format!("{why}{at}"));
                None
            }
        }
    }

    /// The turn that keeps step `at` to `limit` hosts at a time, this host's own reading of its
    /// `throttle`; `Some(None)` for a step with none, and `None` when the run is interrupted
    /// while the host waits for its turn.
    pub(super) async fn throttled(
        &mut self,
        at: usize,
        limit: Option<u64>,
    ) -> Option<Option<Turn>> {
        let Some(limit) = limit.filter(|limit| *limit > 0) else {
            return Some(None);
        };
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        let gate = Arc::clone(
            self.plan
                .throttles
                .lock()
                .expect("throttle lock")
                .entry(at)
                .or_default(),
        );
        loop {
            if self.stopped() {
                return None;
            }
            // Registered before the count is read, so a turn given back in between is not missed.
            let freed = gate.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            if let Some(turn) = Turn::take(&gate, limit) {
                return Some(Some(turn));
            }
            tokio::select! {
                () = freed => {}
                res = self.stop.changed(), if !self.stop_broken => {
                    if res.is_err() {
                        self.stop_broken = true;
                    } else {
                        return None;
                    }
                }
            }
        }
    }
}
