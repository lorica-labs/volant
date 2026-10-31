// SPDX-License-Identifier: GPL-3.0-or-later
//! The end of a run: the recap and what `set_stats` collected.

use std::io::Write;

use super::recap_json::dumps_indented;
use super::{CHANGED, FAILED, HOST_COLUMN, OK, Renderer, SKIPPED, Style, UNREACHABLE, WARNING};
use crate::stats::{CustomStats, Stats};

impl Renderer {
    /// What `set_stats` collected, after the recap: each host that asked for `per_host`, sorted,
    /// then the run's own after a blank line. Nothing at all when nothing was collected.
    pub(super) fn custom_stats(&mut self, stats: &CustomStats) {
        if stats.is_empty() || self.minimal.is_some() {
            return;
        }
        let dump = |map: &serde_json::Map<String, serde_json::Value>| {
            dumps_indented(&serde_json::Value::Object(map.clone()), 0, 1).replace('\n', "")
        };
        self.banner("CUSTOM STATS:");
        let run = stats.run();
        for (host, data) in stats.hosts() {
            let _ = writeln!(self.out, "	{host}: {}", dump(data));
        }
        if !run.is_empty() {
            let _ = writeln!(self.out);
            let _ = writeln!(self.out, "	RUN: {}", dump(run));
        }
        let _ = writeln!(self.out);
    }

    /// The recap, then what `set_stats` collected when `custom` hands it over. The `minimal`
    /// callback prints no recap; the `json` one prints its document here.
    pub fn recap(&mut self, stats: &Stats, custom: Option<&CustomStats>) {
        if !self.json_finish(stats, custom) {
            self.recap_lines(stats);
            if let Some(custom) = custom {
                self.custom_stats(custom);
            }
        }
        self.aggregates_recap();
    }

    fn recap_lines(&mut self, stats: &Stats) {
        if self.minimal.is_some() {
            return;
        }
        self.banner("PLAY RECAP");
        for (host, h) in stats.hosts() {
            let host_style = if h.failed > 0 || h.unreachable > 0 {
                FAILED
            } else if h.changed > 0 {
                CHANGED
            } else {
                OK
            };
            let name = if self.color {
                let painted = self.paint(host_style, host);
                let width = HOST_COLUMN + (painted.len() - host.len());
                format!("{painted:<width$}")
            } else {
                format!("{host:<HOST_COLUMN$}")
            };
            let field = |lead: &str, n: u32, style: Style| {
                let text = format!("{lead}={n:<4}");
                if n > 0 {
                    self.paint(style, &text)
                } else {
                    text
                }
            };
            let _ = writeln!(
                self.out,
                "{name} : {} {} {} {} {} {} {}",
                field("ok", h.ok, OK),
                field("changed", h.changed, CHANGED),
                field("unreachable", h.unreachable, UNREACHABLE),
                field("failed", h.failed, FAILED),
                field("skipped", h.skipped, SKIPPED),
                field("rescued", h.rescued, OK),
                field("ignored", h.ignored, WARNING),
            );
        }
        let _ = writeln!(self.out);
    }
}
