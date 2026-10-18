// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::Duration;

use serde_json::{Value, json};
use volant_protocol::TaskResult;

use crate::executor::prepare::Item;
use crate::playbook::PlayTask;
use crate::template::{Templar, TemplateError};
use crate::vars::HostVars;

use super::judge::{all_hold, apply_conditions, condition_vars, settle_failure, timed_out};

/// What `until`, `retries` and `delay` ask of one task, rendered against its own variables.
///
/// The rule is ansible-core 2.19.12's `TaskExecutor._execute` (`task_executor.py`, the attempt
/// loop): `retries: R` is **R + 1 runs**, the first plus R retries, and `until` with no `retries`
/// is R = 3. A retry line and the `delay` follow each of the first R failed runs, never the last;
/// the line after run `i` says `(R + 1 - i retries left)`, so the last line says `(1 retries left)`
/// and is followed by one more run. A run out of attempts reports `attempts: R`, not R + 1.
/// Measured there on the dev machine: `retries: 2` on a failing `shell` that appends a line to a
/// file prints both retry lines, reports `"attempts": 2`, and leaves **three** lines in the file.
/// Also measured: `retries` with no `until` retries while the result is failed; `retries` under 1
/// (0 and -1) turns the whole machinery off, so the task runs once, prints no retry line, carries
/// no `attempts` and is not failed by a condition that never held; the default `delay` is five
/// seconds.
#[derive(Clone)]
pub(in crate::executor) struct Retry {
    /// R: the runs after the first, never zero.
    pub(in crate::executor) retries: u32,
    pub(in crate::executor) delay: Duration,
    /// The conditions that end the loop. Empty means "the result did not fail".
    until: Vec<String>,
    /// What `time.sleep(delay)` raises in the reference, for a delay it refuses.
    unsleepable: Option<&'static str>,
}

/// `time.sleep` on a value Python's clock, nanoseconds in a signed 64-bit integer, cannot hold.
pub(super) const PYTIME_OUT_OF_RANGE: &str = "Task failed: timestamp out of range for C PyTime_t";

/// `time.sleep(float('nan'))`.
const SLEEP_NAN: &str = "Task failed: Invalid value NaN (not a number)";

impl Retry {
    /// The failure the reference's wait between two attempts raises for a `delay` its
    /// `time.sleep` refuses, in place of the wait: the item ends there, after the retry line.
    /// Measured on ansible-core 2.19.12: `retries: 1` on a failing `command` with `delay: 1e20`
    /// prints `(1 retries left)`, then fails the item with `{"changed": false, "msg": "Task
    /// failed: timestamp out of range for C PyTime_t"}`, and `delay: nan` with `Task failed:
    /// Invalid value NaN (not a number)`.
    pub(in crate::executor) fn sleep_failure(&self) -> Option<TaskResult> {
        self.unsleepable.map(|msg| {
            let mut failed = TaskResult::failed_with(msg);
            failed.0.insert("changed".into(), json!(false));
            failed
        })
    }
}

