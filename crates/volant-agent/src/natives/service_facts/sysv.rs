// SPDX-License-Identifier: GPL-3.0-or-later
//! `service --status-all`, read the way the reference reads it.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Map, Value};

use super::imp::{Listed, command, py_space};
use crate::natives::runtime::{Clock, Root};
/// `_list_sysvinit`: `service --status-all`, read with the reference's expression.
pub(super) fn sysv(
    root: &Root,
    service: &Path,
    env: &BTreeMap<String, String>,
    clock: Clock,
) -> Listed {
    let (rc, stdout) = command(env, clock, service, &["--status-all"])?;
    if rc == 4 && !root.exists("/etc/init.d") {
        return Ok(Vec::new());
    }
    if rc != 0 {
        return Err(
            format!("service --status-all exited {rc}, which the module warns about").into(),
        );
    }
    Ok(status_all(&stdout)
        .into_iter()
        .map(|(name, running)| {
            let mut service = Map::new();
            service.insert("name".into(), Value::from(name.as_str()));
            service.insert(
                "state".into(),
                Value::from(if running { "running" } else { "stopped" }),
            );
            service.insert("source".into(), Value::from("sysv"));
            (name, service)
        })
        .collect())
}

/// Every match of `^\s*\[ (?P<state>\+|\-) \]\s+(?P<name>.+)$` under `re.M`, found as
/// `finditer` finds them: `\s` crosses line ends, and `\s+` gives characters back to `.+`
/// when nothing else follows. `true` for `+`.
pub(super) fn status_all(text: &str) -> Vec<(String, bool)> {
    let mut found = Vec::new();
    let mut at = 0;
    while at <= text.len() {
        if at > 0 && !text[..at].ends_with('\n') {
            match text[at..].find('\n') {
                Some(end) => at += end + 1,
                None => break,
            }
            continue;
        }
        match match_at(text, at) {
            Some((end, name, running)) => {
                found.push((name, running));
                at = end;
            }
            None => at += text[at..].chars().next().map_or(1, char::len_utf8),
        }
    }
    found
}

pub(super) fn match_at(text: &str, start: usize) -> Option<(usize, String, bool)> {
    let rest = &text[start..];
    let skipped = rest.len() - rest.trim_start_matches(py_space).len();
    let after = &rest[skipped..];
    let running = if after.starts_with("[ + ]") {
        true
    } else if after.starts_with("[ - ]") {
        false
    } else {
        return None;
    };
    let q = start + skipped + "[ + ]".len();
    let spaces: Vec<(usize, char)> = text[q..]
        .char_indices()
        .take_while(|(_, c)| py_space(*c))
        .collect();
    let run_end = spaces.last().map_or(0, |(offset, c)| offset + c.len_utf8());
    // `\s+` takes at least one; `.+` then needs a character that is not a line end.
    for count in (1..=spaces.len()).rev() {
        let from = q + spaces.get(count).map_or(run_end, |(offset, _)| *offset);
        if text[from..].starts_with('\n') || from == text.len() {
            continue;
        }
        let end = text[from..].find('\n').map_or(text.len(), |end| from + end);
        return Some((end, text[from..end].to_string(), running));
    }
    None
}
