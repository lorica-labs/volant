// SPDX-License-Identifier: GPL-3.0-or-later
//! What an unreachable host leaves behind under `ignore_unreachable`.

use std::sync::Mutex;

use serde_json::Value;
use tokio::sync::mpsc;
use volant_protocol::TaskResult;

use crate::compile::Compiled;
use crate::playbook::PlayTask;
use crate::vars::VarStore;

use super::Driver;
use crate::executor::coordinator::Event;
use crate::executor::run::{fact_targets, record_registered};
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
    msg: String,
    delegate: Option<&str>,
    store: &Mutex<VarStore>,
    register_hosts: &[String],
) {
    let mut result = TaskResult::default();
    result.0.insert("changed".into(), Value::Bool(false));
    result.0.insert("msg".into(), Value::String(msg.clone()));
    result.0.insert("unreachable".into(), Value::Bool(true));
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
    compiled: &Compiled,
    index: usize,
    msg: String,
    delegate: Option<&str>,
    store: &Mutex<VarStore>,
    undecided: Option<usize>,
    pos: &mut usize,
) -> bool {
    let task = &compiled.steps[index].task;
    let live = driver.progress.borrow().live_hosts.clone();
    let targets = fact_targets(task, host, &live);
    ignore_unreachable(tx, host, index, task, msg, delegate, store, &targets).await;
    if let Some(step) = undecided {
        let Some(next) = driver.advance(step).await else {
            return false;
        };
        *pos = next;
    }
    true
}
