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
/// `ignored` is parallel to `results` too: each item's `ignore_errors` as `prepare` rendered it,
/// where the task wrote a template, and the task's own keyword for an item it is missing. A
/// failure is ignored item by item, and the task fails when one failed item was not.
///
/// `names` is parallel to `results` as well: each item's task name, already templated. The
/// `FAILED - RETRYING` line shows it rather than the raw `task.name` - measured, a templated name
/// like `probe {{ n }}` renders there the way it does everywhere else.
#[expect(
    clippy::too_many_arguments,
    reason = "four lists parallel to results: labels, ignored, retries and names; folding them into a struct hides which is parallel to what"
)]
pub(super) async fn report_task(
    tx: &mpsc::Sender<Event>,
    host: &str,
    index: usize,
    task: &PlayTask,
    results: &[(Option<Value>, TaskResult)],
    labels: &[Option<String>],
    ignored: &[Option<bool>],
    retries: &[Vec<u32>],
    names: &[String],
    dump: Dump,
    rescuable: bool,
    delegate: Option<&str>,
) -> Option<TaskResult> {
    let is_loop = task.loop_items.is_some();
    let censored = task.censors();
    let ignores = |i: usize| {
        ignored
            .get(i)
            .copied()
            .flatten()
            .unwrap_or_else(|| task.ignores_errors())
    };
    // Whether a failed item stands: one that its own `ignore_errors` did not cover.
    let mut failure_stands = false;
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
        let outcome = classify(r, ignores(i), rescuable);
        failure_stands |= r.failed() && !ignores(i);
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
        let outcome = classify(&aggregate, !failure_stands, rescuable);
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
    if failure_stands {
        failure.or_else(|| Some(TaskResult::default()))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::task;
    use super::*;
    use crate::stats::Outcome;
    use serde_json::json;

    /// The task's verdict and each line's outcome, for two failed items of a loop.
    async fn two_failed_items(ignored: &[Option<bool>]) -> (bool, Vec<Outcome>) {
        let mut t = task("command");
        t.loop_items = Some(json!([1, 2]));
        let results: Vec<(Option<Value>, TaskResult)> = [1, 2]
            .into_iter()
            .map(|n| (Some(json!(n)), TaskResult::failed_with("boom")))
            .collect();
        let (tx, mut rx) = mpsc::channel(16);
        let failed = report_task(
            &tx,
            "h1",
            0,
            &t,
            &results,
            &[None, None],
            ignored,
            &[],
            &[],
            Dump::No,
            false,
            None,
        )
        .await
        .is_some();
        drop(tx);
        let mut outcomes = Vec::new();
        while let Some(event) = rx.recv().await {
            if let Event::Result { outcome, .. } = event {
                outcomes.push(outcome);
            }
        }
        (failed, outcomes)
    }

    /// A failure is ignored item by item: an item whose own `ignore_errors` rendered true shows
    /// `...ignoring`, the other fails, and the task fails because one failure stands. When every
    /// failed item is covered, the task and its aggregate line are ignored.
    ///
    /// What would make this red: one verdict read for the whole task, which either ignores the
    /// item that asked not to be or fails the one that asked to be.
    #[tokio::test]
    async fn a_failure_is_ignored_item_by_item() {
        assert_eq!(
            two_failed_items(&[Some(true), Some(false)]).await,
            (
                true,
                vec![Outcome::Ignored, Outcome::Failed, Outcome::Failed]
            )
        );
        assert_eq!(
            two_failed_items(&[Some(true), Some(true)]).await,
            (false, vec![Outcome::Ignored; 3])
        );
        assert_eq!(
            two_failed_items(&[]).await,
            (true, vec![Outcome::Failed; 3]),
            "no verdict falls back to the task's keyword, unset here"
        );
    }
}
