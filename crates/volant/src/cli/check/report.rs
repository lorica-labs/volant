// SPDX-License-Identifier: GPL-3.0-or-later
//! A playbook's verdict, and the text and JSON that report every verdict of a check.

use std::fmt::Write;

use serde_json::{Value, json};

/// Why a playbook is not `ready`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// Volant cannot run it yet: a gap of this engine.
    Unsupported,
    /// ansible-core rejects it too.
    Invalid,
    /// Only a run can tell.
    Unknown,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Unsupported => "unsupported",
            Kind::Invalid => "invalid",
            Kind::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct Reason {
    pub kind: Kind,
    /// The exit code `volant playbook` stops with, for a refusal.
    pub code: Option<i32>,
    pub message: String,
    pub file: Option<String>,
    pub line: Option<usize>,
}

#[derive(Debug)]
pub(super) struct Verdict {
    pub path: String,
    pub plays: usize,
    pub tasks: usize,
    pub reasons: Vec<Reason>,
}

impl Verdict {
    pub(super) fn refused(&self) -> bool {
        self.reasons.iter().any(|r| r.kind != Kind::Unknown)
    }

    pub(super) fn word(&self) -> &'static str {
        if self.refused() {
            "refused"
        } else if self.reasons.is_empty() {
            "ready"
        } else {
            "unknown"
        }
    }
}

fn place(reason: &Reason) -> String {
    match (&reason.file, reason.line) {
        (Some(file), Some(line)) => format!("{file}:{line}"),
        (Some(file), None) => file.clone(),
        _ => "-".to_string(),
    }
}

/// One line per playbook, the first reason on it and the others below it, then the counts.
pub(super) fn text(verdicts: &[Verdict], skipped: Option<usize>) -> String {
    let width = verdicts.iter().map(|v| v.path.len()).max().unwrap_or(0);
    let mut out = String::new();
    for verdict in verdicts {
        // Refusals first: they are what a reader has to act on.
        let mut reasons: Vec<&Reason> = verdict.reasons.iter().collect();
        reasons.sort_by_key(|r| r.kind == Kind::Unknown);
        if reasons.is_empty() {
            _ = writeln!(out, "{:width$}  ready", verdict.path);
        }
        for (i, reason) in reasons.iter().enumerate() {
            let path = if i == 0 { verdict.path.as_str() } else { "" };
            let (word, kind) = match reason.kind {
                Kind::Unknown => ("unknown", String::new()),
                Kind::Unsupported => ("refused", String::new()),
                Kind::Invalid => ("refused", "(invalid)  ".to_string()),
            };
            _ = writeln!(
                out,
                "{path:width$}  {word}  {kind}{}  {}",
                place(reason),
                reason.message
            );
        }
    }
    let count = |word: &str| verdicts.iter().filter(|v| v.word() == word).count();
    let noun = if verdicts.len() == 1 {
        "playbook"
    } else {
        "playbooks"
    };
    _ = write!(
        out,
        "{} {noun}: {} ready, {} refused, {} unknown",
        verdicts.len(),
        count("ready"),
        count("refused"),
        count("unknown")
    );
    if let Some(skipped) = skipped {
        _ = write!(out, ", {skipped} other YAML files skipped");
    }
    out.push('\n');
    out
}

/// The one JSON document of `--json`.
pub(super) fn json(verdicts: &[Verdict], skipped: usize) -> Value {
    let count = |word: &str| verdicts.iter().filter(|v| v.word() == word).count();
    let playbooks: Vec<Value> = verdicts
        .iter()
        .map(|v| {
            let reasons: Vec<Value> = v
                .reasons
                .iter()
                .map(|r| {
                    json!({
                        "kind": r.kind.name(),
                        "code": r.code,
                        "message": r.message,
                        "file": r.file,
                        "line": r.line,
                    })
                })
                .collect();
            json!({
                "path": v.path,
                "verdict": v.word(),
                "plays": v.plays,
                "tasks": v.tasks,
                "reasons": reasons,
            })
        })
        .collect();
    json!({
        "v": 1,
        "engine": crate::VERSION,
        "lens": ["ready"],
        "playbooks": playbooks,
        "summary": {
            "ready": count("ready"),
            "refused": count("refused"),
            "unknown": count("unknown"),
            "skipped_files": skipped,
        },
    })
}
