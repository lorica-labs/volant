// SPDX-License-Identifier: GPL-3.0-or-later
//! The comparison's report: a summary and each divergence for a person, or JSON Lines for a
//! CI job, one object per divergence and a summary object last.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde_json::{Value, json};

use super::compare::{Divergence, KINDS, Report};

impl Report {
    pub(super) fn human(&self) -> String {
        let mut out = String::new();
        let found = match self.divergences.len() {
            0 => "no divergence".to_owned(),
            n => format!("{n} divergence{}: {}", plural(n), counts(&self.by_kind())),
        };
        let _ = writeln!(
            out,
            "Shadow replay: {} task{} compared on {} host{}, {found}.",
            self.compared,
            plural(self.compared),
            self.hosts,
            plural(self.hosts)
        );
        if !self.skipped.is_empty() {
            let _ = writeln!(
                out,
                "Tasks not compared, or on their status alone: {}.",
                counts(&self.skipped)
            );
        }
        if !self.unargued.is_empty() {
            let _ = writeln!(out, "Arguments not compared: {}.", counts(&self.unargued));
        }
        if self.truncated {
            let _ = writeln!(
                out,
                "The recording is truncated: the run it holds was interrupted, so this replay is dirty."
            );
        }
        for d in &self.divergences {
            let _ = write!(
                out,
                "\n{}  {}  {}{}\n  ansible: {}\n  volant:  {}\n  diff:    {}\n",
                d.kind,
                d.host,
                d.path,
                if d.name.is_empty() {
                    String::new()
                } else {
                    format!("  {}", d.name)
                },
                d.ansible,
                d.volant,
                diff(&d.ansible, &d.volant)
            );
        }
        out
    }

    pub(super) fn json(&self) -> String {
        let mut out = String::new();
        for d in &self.divergences {
            let _ = writeln!(out, "{}", divergence(d));
        }
        let summary = json!({"summary": {
            "compared": self.compared,
            "hosts": self.hosts,
            "divergences": self.by_kind(),
            "not_compared": self.skipped,
            "arguments_not_compared": self.unargued,
            "truncated": self.truncated,
        }});
        let _ = writeln!(out, "{summary}");
        out
    }

    /// The number of divergences of each kind found, most serious first.
    fn by_kind(&self) -> BTreeMap<&'static str, usize> {
        let mut kinds = BTreeMap::new();
        for d in &self.divergences {
            *kinds.entry(d.kind).or_default() += 1;
        }
        kinds
    }
}

fn divergence(d: &Divergence) -> Value {
    json!({
        "host": d.host,
        "task": {"path": d.path, "name": d.name},
        "kind": d.kind,
        "ansible": d.ansible,
        "volant": d.volant,
        "diff": diff(&d.ansible, &d.volant),
    })
}

/// `reason count, reason count`, the kinds of divergence in their order of seriousness.
fn counts<K: AsRef<str> + Ord>(counts: &BTreeMap<K, usize>) -> String {
    let rank = |k: &str| KINDS.iter().position(|x| *x == k).unwrap_or(KINDS.len());
    let mut entries: Vec<(&K, &usize)> = counts.iter().collect();
    entries.sort_by_key(|(k, _)| (rank(k.as_ref()), k.as_ref().to_owned()));
    entries
        .iter()
        .map(|(k, n)| format!("{} {n}", label(k.as_ref())))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The words a person reads for a reason the JSON names by key.
fn label(key: &str) -> &str {
    match key {
        "after_flow" => "after a flow divergence",
        "no_log" => "no_log",
        "not_recorded" => "not in the recording",
        "controller" => "run on the controller",
        other => other,
    }
}

/// What changed from one value to the other: key by key for two objects.
fn diff(ansible: &Value, volant: &Value) -> String {
    let (Value::Object(a), Value::Object(v)) = (ansible, volant) else {
        return format!("{ansible} -> {volant}");
    };
    let mut keys: Vec<&String> = a.keys().chain(v.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|k| a.get(*k) != v.get(*k))
        .map(|k| {
            let side =
                |m: &serde_json::Map<String, Value>| m.get(k).cloned().unwrap_or(Value::Null);
            format!("{k}: {} -> {}", side(a), side(v))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
