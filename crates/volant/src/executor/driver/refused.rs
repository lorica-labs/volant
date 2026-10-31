// SPDX-License-Identifier: GPL-3.0-or-later
//! A batch whose host answered and then refused to escalate.

use std::sync::Mutex;

use tokio::sync::mpsc;
use volant_protocol::TaskResult;

use super::Driver;
use crate::executor::coordinator::Event;
use crate::executor::debugger::Hold;
use crate::executor::prepare::Item;
use crate::executor::report::report_task;
use crate::executor::run::{fact_targets, record_registered};
use crate::playbook::{Flag, PlayTask};
use crate::render::Dump;
use crate::vars::VarStore;

/// Reports the refusal as the failure of the batch's first task, and hands it back unless its
/// `ignore_errors` covered it.
///
/// Measured on ansible-core 2.19.12: a refused `become_user` is a task failure, so a rescue takes
/// it (`rescued=1`, exit 0), and `ignore_errors` lets the host carry on with the result
/// registered. That holds here only for a task alone in its batch, one item: behind a first task
/// the rest never ran, and a run reporting success while having quietly skipped them is the worst
/// outcome here.
#[expect(
    clippy::too_many_arguments,
    reason = "the driver's own state for one report, as `report_task` takes it"
)]
pub(super) async fn report_refusal(
    driver: &Driver<'_>,
    tx: &mpsc::Sender<Event>,
    host: &str,
    task: &PlayTask,
    batch: &[(usize, Vec<Item>)],
    msg: String,
    rescuable: bool,
    delegate: Option<&str>,
    store: &Mutex<VarStore>,
    hold: Hold<'_>,
) -> Option<TaskResult> {
    let mut task = task.clone();
    let ignore = match batch {
        [(_, items)] if task.loop_items.is_none() && items.len() == 1 => items[0].ignore_errors,
        _ => {
            task.ignore_errors = Some(Flag::Fixed(false));
            None
        }
    };
    let results = vec![(None, crate::transport::refused_escalation(msg))];
    let live = driver.progress.borrow().live_hosts.clone();
    let targets = fact_targets(&task, host, &live);
    record_registered(
        &mut store.lock().expect("vars lock"),
        &task,
        &targets,
        &results,
        &[],
    );
    report_task(
        tx,
        host,
        batch[0].0,
        &task,
        &results,
        &[None],
        &[ignore],
        &[],
        &[],
        Dump::No,
        rescuable,
        delegate,
        hold,
    )
    .await
}
