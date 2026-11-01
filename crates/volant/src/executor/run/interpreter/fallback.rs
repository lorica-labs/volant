// SPDX-License-Identifier: GPL-3.0-or-later

//! Discovery over a host's own `ansible_interpreter_python_fallback`, probed on the host the way
//! ansible-core 2.19.12's `discover_interpreter` probes it.
//!
//! The reference's default list is what the agent resolves at the handshake, so a host on it
//! needs no probe. A host with a list of its own gets the reference's shell line run on it, once
//! per discovery: after that the `discovered_interpreter_python` fact answers, as it does there.

use std::collections::HashMap;

use serde_json::{Map, Value, json};
use tokio::sync::watch;
use volant_protocol::interpreter::CANDIDATES;
use volant_protocol::{Task, TaskResult};

use crate::playbook::python_repr;
use crate::vars::{HostSettings, host_setting};

use super::super::batch::send_batch;
use super::super::channel::AgentChannel;
use super::{DISCOVERED_INTERPRETER, DISCOVERY_HELP, chosen_interpreter, discovery_warning};

/// Where the reference lands when the list finds nothing, or when the probe breaks.
const LAST_RESORT: &str = "/usr/bin/python3";

/// The batch number a probe goes out under, never one the driver numbers its batches with.
const PROBE_BATCH: u64 = u64::MAX;

/// The list a step's host set for itself, and what an earlier discovery over it found, as the
/// host's variables show it.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::executor) struct Fallback {
    list: Vec<String>,
    known: Option<String>,
}

/// The host's own list, `None` for a host that kept the reference's default list.
pub(in crate::executor) fn fallback<V: HostSettings + ?Sized>(vars: &V) -> Option<Fallback> {
    let list: Vec<String> = match host_setting(vars, "ansible_interpreter_python_fallback")? {
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map_or_else(|| python_repr(item), str::to_string)
            })
            .collect(),
        // A config value of type list, written as text: split on commas.
        Value::String(text) => text.split(',').map(|s| s.trim().to_string()).collect(),
        _ => return None,
    };
    if list == CANDIDATES {
        return None;
    }
    let known = host_setting(vars, "ansible_facts")
        .and_then(|facts| facts.get(DISCOVERED_INTERPRETER))
        .and_then(Value::as_str);
    Some(Fallback {
        list,
        known: known.map(str::to_string),
    })
}

/// What discovering over a host's own list found: the interpreter, the warning the probe gave
/// (registered, then as shown) and whether the usual warning about a discovered interpreter
/// follows it.
#[derive(Debug, Clone, PartialEq)]
struct Probed {
    interpreter: String,
    note: Option<(String, String)>,
    announced: bool,
}

/// The interpreters the steps of one batch choose from: what a host's own list found for the
/// steps that set one, what the agent reported at the handshake for the rest.
pub(in crate::executor) struct Found {
    by_step: HashMap<usize, Probed>,
    reported: Vec<String>,
}

impl Found {
    /// Probes, once per list, for every step of the batch whose host set its own list and has
    /// not discovered over it yet; a step whose fact already answers gets that.
    ///
    /// `(live, discovered)`: a replayed run reaches no host and probes nothing, and only a host
    /// the fact store holds the discovered interpreter for is taken at its variables' word -
    /// those can still show a fact `meta: clear_facts` removed.
    pub(in crate::executor) async fn probe<'a, C: AgentChannel>(
        link: &mut C,
        host: &str,
        steps: impl Iterator<Item = (usize, Option<&'a Fallback>)>,
        (live, discovered): (bool, bool),
        stop: (&mut watch::Receiver<bool>, &mut bool),
    ) -> Found {
        let mut found = Found {
            by_step: HashMap::new(),
            reported: link.interpreters(),
        };
        let (stop, stop_broken) = stop;
        let mut lists: Vec<(Vec<String>, Probed)> = Vec::new();
        for (index, fallback) in steps.filter(|_| live) {
            let Some(Fallback { list, known }) = fallback else {
                continue;
            };
            let probed = match known.as_ref().filter(|_| discovered) {
                Some(path) => Probed {
                    interpreter: path.clone(),
                    note: None,
                    announced: true,
                },
                None => {
                    if let Some((_, done)) = lists.iter().find(|(l, _)| l == list) {
                        done.clone()
                    } else {
                        let done = probe(link, host, list, stop, stop_broken).await;
                        lists.push((list.clone(), done.clone()));
                        done
                    }
                }
            };
            found.by_step.insert(index, probed);
        }
        found
    }