/// `item` is the one whose variables `retries` and `delay` are rendered against.
pub(super) fn retry_plan(
    task: &PlayTask,
    item: Option<&Item>,
    templar: &Templar,
) -> Result<Option<Retry>, TaskResult> {
    let empty = HostVars::default();
    let vars = item.map_or(&empty, |i| &i.vars);
    // `retries` is the reference's `isa='int'`, which refuses a value it would truncate (its
    // `Decimal` check: `'2.5'` and `2.5` fail, `'2.0'` and `true` pass), and `delay` its
    // `isa='float'`. Measured on ansible-core 2.19.12, both messages included.
    let number = |raw: &Value, keyword: &'static str, isa: &str| -> Result<f64, TaskResult> {
        let rendered = templar
            .render_value(raw, vars)
            .map_err(|e| raised(&format!("Task failed: {}", e.0), Vec::new()))?;
        let n = match &rendered {
            Value::Bool(b) => Some(f64::from(u8::from(*b))),
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        };
        n.filter(|n| isa == "float" || (n.is_finite() && n.fract() == 0.0))
            .ok_or_else(|| {
                let keyword_error = format!("Error processing keyword '{keyword}'.");
                let value_error = format!(
                    "The value {} could not be converted to '{isa}'.",
                    crate::playbook::python_repr(&rendered)
                );
                let at = task.value_origins.get(keyword);
                // Measured on ansible-core 2.19.12: the keyword's cause points at the keyword's
                // value, and the conversion's at where the value was written - the same place
                // for a literal (`delay: x`). A value a template produced was written somewhere
                // this engine does not track, so its sentence joins the keyword's, as the
                // reference joins a cause that has no origin of its own.
                let literal = !matches!(raw, Value::String(s) if Templar::is_template(s));
                let causes = if literal {
                    vec![
                        crate::render::cause(&keyword_error, at, None),
                        crate::render::cause(&value_error, at, None),
                    ]
                } else {
                    vec![crate::render::cause(
                        &crate::render::concat_message(&keyword_error, &value_error),
                        at,
                        None,
                    )]
                };
                raised(
                    &format!(
                        "Task failed: {}",
                        crate::render::concat_message(&keyword_error, &value_error)
                    ),
                    causes,
                )
            })
    };
    let retries = match &task.retries {
        Some(raw) => {
            let n = number(raw, "retries", "int")?;
            if n < 1.0 {
                return Ok(None);
            }
            n as u32
        }
        None if task.until.is_empty() => return Ok(None),
        None => 3,
    };
    // `if delay < 0: delay = 1` in ansible-core 2.19.12 `task_executor.py`, then `time.sleep`,
    // which refuses NaN and anything its nanosecond clock cannot hold.
    let (delay, unsleepable) = match &task.delay {
        Some(raw) => match number(raw, "delay", "float")? {
            d if d.is_nan() => (Duration::MAX, Some(SLEEP_NAN)),
            d if d < 0.0 => (Duration::from_secs(1), None),
            d if d * 1e9 > i64::MAX as f64 => (Duration::MAX, Some(PYTIME_OUT_OF_RANGE)),
            d => (Duration::from_secs_f64(d), None),
        },
        None => (Duration::from_secs(5), None),
    };
    Ok(Some(Retry {
        retries,
        delay,
        until: task.until.clone(),
        unsleepable,
    }))
}

/// One item's retry plan, or the failed result it reports instead of running: its `retries` or
/// `delay` did not render, or the task's `timeout` is out of range.
pub(in crate::executor) type ItemRetry = Result<Option<Retry>, TaskResult>;

/// The largest `timeout` the reference accepts (`_alarm_timeout.py`, `_MAX_TIMEOUT`).
const MAX_TIMEOUT: u64 = 100_000_000;

/// Each item's retry plan, rendered against its own variables: the reference post-validates
/// `retries` and `delay` once per item. An item whose render fails reports that failure and the
/// other items run: measured on ansible-core 2.19.12, a loop with `retries: "{{ item.r }}"` over
/// `r: 1`, `r: 2`, `r: x` and `r: 0` retries the first once and the second twice, fails the
/// third alone with `Task failed: Error processing keyword 'retries': The value 'x' could not be
/// converted to 'int'.`, and runs the fourth once with no `attempts`. A skipped item renders
/// nothing.
///
/// A `timeout` over [`MAX_TIMEOUT`] fails each item that runs, after the retry plan rendered, as
/// the reference's alarm refuses it at the first attempt: measured on ansible-core 2.19.12,
/// `timeout: 100000001` fails every item of a loop with `Task failed: Timeout 100000001 is
/// invalid, it must be between 0 and 100000000.` and runs nothing. Every deadline behind this
/// point is then a sum that fits.
pub(in crate::executor) fn retry_plans(
    task: &PlayTask,
    items: &[Item],
    templar: &Templar,
) -> Vec<ItemRetry> {
    items
        .iter()
        .map(|item| {
            if item.skipped.is_some() {
                return Ok(None);
            }
            let plan = retry_plan(task, Some(item), templar)?;
            match task.timeout {
                Some(t) if t > MAX_TIMEOUT => Err(raised(
                    &format!(
                        "Task failed: Timeout {t} is invalid, it must be between 0 and {MAX_TIMEOUT}."
                    ),
                    Vec::new(),
                )),
                _ => Ok(plan),
            }
        })
        .collect()
}

/// A controller-side task its `timeout` ended. Measured on ansible-core 2.19.12: its error block
/// chains `Timed out after N second(s).` behind `Task failed.`, where a module's timeout prints
/// one line.
pub(in crate::executor) fn local_timed_out(seconds: u64) -> TaskResult {
    let mut r = TaskResult::timed_out(seconds);
    r.0.insert(
        crate::render::CHAIN_KEY.into(),
        json!([crate::render::cause(
            &format!("Timed out after {seconds} second(s)."),
            None,
            None
        )]),
    );
    r
}

/// A failure the executor raised rather than a module returned: `changed: false` and the message,
/// measured on ansible-core 2.19.12, and the causes its error block chains, if any.
pub(super) fn raised(msg: &str, causes: Vec<Value>) -> TaskResult {
    let mut failed = TaskResult::failed_with(msg);
    failed.0.insert("changed".into(), json!(false));
    if !causes.is_empty() {
        failed
            .0
            .insert(crate::render::CHAIN_KEY.into(), Value::Array(causes));
    }
    failed
}

