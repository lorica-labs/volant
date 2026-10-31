// SPDX-License-Identifier: GPL-3.0-or-later
//! What an unreachable host leaves behind under `ignore_unreachable`.

use std::sync::Mutex;

use serde_json::Value;
use tokio::sync::mpsc;
use volant_protocol::TaskResult;

use crate::playbook::PlayTask;
use crate::vars::VarStore;

use super::Driver;
use crate::executor::coordinator::{Event, LostItem};
use crate::executor::prepare::{Item, item_ignores_unreachable};
use crate::executor::run::{fact_targets, record_registered};
use crate::template::Templar;
/// An unreachable host under `ignore_unreachable`: the step shows the reason and the host stays.
/// What a `register` on the task reads is the reference's own unreachable result.
#[expect(
    clippy::too_many_arguments,
    reason = "the step's whole context: the sender, the host, the step, the reason and the stores"
)]
pub(super) async fn ignore_unreachable(
    tx: &mpsc::Sender<Event>,
    host: &str,
    index: usize,
    task: &PlayTask,
    (msg, items): (String, Vec<LostItem>),
    delegate: Option<&str>,
    store: &Mutex<VarStore>,
    register_hosts: &[String],
) {
    let (plain, reason) = crate::executor::coordinator::returned(&msg);
    let mut result = TaskResult::default();
    result.0.insert("changed".into(), Value::Bool(false));
    if plain {
        result.0.insert("failed".into(), Value::Bool(false));
    }
    result
        .0
        .insert("msg".into(), Value::String(reason.to_string()));
    result.0.insert("unreachable".into(), Value::Bool(true));
    // A loop registers its items under `results`, each as the reference keeps it, and the
    // aggregate stays unreachable.
    if !items.is_empty() {
        let kept: Vec<Value> = items
            .iter()
            .map(|(_, item)| {
                let mut kept = item.clone();
                kept.insert("exception".into(), "(traceback unavailable)".into());
                Value::Object(kept)
            })
            .collect();
        result.0.insert("msg".into(), "All items completed".into());
        result.0.insert("results".into(), Value::Array(kept));
    }
    record_registered(
        &mut store.lock().expect("vars lock"),
        task,
        register_hosts,
        &[(None, result)],
        &[],
    );
    let _ = tx
        .send(Event::UnreachableIgnored {
            host: host.to_string(),
            index,
            msg,
            censored: task.censors(),
            delegate: delegate.map(str::to_string),
            origin: task.origin.clone(),
            items,
        })
        .await;
    let _ = tx
        .send(Event::TaskDone {
            host: host.to_string(),
            index,
        })
        .await;
}

/// `ignore_unreachable` for a batch that never got an answer: shows the step, then moves `pos`
/// past it when the batch stopped short of doing so itself. `false` when the run was
/// interrupted on the way.
#[expect(
    clippy::too_many_arguments,
    reason = "the step's whole context: the driver, the sender, the list, the reason and the stores"
)]
pub(super) async fn ignore_the_batch(
    driver: &mut Driver<'_>,
    tx: &mpsc::Sender<Event>,
    host: &str,
    task: &PlayTask,
    index: usize,
    lost: (String, Vec<LostItem>),
    delegate: Option<&str>,
    store: &Mutex<VarStore>,
    undecided: Option<usize>,
    pos: &mut usize,
) -> bool {
    let live = driver.progress.borrow().live_hosts.clone();
    let targets = fact_targets(task, host, &live);
    ignore_unreachable(tx, host, index, task, lost, delegate, store, &targets).await;
    if let Some(step) = undecided {
        let Some(next) = driver.advance(step).await else {
            return false;
        };
        *pos = next;
    }
    true
}

/// What a batch a host could not be reached for leaves per loop item: measured on ansible-core
/// 2.19.12, each item of a looped step tries the connection and reports its own unreachable
/// result, and the host is forgiven only when every item's `ignore_unreachable` holds. A step
/// with no loop, or a batch of several, is one result, under the task's own keyword.
pub(super) fn lost_items(
    task: &PlayTask,
    batch: &[(usize, Vec<Item>)],
    msg: &str,
    templar: &Templar,
) -> (Vec<LostItem>, bool) {
    let ([(_, items)], true) = (batch, task.loop_items.is_some()) else {
        return (Vec::new(), task.ignores_unreachable());
    };
    let mut ignored = true;
    let lost = items
        .iter()
        .filter(|item| item.skipped.is_none())
        .map(|item| {
            ignored &= item_ignores_unreachable(task, item, templar);
            let mut result = serde_json::Map::new();
            result.insert("ansible_loop_var".into(), task.loop_var.clone().into());
            result.insert("changed".into(), Value::Bool(false));
            result.insert("item".into(), item.element.clone().unwrap_or(Value::Null));
            result.insert("msg".into(), msg.into());
            result.insert("unreachable".into(), Value::Bool(true));
            (item.label.clone(), result)
        })
        .collect();
    (lost, ignored)
}
