// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant shadow status`: what the ledger says of each playbook. A streak of clean replays,
//! the share of the playbook's tasks those replays compared, and a verdict: `ready`,
//! `not-ready` or `diverged`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde_json::{Value, json};

use super::ledger::Line;

/// One playbook's standing.
#[derive(Debug, PartialEq)]
pub(super) struct Row {
    pub(super) playbook: String,
    pub(super) streak: usize,
    /// The share of the playbook's tasks compared during the streak, in thousandths.
    pub(super) coverage: u32,
    pub(super) known: usize,
    /// The tasks never compared during the streak, by `file:line`, with their names.
    pub(super) missing: BTreeMap<String, String>,
    /// When the last dirty replay ran, and its kinds of divergence.
    pub(super) last_divergence: Option<(u64, Vec<String>)>,
    pub(super) verdict: &'static str,
    pub(super) reasons: Vec<String>,
}

/// The standing of each playbook in `lines`, in the order they first appear, against `runs`
/// clean replays in a row and a `coverage` in thousandths.
pub(super) fn rows(lines: &[Line], runs: u32, coverage: u32) -> Vec<Row> {
    let mut playbooks: Vec<&str> = Vec::new();
    for line in lines {
        if !playbooks.contains(&line.playbook.as_str()) {
            playbooks.push(&line.playbook);
        }
    }
    playbooks
        .into_iter()
        .map(|playbook| row(playbook, lines, runs, coverage))
        .collect()
}

fn row(playbook: &str, lines: &[Line], runs: u32, wanted: u32) -> Row {
    let mut streak: Vec<&Line> = Vec::new();
    let mut last: Option<&Line> = None;
    let mut last_dirty: Option<&Line> = None;
    for line in lines.iter().filter(|l| l.playbook == playbook) {
        // A replay of an ansible-core Volant does not follow says nothing either way.
        if line.verdict == "out-of-profile" {
            continue;
        }
        // Another Volant is another engine under test: its streak starts again.
        if last.is_some_and(|l| l.volant != line.volant) {
            streak.clear();
        }
        if line.verdict == "dirty" {
            streak.clear();
            last_dirty = Some(line);
        } else {
            streak.push(line);
        }
        last = Some(line);
    }
    let known = last.map(|l| l.known.clone()).unwrap_or_default();
    let compared: BTreeSet<&String> = streak.iter().flat_map(|l| &l.tasks).collect();
    let missing: BTreeMap<String, String> = known
        .iter()
        .filter(|(path, _)| !compared.contains(path))
        .map(|(path, name)| (path.clone(), name.clone()))
        .collect();
    let coverage = if known.is_empty() || streak.is_empty() {
        if streak.is_empty() { 0 } else { 1000 }
    } else {
        u32::try_from((known.len() - missing.len()) * 1000 / known.len()).unwrap_or(1000)
    };
    let last_divergence =
        last_dirty.map(|l| (l.time, l.divergences.keys().cloned().collect::<Vec<_>>()));
    let mut reasons = Vec::new();
    let verdict = if last.is_some_and(|l| l.verdict == "dirty") {
        reasons.push("the last replay diverged".to_owned());
        "diverged"
    } else {
        if streak.len() < runs as usize {
            reasons.push(format!(
                "{} clean replay{} in a row, {runs} wanted",
                streak.len(),
                if streak.len() == 1 { "" } else { "s" }
            ));
        }
        if coverage < wanted {
            let names: Vec<String> = missing
                .iter()
                .map(|(path, name)| format!("{name} ({path})"))
                .collect();
            reasons.push(format!(
                "{} of the tasks compared, {} wanted; never compared: {}",
                percent(coverage),
                percent(wanted),
                names.join(", ")
            ));
        }
        if reasons.is_empty() {
            "ready"
        } else {
            "not-ready"
        }
    };
    Row {
        playbook: playbook.to_owned(),
        streak: streak.len(),
        coverage,
        known: known.len(),
        missing,
        last_divergence,
        verdict,
        reasons,
    }
}

fn percent(thousandths: u32) -> String {
    let whole = thousandths / 10;
    match thousandths % 10 {
        0 => format!("{whole}%"),
        tenth => format!("{whole}.{tenth}%"),
    }
}

/// How long ago `then` was, seen from `now`, in the largest whole unit.
fn age(then: u64, now: u64) -> String {
    let secs = now.saturating_sub(then);
    let (n, unit) = match secs {
        0..60 => return "just now".to_owned(),
        60..3600 => (secs / 60, "minute"),
        3600..86_400 => (secs / 3600, "hour"),
        _ => (secs / 86_400, "day"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// The rows for a person: one line each, then its reasons.
pub(super) fn human(rows: &[Row], runs: u32, now: u64) -> String {
    let mut out = String::new();
    if rows.is_empty() {
        out.push_str("No replay in the ledger yet.\n");
    }
    for row in rows {
        let divergence = row.last_divergence.as_ref().map_or_else(
            || "no divergence".to_owned(),
            |(time, kinds)| format!("last divergence {} ({})", age(*time, now), kinds.join(", ")),
        );
        let _ = writeln!(
            out,
            "{}  {}  streak {}/{runs}, coverage {} of {} task{} ({} never compared), {divergence}",
            row.playbook,
            row.verdict,
            row.streak,
            percent(row.coverage),
            row.known,
            if row.known == 1 { "" } else { "s" },
            row.missing.len()
        );
        for reason in &row.reasons {
            let _ = writeln!(out, "  {reason}");
        }
    }
    out
}

/// The rows as JSON Lines, one object each.
pub(super) fn json(rows: &[Row], now: u64) -> String {
    let mut out = String::new();
    for row in rows {
        let object = json!({
            "playbook": row.playbook,
            "verdict": row.verdict,
            "streak": row.streak,
            "coverage": f64::from(row.coverage) / 1000.0,
            "tasks": row.known,
            "never_compared": row.missing.iter().map(|(p, n)| json!({"path": p, "name": n})).collect::<Vec<_>>(),
            "last_divergence": row.last_divergence.as_ref().map_or(Value::Null, |(time, kinds)| {
                json!({"age_seconds": now.saturating_sub(*time), "kinds": kinds})
            }),
            "reasons": row.reasons,
        });
        let _ = writeln!(out, "{object}");
    }
    out
}

#[cfg(test)]
#[path = "status_tests.rs"]
mod tests;
