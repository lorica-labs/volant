// SPDX-License-Identifier: GPL-3.0-or-later
//! What a host's driver tells the coordinator about a step besides its results.

use serde_json::{Map, Value};
use volant_protocol::TaskResult;
use volant_protocol::modules::short_name;

use crate::executor::RunOptions;
use crate::executor::coordinator::{Event, MetaEffect, Shade};
use crate::playbook::PlayTask;
use crate::render::Dump;
use crate::stats::Outcome;
use crate::transport::{ConnectionKeywords, Transport};

use super::Driver;

impl Driver<'_> {
    /// A `[WARNING]` for this host at the step at `index`, shown even under `no_log`.
    pub(super) async fn warn(&self, index: usize, message: String) {
        let _ = self
            .tx
            .send(Event::Warning {
                host: self.name.to_string(),
                index,
                message,
                censored: false,
            })
            .await;
    }

    /// The connection the step at `index` runs over on `target`, and the `-vvv` redirect lines
    /// resolving its name gave, printed under that step as the reference prints them.
    pub(super) async fn reach(
        &self,
        index: usize,
        target: &str,
        vars: &Map<String, Value>,
        options: &RunOptions,
        keywords: &ConnectionKeywords,
    ) -> anyhow::Result<Transport> {
        let resolved = Transport::for_task(target, vars, &options.defaults, keywords);
        for line in options.defaults.names.take_lines(target) {
            let _ = self
                .tx
                .send(Event::Display {
                    host: self.name.to_string(),
                    index,
                    line,
                    shade: Shade::Verbose(3),
                    banner: true,
                })
                .await;
        }
        resolved
    }

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
                task_origin: None,
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

/// How the reference's callback cleans this task's result lines.
pub(super) fn dump_of(task: &PlayTask) -> Dump {
    match short_name(&task.module) {
        "debug" => Dump::Debug,
        "setup" | "gather_facts" => Dump::Setup,
        _ => Dump::No,
    }
}
