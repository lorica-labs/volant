// SPDX-License-Identifier: GPL-3.0-or-later
//! The shadow ledger: one JSON line per replay, `.volant/shadow.jsonl` beside the `ansible.cfg`
//! in effect, which `volant shadow status` reads to say whether each playbook is ready.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One replay.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct Line {
    /// When the replay ran, as UTC text and as seconds since the epoch.
    pub(super) date: String,
    pub(super) time: u64,
    /// The playbooks the recording ran, as the checkout names them.
    pub(super) playbook: String,
    pub(super) playbook_sha256: String,
    pub(super) volant: String,
    pub(super) ansible_core: String,
    pub(super) recording_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) lock_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) limit: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) tags: Vec<String>,
    pub(super) hosts: usize,
    pub(super) compared: usize,
    pub(super) not_compared: usize,
    /// The `file:line` of every task compared.
    pub(super) tasks: Vec<String>,
    /// Every task the replay compiled, by `file:line`, with its name.
    pub(super) known: BTreeMap<String, String>,
    pub(super) divergences: BTreeMap<String, usize>,
    /// `clean`, `dirty` or `out-of-profile`.
    pub(super) verdict: String,
}

/// The ledger `explicit` names, or `.volant/shadow.jsonl` beside the `ansible.cfg` in effect,
/// or in the working directory when there is none.
pub(super) fn path(explicit: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    let dir = crate::config::locate()
        .and_then(|cfg| cfg.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    dir.join(".volant").join("shadow.jsonl")
}

/// Appends `line` to the ledger at `path`, making its directory when it is missing.
pub(super) fn append(path: &Path, line: &Line) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let text = serde_json::to_string(line).map_err(std::io::Error::other)?;
    writeln!(file, "{text}")
}

/// The lines of the ledger at `path`; a line that cannot be read is left out with a warning that
/// names it. A ledger that does not exist has no line.
pub(super) fn read(path: &Path) -> std::io::Result<Vec<Line>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut lines = Vec::new();
    for (n, text) in text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        match serde_json::from_str(text) {
            Ok(line) => lines.push(line),
            Err(e) => eprintln!(
                "[WARNING]: line {} of {} cannot be read and is left out: {e}",
                n + 1,
                path.display()
            ),
        }
    }
    Ok(lines)
}

/// `secs` since the epoch as UTC text, `2026-10-25T10:36:22Z`.
pub(super) fn utc(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or_default();
    let rest = secs % 86_400;
    // Howard Hinnant's `civil_from_days`.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Seconds since the epoch, now.
pub(super) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
