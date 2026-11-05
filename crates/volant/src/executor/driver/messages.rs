// SPDX-License-Identifier: GPL-3.0-or-later
//! What a host's driver tells the coordinator about a step besides its results.

use serde_json::{Map, Value};
use volant_protocol::TaskResult;
use volant_protocol::modules::short_name;

use crate::executor::RunOptions;
use crate::executor::coordinator::{Event, MetaEffect, Shade};
use crate::executor::prepare::{Item, render_deferred};
use crate::playbook::PlayTask;
use crate::render::Dump;
use crate::stats::Outcome;
use crate::template::Templar;
use crate::transport::{ConnectionKeywords, Transport};

use super::Driver;

impl Driver<'_> {
    /// What the helper's `Display` said while a host no agent runs on ran the steps at
    /// `indices`, taken out of each result, which holds it under `_volant_display`: each verbose
    /// line at its level, `<host> ` in front when it names one, and a plain line as it is.
    /// Warnings and deprecations reach the output through the result's own fields.
    pub(super) async fn show_display(
        &self,
        indices: impl Iterator<Item = usize>,
        results: &mut [Vec<Option<TaskResult>>],
    ) {
        for (index, items) in indices.zip(results.iter_mut()) {
            for result in items.iter_mut().flatten() {
                let Some(Value::Array(events)) = result.0.remove("_volant_display") else {
                    continue;
                };
                for event in events {
                    let text = event
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let line = match event.get("host").and_then(Value::as_str) {
                        Some(host) => format!("<{host}> {text}"),
                        None => text.to_string(),
                    };
                    let shade = match event.get("level").and_then(Value::as_u64) {
                        Some(0) => Shade::Plain,
                        Some(level) => Shade::Verbose(u8::try_from(level).unwrap_or(u8::MAX)),
                        None => continue,
                    };
                    let host = self.name.to_string();
                    let event = Event::Display {
                        host,
                        index,
                        line,
                        shade,
                        banner: true,
                    };
                    let _ = self.tx.send(event).await;
                }
            }
        }
    }

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

    /// Loop item `ii` of the step at `index` as the loop reaches it, the rest of its fields
    /// rendered and their warnings shown: see `render_deferred`.
    pub(super) async fn reach_item<'i>(
        &self,
        index: usize,
        task: &PlayTask,
        items: &'i mut [Item],
        ii: usize,
        templar: &Templar,
    ) -> &'i Item {
        for message in render_deferred(task, self.plan, templar, self.name, &mut items[ii], ii) {
            self.warn(index, message).await;
        }
        &items[ii]
    }

    /// What the coordinator prints over the step at `index` before it runs: its name as rendered
    /// for this host, when it is a template, and the banner itself when the step `runs` now.
    pub(super) async fn starting(&self, index: usize, spoken: Option<String>, runs: bool) {
        let host = self.name.to_string();
        if let Some(name) = spoken {
            let host = host.clone();
            let _ = self.tx.send(Event::Named { host, index, name }).await;
        }
        if runs {
            let _ = self.tx.send(Event::Started { host, index }).await;
        }
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
        self.passed(index);
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
        self.meta_said(index, &why).await;
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

    /// The `META: <what it did>` line a `meta` the playbook wrote prints at `-vv`, measured on
    /// ansible-core 2.19.12: the reason it was skipped, or what it did.
    pub(super) async fn meta_said(&self, index: usize, header: &str) {
        let _ = self
            .tx
            .send(Event::Display {
                host: self.name.to_string(),
                index,
                line: format!("META: {header}"),
                shade: Shade::Verbose(2),
                banner: true,
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

/// What a `meta` that ran says it did, in the reference's words.
pub(super) fn meta_done(action: &str, host: &str, role: Option<&str>) -> String {
    match action {
        "end_host" => format!("ending play for {host}"),
        "end_batch" => "ending batch".to_string(),
        "end_play" => "ending play".to_string(),
        "clear_facts" => "facts cleared".to_string(),
        "clear_host_errors" => "cleared host errors".to_string(),
        "refresh_inventory" => "inventory successfully refreshed".to_string(),
        "reset_connection" => "reset connection".to_string(),
        "end_role" => format!("ending role {} for {host}", role.unwrap_or_default()),
        other => other.to_string(),
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
