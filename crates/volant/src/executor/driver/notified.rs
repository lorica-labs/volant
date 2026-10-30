// SPDX-License-Identifier: GPL-3.0-or-later
//! The `-vv` lines about notifications, measured on ansible-core 2.19.12: `Notification for
//! handler X has been saved.` before a changed task's result, and `NOTIFIED HANDLER X for host`
//! once per handler when a host reaches a flush.

use tokio::sync::mpsc;

use crate::compile::{Compiled, Step, StepKind};

use super::super::coordinator::Event;
use super::Item;

/// The names a task saved notifications for, one per item and name as rendered, at `-vv` and
/// above; empty when the task notified nothing.
pub(super) fn saved_lines(verbosity: u8, to_notify: &[usize], items: &[Item]) -> Vec<String> {
    if verbosity < 2 || to_notify.is_empty() {
        return Vec::new();
    }
    items.iter().flat_map(|i| i.notify.clone()).collect()
}

/// Whether the host sits out the include at `step`: a handler's when it did not notify that
/// handler, any other when it runs for its handlers alone.
pub(super) fn sits_out(step: &Step, notified: &[usize], handlers_only: bool) -> bool {
    match step.kind {
        StepKind::Handler(i) => !notified.contains(&i),
        _ => handlers_only,
    }
}

/// A handler's name once, in the order the host notified them: the reference lists the handlers
/// a host has pending, a topic standing for each handler that listens to it.
fn notified_names(c: &Compiled, notified: &[usize]) -> Vec<String> {
    let mut seen = Vec::new();
    for &i in notified {
        if !seen.contains(&i) {
            seen.push(i);
        }
    }
    seen.into_iter()
        .filter_map(|i| c.handlers.get(i))
        .map(|h| h.task.name.clone())
        .collect()
}

/// Says which handlers `host` has pending, in front of the banner of the first one.
pub(super) async fn announce_notified(
    tx: &mpsc::Sender<Event>,
    host: &str,
    index: usize,
    c: &Compiled,
    notified: &[usize],
) {
    for name in notified_names(c, notified) {
        let _ = tx
            .send(Event::Display {
                host: host.to_string(),
                index,
                line: format!("NOTIFIED HANDLER {name} for {host}"),
                shade: crate::executor::coordinator::Shade::Plain,
                banner: false,
            })
            .await;
    }
}