    /// The interpreters step `index` chooses from.
    pub(in crate::executor) fn interpreters(&self, index: usize) -> Vec<String> {
        self.by_step
            .get(&index)
            .map_or_else(|| self.reported.clone(), |p| vec![p.interpreter.clone()])
    }

    /// The interpreter step `index` runs under, or the failure it reports instead.
    pub(in crate::executor) fn chosen(
        &self,
        index: usize,
        asked: Option<&str>,
    ) -> Result<String, TaskResult> {
        chosen_interpreter(asked, &self.interpreters(index))
    }

    /// The interpreter a discovery on step `index` finds, if any.
    pub(in crate::executor) fn discovered(&self, index: usize) -> Option<String> {
        self.interpreters(index).into_iter().next()
    }

    /// The warnings a discovery on step `index` gives, each as shown and as registered: the
    /// probe's own first, then the usual one unless the list found nothing.
    pub(in crate::executor) fn warn(
        &self,
        index: usize,
        host: &str,
        said: &mut Vec<(String, bool)>,
        registered: &mut Vec<String>,
    ) {
        let probed = self.by_step.get(&index);
        if let Some((kept, shown)) = probed.and_then(|p| p.note.as_ref()) {
            said.push((shown.clone(), false));
            registered.push(kept.clone());
        }
        if let Some(interpreter) = self.discovered(index)
            && probed.is_none_or(|p| p.announced)
        {
            let text = discovery_warning(host, &interpreter);
            said.push((format!("{text} {DISCOVERY_HELP}"), false));
            registered.push(text);
        }
    }
}

/// Runs the reference's line on the host: `command -v` once per name, between two markers.
///
/// Each name is quoted by hand, as the reference quotes it rather than with `shlex.quote`, so a
/// name holding a quote breaks the line there too and ends in the same warning.
async fn probe<C: AgentChannel>(
    link: &mut C,
    host: &str,
    list: &[String],
    stop: &mut watch::Receiver<bool>,
    stop_broken: &mut bool,
) -> Probed {
    let line = format!(
        "echo FOUND; {}; echo ENDFOUND",
        list.iter()
            .map(|py| format!("command -v '{py}'"))
            .collect::<Vec<_>>()
            .join("; ")
    );
    let task = Task {
        module: "raw".into(),
        args: Map::from_iter([("_raw_params".to_string(), json!(line))]),
        ..Task::default()
    };
    let (mut results, _) = send_batch(
        link,
        host,
        PROBE_BATCH,
        vec![task],
        stop,
        stop_broken,
        &mut Vec::new(),
    )
    .await;
    let stdout = results
        .pop()
        .flatten()
        .and_then(|r| {
            r.0.get("stdout")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    read_probe(host, list, &stdout)
}

/// What the probe's output says, read as the reference reads it: everything between the
/// markers, the first line that is an absolute path.
fn read_probe(host: &str, list: &[String], stdout: &str) -> Probed {
    let host = python_repr(&json!(host));
    let between = stdout
        .strip_prefix("FOUND")
        .and_then(|rest| rest.rfind("ENDFOUND").map(|end| &rest[..end]));
    let Some(between) = between else {
        let cause = "unexpected output from Python interpreter discovery";
        let head = format!("Unhandled error in Python interpreter discovery for host {host}");
        return Probed {
            interpreter: LAST_RESORT.into(),
            note: Some((
                format!("{head}: {cause}"),
                format!("{head}: {cause}\n\n{head}.\n\n<<< caused by >>>\n\n{cause}\n"),
            )),
            announced: true,
        };
    };
    if let Some(path) = between.lines().find(|line| line.starts_with('/')) {
        return Probed {
            interpreter: path.trim().to_string(),
            note: None,
            announced: true,
        };
    }
    let msg = format!(
        "No python interpreters found for host {host} (tried {}).",
        python_repr(&json!(list))
    );
    Probed {
        interpreter: LAST_RESORT.into(),
        note: Some((msg.clone(), msg)),
        announced: false,
    }
}

#[cfg(test)]
mod tests;
