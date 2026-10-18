// SPDX-License-Identifier: GPL-3.0-or-later
//! What a host's driver tells the coordinator about a step besides its results.

use serde_json::Value;
use volant_protocol::TaskResult;

use crate::executor::coordinator::{Event, MetaEffect};
use crate::render::Dump;
use crate::stats::Outcome;

use super::Driver;

impl Driver<'_> {
    /// Tells the coordinator this host is done with the step at `index`.
    pub(super) async fn task_done(&self, index: usize) {
        let _ = self
            .tx
            .send(Event::TaskDone {
                host: self.name.to_string(),
                index,
            })
            .await;
    }

    /// Tells the coordinator what the `meta` action at `index` asks of the batch.
    pub(super) async fn meta(&self, index: usize, effect: MetaEffect) {
        let _ = self
            .tx
            .send(Event::Meta {
                host: self.name.to_string(),
                index,
                effect,
            })
            .await;
    }

    /// The line of a `meta` whose condition did not hold, with the words the reference gives it
    /// at `-v`, or the censored line under `no_log`. It is no result the recap counts: measured, a skipped `meta` adds nothing.
    pub(super) async fn meta_skipped(
        &self,
        index: usize,
        action: &str,
        role: Option<&str>,
        censored: bool,
    ) {
        let host = self.name;
        let held = format!("{action} conditional evaluated to False");
        let (msg, why) = match action {
            "flush_handlers" => (
                action.to_string(),
                format!("{held}, not running handlers for {host}"),
            ),
            "clear_facts" => (
                action.to_string(),
                format!("{held}, not clearing facts and fact cache for {host}"),
            ),
            "clear_host_errors" => (
                action.to_string(),
                format!("{held}, not clearing host error state for {host}"),
            ),
            "end_batch" => (
                action.to_string(),
                format!("{held}, continuing current batch"),
            ),
            "end_play" => (action.to_string(), format!("{held}, continuing play")),
            "end_host" => (
                format!("end_host conditional evaluated to false, continuing execution for {host}"),
                format!("{held}, continuing execution for {host}"),
            ),
            // The reference's sentence runs the two halves together, and so does this.
            _ => (
                action.to_string(),
                format!(
                    "{held}continuing role {} for {host}",
                    role.unwrap_or_default()
                ),
            ),
        };
        let mut result = TaskResult::default();
        result.0.insert("msg".into(), Value::String(msg));
        result.0.insert("skipped".into(), Value::Bool(true));
        result.0.insert("skip_reason".into(), Value::String(why));
        let _ = self
            .tx
            .send(Event::Result {
                host: host.to_string(),
                index,
                label: None,
                outcome: Outcome::Skipped,
                result,
                dump: Dump::No,
                show: true,
                counts: false,
                censored,
                delegate: None,
                error: None,
            })
            .await;
    }

    /// Asks for the banner of the step at `index`.
    pub(super) async fn banner(&self, index: usize) {
        let _ = self
            .tx
            .send(Event::Banner {
                host: self.name.to_string(),
                index,
            })
            .await;
    }
}
