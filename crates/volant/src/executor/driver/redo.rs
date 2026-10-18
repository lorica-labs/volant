// SPDX-License-Identifier: GPL-3.0-or-later
//! A host that could not be reached, held at the debugger prompt when its step opens it there.

use tokio::sync::{mpsc, oneshot};

use crate::executor::coordinator::Event;
use crate::executor::debugger::{self, Answer, Edits, Kind};
use crate::executor::prepare::PlayPlan;

/// Sends `gone`, the `Unreachable` of this host, and when its step opens the prompt for it,
/// waits for the operator's answer: the step `r` runs again and the edits made before it, or
/// `None` for a host that leaves the play. Measured on ansible-core 2.19.12, `r` behind
/// `UNREACHABLE!` tries the task again, and a host still out of reach prints the line and opens
/// the prompt again.
pub(super) async fn unreachable(
    tx: &mpsc::Sender<Event>,
    plan: &PlayPlan,
    gone: Event,
) -> Option<(usize, Option<Edits>)> {
    let by_default = debugger::by_default(plan.strategy);
    let held =
        match &gone {
            Event::Unreachable {
                host,
                index: Some(at),
                ..
            } if plan.steps().steps.get(*at).is_some_and(|step| {
                debugger::opens_for(&step.task, by_default, Kind::Unreachable)
            }) =>
            {
                Some((host.clone(), *at))
            }
            _ => None,
        };
    let Some((host, at)) = held else {
        let _ = tx.send(gone).await;
        return None;
    };
    let (reply, answer) = oneshot::channel();
    let _ = tx
        .send(Event::Debug {
            host,
            index: at,
            reply,
        })
        .await;
    let _ = tx.send(gone).await;
    // A coordinator that went away without answering leaves the host gone.
    match answer.await {
        Ok(Answer::Redo(edits)) => Some((at, edits)),
        _ => None,
    }
}
