// SPDX-License-Identifier: GPL-3.0-or-later
//! Turning one task's results into the events the coordinator shows.

use serde_json::Value;
use tokio::sync::mpsc;
use volant_protocol::TaskResult;

use crate::playbook::{Flag, PlayTask};
use crate::render::{DIFF_KEY, Dump, ERROR_KEY, ErrorBlock};

use super::coordinator::Event;
use super::run::{RetryLine, classify, empty_loop_result, loops, registered_value};

/// Sends the result lines of one task and its `TaskDone`. Returns the result a rescue would be
/// given as `ansible_failed_result` when the task failed for good, and `None` when it did not:
/// for a loop that is the aggregate, `results` and all, as the reference hands it over.
///
/// `labels` is parallel to `results`: each item's display label, honouring a custom
/// `loop_control.label` template where the playbook gave one. `rescuable` says whether a
/// `rescue` around this step takes a failure here, which is what tells a `fatal:` line that
/// counts `failed` from one that counts `rescued`.
///
/// `retries` is parallel to `results` too: the retry lines of the attempts that item needed, in
/// order. They are sent from here rather than as they happen because the reference
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
    retries: &[Vec<RetryLine>],
    names: &[String],
    dump: Dump,
    rescuable: bool,
    delegate: Option<&str>,
) -> Option<TaskResult> {
    let is_loop = loops(task, results.first().map(|(element, _)| element));
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
    for (i, (element, r)) in results.iter().enumerate() {
        for (left, attempt) in retries.get(i).into_iter().flatten() {
            let _ = tx
                .send(Event::Retrying {
                    host: host.to_string(),
                    index,
                    name: names.get(i).cloned().unwrap_or_else(|| task.name.clone()),
                    left: *left,
                    result: attempt.clone(),
                    censored,
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
        let error = r.failed().then(|| {
            let mut block = ErrorBlock::of(r, task.origin.as_ref(), censored);
            if task.origin.is_none() && !censored {
                block.unknown = generated_source(task);
            }
            block
        });
        // A loop item's line shows the loop's own keys, as its registered result holds them:
        // measured on ansible-core 2.19.12 for a failed item and an `assert`'s, on the agent path
        // and the controller side alike. The `_ansible_` keys stay for the renderer to read.
        let mut shown = r.clone();
        shown.0.remove(ERROR_KEY);
        if is_loop && let Some(element) = element {
            item_keys(task, &mut shown.0, element);
        }
        // A templated `diff` was rendered per item for the module, which only reports a diff
        // when it is on, so a result that carries one stands for an asked-for diff.
        if !matches!(task.diff, None | Some(Flag::Fixed(false))) {
            shown.0.insert(DIFF_KEY.into(), Value::Bool(true));
        }
        let _ = tx
            .send(Event::Result {
                host: host.to_string(),
                index,
                label,
                outcome,
                result: shown,
                dump,
                show: true,
                counts: !is_loop,
                censored,
                delegate: delegate.map(str::to_string),
                error,
            })
            .await;
    }
    let mut failure = first_failure.map(|r| without_error(&r));
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
            // The reference prints no aggregate line for a loop that had items, except when
            // every one of them was skipped: `v2_runner_on_skipped` fires once for the task
            // itself, after the item lines, whenever the aggregate it hands over comes back
            // skipped - measured, `geerlingguy.git : Build git.` with a false `when` prints its
            // two item lines and then `skipping: [host]`. An ok, changed or failed loop still
            // shows nothing here: the item lines (and `...ignoring`) carry the message alone.
            //
            // A failed `debug` loop is the exception, measured: `_clean_results` strips a `debug`
            // result with a `msg` down to that `msg`, `results` included, so the callback no
            // longer sees a loop and prints `fatal: [h]: FAILED! => {"msg": "One or more items
            // failed"}` after the items. A `no_log` one has no `msg` left to keep, and no line.
            let show = aggregate.skipped() || (dump == Dump::Debug && any_failed && !censored);
            (aggregate, show)
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
                dump,
                show,
                counts: true,
                censored,
                delegate: delegate.map(str::to_string),
                error: None,
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

/// `r` as it is registered or handed to a rescue: without the error block's message the executor
/// left on it, and without the `_ansible_` keys, which the reference strips on the way too
/// (measured: a registered `debug` holds `msg`, `failed` and `changed` alone).
pub(super) fn without_error(r: &TaskResult) -> TaskResult {
    let mut r = r.clone();
    r.0.remove(ERROR_KEY);
    r.0.retain(|k, _| !k.starts_with("_ansible_") && !k.starts_with("_volant_"));
    r
}

/// What the reference quotes under `Origin: <unknown>` for the argument check it puts in front of
/// a role: the task it built (`Role._create_validation_task`), as Python prints a dict, shortened
/// to 120 characters. Measured on ansible-core 2.19.12. `None` for any other task.
fn generated_source(task: &PlayTask) -> Option<String> {
    if volant_protocol::modules::short_name(&task.module) != "validate_argument_spec" {
        return None;
    }
    let context = task.args.get("validate_args_context")?;
    let ds = serde_json::json!({
        "action": "ansible.builtin.validate_argument_spec",
        "args": {
            "argument_spec": task.args.get("argument_spec")?,
            "provided_arguments": task.args.get("provided_arguments")?,
            "validate_args_context": {
                "type": context.get("type")?,
                "name": context.get("name")?,
                "argument_spec_name": context.get("argument_spec_name")?,
                "path": context.get("path")?,
            },
        },
        "name": task.name,
        "tags": ["always"],
    });
    Some(crate::render::shorten(
        &crate::playbook::python_repr(&ds),
        120,
    ))
}

/// The keys the reference adds to a loop item's result: the item under the loop's variable name,
/// and that name under `ansible_loop_var`.
pub(super) fn item_keys(
    task: &PlayTask,
    result: &mut serde_json::Map<String, Value>,
    element: &Value,
) {
    result.insert(task.loop_var.clone(), element.clone());
    result.insert(
        "ansible_loop_var".into(),
        Value::from(task.loop_var.clone()),
    );
    if let Some(name) = &task.index_var {
        result.insert("ansible_index_var".into(), Value::from(name.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::Outcome;
    use crate::testing::task;
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

    /// Each item's outcome and whether its line shows, for a loop whose every item is skipped.
    async fn all_items_skipped() -> Vec<(Outcome, bool)> {
        let mut t = task("command");
        t.loop_items = Some(json!([1, 2]));
        let results: Vec<(Option<Value>, TaskResult)> = [1, 2]
            .into_iter()
            .map(|n| {
                (
                    Some(json!(n)),
                    TaskResult(json!({"skipped": true}).as_object().unwrap().clone()),
                )
            })
            .collect();
        let (tx, mut rx) = mpsc::channel(16);
        report_task(
            &tx,
            "h1",
            0,
            &t,
            &results,
            &[None, None],
            &[],
            &[],
            &[],
            Dump::No,
            false,
            None,
        )
        .await;
        drop(tx);
        let mut out = Vec::new();
        while let Some(event) = rx.recv().await {
            if let Event::Result { outcome, show, .. } = event {
                out.push((outcome, show));
            }
        }
        out
    }

    /// A loop whose every item a `when` skips still prints the task's own `skipping: [host]` line
    /// after the item lines.
    #[tokio::test]
    async fn a_loop_whose_every_item_is_skipped_shows_the_task_s_own_line() {
        assert_eq!(
            all_items_skipped().await,
            vec![
                (Outcome::Skipped, true),
                (Outcome::Skipped, true),
                (Outcome::Skipped, true),
            ]
        );
    }

    /// A failure is ignored item by item: an item whose own `ignore_errors` rendered true shows
    /// `...ignoring`, the other fails, and the task fails because one failure stands.
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