/// Whether any item of [`retry_plans`] retries or failed to render, which sends the task down
/// the one-item-at-a-time path.
pub(in crate::executor) fn any_retried(plans: &[ItemRetry]) -> bool {
    plans.iter().any(|plan| !matches!(plan, Ok(None)))
}

/// Whether the attempt loop stops here: the `until` conditions all hold, or - when the task gave
/// none - the result did not fail.
///
/// Measured on ansible-core 2.19.12, both halves: a task whose `failed_when` makes it fail while
/// its `until` holds stops at once (`attempts: 1`, no retry line), and a task whose
/// `changed_when` makes `until: r.changed` false retries although the module itself succeeded.
/// So the conditions decide the result first and `until` reads what they decided, with the
/// registered name bound to it.
pub(super) fn until_holds(
    task: &PlayTask,
    item: &Item,
    result: &TaskResult,
    retry: &Retry,
    templar: &Templar,
) -> Result<bool, TemplateError> {
    if retry.until.is_empty() {
        return Ok(!result.failed());
    }
    all_hold(
        &retry.until,
        &condition_vars(task, item, result, templar),
        templar,
    )
}

/// One `FAILED - RETRYING` line: the count it shows and the attempt that failed.
pub(in crate::executor) type RetryLine = (u32, TaskResult);

/// What one run of a retried task decides.
pub(in crate::executor) enum Attempt {
    /// The loop ends with this result.
    Done(TaskResult),
    /// Another run follows, after a retry line showing this count, then the `delay`. The result
    /// is the failed attempt's, with `attempts` and `retries` on it, for that line's `Result was:`.
    Again(u32, TaskResult),
}

/// Run `attempt` (from 1) of a retried task, judged: `changed_when` and `failed_when` first,
/// `attempts` set so `until` can read it, then `until`. The one place the attempt rule lives, for
/// the controller-side, remote and action-plugin loops alike.
///
/// An `until` that cannot be evaluated ends the task there, with no further run and no
/// `attempts` - measured, and the reference's own prefix for a task that dies rather than fails.
/// Running out is a failed task even when the module passed (measured with `changed_when: false`
/// under `until: r.changed`), and it reports `attempts: R` although R + 1 runs happened.
pub(in crate::executor) fn judge_attempt(
    task: &PlayTask,
    item: &Item,
    raw: TaskResult,
    attempt: u32,
    retry: &Retry,
    templar: &Templar,
) -> Attempt {
    // Out of the whole loop, as the reference's timeout exception is: see `finish`.
    if timed_out(&raw) {
        return Attempt::Done(raw);
    }
    // Before the conditions, which may read it, and again after them, since a condition that
    // could not be evaluated hands back a result of its own.
    let mut raw = raw;
    raw.0.insert("attempts".into(), json!(attempt));
    let mut r = apply_conditions(task, item, raw, templar);
    r.0.insert("attempts".into(), json!(attempt));
    match until_holds(task, item, &r, retry, templar) {
        // Measured on ansible-core 2.19.12: `changed: false`, and a cause pointing at the
        // `until` value.
        Err(e) => {
            let cause = format!("Error while evaluating conditional: {}", e.0);
            return Attempt::Done(raised(
                &conditional_error(&e),
                vec![crate::render::cause(
                    &cause,
                    task.value_origins.get("until"),
                    None,
                )],
            ));
        }
        Ok(true) => {
            settle_failure(&mut r, task);
            return Attempt::Done(r);
        }
        Ok(false) => {}
    }
    if attempt > retry.retries {
        r.0.insert("attempts".into(), json!(retry.retries));
        r.0.insert("failed".into(), json!(true));
        settle_failure(&mut r, task);
        return Attempt::Done(r);
    }
    // `retries` is the reference's count of runs, R + 1, measured in its `Result was:`.
    r.0.insert("retries".into(), json!(retry.retries + 1));
    Attempt::Again(retry.retries + 1 - attempt, r)
}

/// What an `until` that cannot be evaluated reports.
///
/// Measured on ansible-core 2.19.12: `Task failed: Error while evaluating conditional: object of
/// type 'dict' has no attribute 'nosuchkey'`, with no further attempt and no `attempts`. The
/// prefix is the reference's, which is what a playbook testing `'Task failed' in result.msg`
/// reads; the sentence behind it is this engine's own wording for the same fault.
pub(super) fn conditional_error(err: &TemplateError) -> String {
    format!("Task failed: Error while evaluating conditional: {}", err.0)
}
