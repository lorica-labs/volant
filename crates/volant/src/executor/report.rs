// SPDX-License-Identifier: GPL-3.0-or-later
//! Turning one task's results into the events the coordinator shows.

use serde_json::Value;
use tokio::sync::mpsc;
use volant_protocol::TaskResult;

use crate::playbook::PlayTask;
use crate::render::Dump;

use super::coordinator::Event;
use super::run::{classify, empty_loop_result, registered_value};

/// Sends the result lines of one task and its `TaskDone`. Returns the result a rescue would be
/// given as `ansible_failed_result` when the task failed for good, and `None` when it did not:
/// for a loop that is the aggregate, `results` and all, as the reference hands it over.
///
/// `labels` is parallel to `results`: each item's display label, honouring a custom
/// `loop_control.label` template where the playbook gave one. `rescuable` says whether a
/// `rescue` around this step takes a failure here, which is what tells a `fatal:` line that
/// counts `failed` from one that counts `rescued`.
///
/// `retries` is parallel to `results` too: the `retries left` counts of the attempts that item
/// needed, in order. They are sent from here rather than as they happen because the reference
/// prints each item's retry lines directly in front of that item's own result line - measured
/// with a loop whose first item passed and whose second needed two attempts.
///
/// `names` is parallel to `results` as well: each item's task name, already templated. The
/// `FAILED - RETRYING` line shows it rather than the raw `task.name` - measured, a templated name
/// like `probe {{ n }}` renders there the way it does everywhere else.
#[expect(
    clippy::too_many_arguments,
    reason = "three lists parallel to results: labels, retries and names; folding them into a struct hides which is parallel to what"
)]
pub(super) async fn report_task(
    tx: &mpsc::Sender<Event>,
    host: &str,
    index: usize,
    task: &PlayTask,
    results: &[(Option<Value>, TaskResult)],
    labels: &[Option<String>],
    retries: &[Vec<u32>],
    names: &[String],
    dump: Dump,
    rescuable: bool,
    delegate: Option<&str>,
) -> Option<TaskResult> {
    let is_loop = task.loop_items.is_some();
    let censored = task.censors();
    let mut any_failed = false;
    let mut first_failure: Option<TaskResult> = None;
    for (i, (_element, r)) in results.iter().enumerate() {
        for left in retries.get(i).into_iter().flatten() {
            let _ = tx
                .send(Event::Retrying {
                    host: host.to_string(),
                    index,
                    name: names.get(i).cloned().unwrap_or_else(|| task.name.clone()),
                    left: *left,
                })
                .await;
        }
        let outcome = classify(r, task.ignores_errors(), rescuable);
        if r.failed() && first_failure.is_none() {
            first_failure = Some(r.clone());
        }
        any_failed |= r.failed();
        let label = labels.get(i).cloned().flatten();
        let _ = tx
            .send(Event::Result {
                host: host.to_string(),
                index,
                label,
                outcome,
                result: r.clone(),
                dump,
                show: true,
                counts: !is_loop,
                censored,
                delegate: delegate.map(str::to_string),
            })
            .await;
    }
    let mut failure = first_failure;
    if is_loop {
        // A loop over an empty list has no item lines to carry it, so the aggregate is the whole
        // display: one `skipping` line for the task.
        let (aggregate, show) = if results.is_empty() {
            (empty_loop_result(), true)
        } else {
            let aggregate = match registered_value(task, results) {
                Value::Object(mut m) => {
                    m.remove("results");
                    TaskResult(m)
                }
                _ => TaskResult::default(),
            };
            // The reference prints no aggregate line for a loop that had items: the failing
            // item's own line carries the message, and `...ignoring` follows the items alone.
            (aggregate, false)
        };
        let outcome = classify(&aggregate, task.ignores_errors(), rescuable);
        // Measured on ansible-core 2.19.12: a rescue reading `ansible_failed_result.results`
        // after a loop whose second item failed sees all three items. The aggregate is the
        // whole task's result, which is what the reference hands over - an item's own result
        // would lose the ones behind it.
        if any_failed {
            failure = Some(aggregate.clone());
        }
        let _ = tx
            .send(Event::Result {
                host: host.to_string(),
                index,
                label: None,
                outcome,
                result: aggregate,
                dump: Dump::No,
                show,
                counts: true,
                censored,
                delegate: delegate.map(str::to_string),
            })
            .await;
    }
    let _ = tx
        .send(Event::TaskDone {
            host: host.to_string(),
            index,
        })
        .await;
    if any_failed && !task.ignores_errors() {
        failure.or_else(|| Some(TaskResult::default()))
    } else {
        None
    }
}
