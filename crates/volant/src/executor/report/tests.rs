// SPDX-License-Identifier: GPL-3.0-or-later
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
        Hold {
            by_default: false,
            permit: None,
            turn: None,
        },
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
        Hold {
            by_default: false,
            permit: None,
            turn: None,
        },
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
