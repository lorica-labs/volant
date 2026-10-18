// SPDX-License-Identifier: GPL-3.0-or-later
//! Where a run's time went. `--profile` prints it on stderr once the run is over, and
//! `VOLANT_PROFILE_JSON=<file>` writes it one JSON object per line: the natives each host's agent
//! reported first, then the phases, then one line per task an agent ran.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use serde_json::{Value, json};
use volant_protocol::{ExecPath, Ran};

/// A stretch of the run the controller times.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// Loading and compiling the playbooks.
    Compile,
    /// The checks that refuse a playbook before anything connects.
    Preflight,
    /// Building the module payloads.
    Union,
    /// Opening a link, or proving a kept one alive.
    Connect,
    /// Making sure the host holds a batch's payload and staged files.
    Blob,
    /// Rendering a task for a host.
    Prepare,
    /// Waiting for the other hosts in front of a step.
    BarrierWait,
    /// A batch's round trip, less the time the agent spent running its tasks.
    Wire,
    /// Closing the links once the run is over.
    Shutdown,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Compile => "compile",
            Phase::Preflight => "preflight",
            Phase::Union => "union",
            Phase::Connect => "connect",
            Phase::Blob => "blob",
            Phase::Prepare => "prepare",
            Phase::BarrierWait => "barrier_wait",
            Phase::Wire => "wire",
            Phase::Shutdown => "shutdown",
        }
    }
}

/// One task one agent ran for one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    pub host: String,
    /// The step's index in the play.
    pub index: usize,
    pub task: String,
    /// The module the agent was asked to run: a plugin's sub-task names its own.
    pub module: String,
    /// What the agent said about running it; absent from an agent that says nothing.
    pub ran: Option<Ran>,
    pub prepare_micros: u64,
}

/// What one link measured of the batches that went over it, until the driver files it.
#[derive(Debug, Default)]
pub struct Ledger {
    pub blob_micros: u64,
    pub wire_micros: u64,
    /// Each result's position in its batch, the module asked for, and what the agent reported.
    pub ran: Vec<(usize, String, Option<Ran>)>,
}

#[derive(Debug, Default)]
struct Inner {
    phases: BTreeMap<(Phase, Option<String>), u64>,
    tasks: Vec<TaskRecord>,
    natives: BTreeMap<String, Vec<String>>,
    facts: Option<String>,
}

/// The whole run's measurements, shared by every host's driver.
#[derive(Debug, Default)]
pub struct Profile(Mutex<Inner>);

/// Microseconds since `start`.
pub fn micros(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn path_name(ran: Option<&Ran>) -> &'static str {
    match ran.map(|r| &r.path) {
        Some(ExecPath::Native) => "native",
        Some(ExecPath::Python) => "python",
        Some(ExecPath::Fallback) => "fallback",
        None => "unreported",
    }
}

fn median(values: &mut [u64]) -> Option<u64> {
    values.sort_unstable();
    values.get(values.len().checked_sub(1)? / 2).copied()
}

fn shown(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_string(), |v| v.to_string())
}

impl Profile {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Adds `micros` to `phase`, for one host or for the run as a whole.
    pub fn phase(&self, host: Option<&str>, phase: Phase, micros: u64) {
        *self
            .lock()
            .phases
            .entry((phase, host.map(str::to_string)))
            .or_default() += micros;
    }

    pub fn task(&self, record: TaskRecord) {
        self.lock().tasks.push(record);
    }

    /// The natives the agent on `host` reported at its last handshake.
    pub fn natives(&self, host: &str, natives: &[String]) {
        self.lock()
            .natives
            .insert(host.to_string(), natives.to_vec());
    }

    /// Where the run's `setup` gets its facts, and why: `native (keys: 23)`, or `python (...)`.
    pub fn facts(&self, decision: String) {
        self.lock().facts = Some(decision);
    }

    /// The table `--profile` prints.
    pub fn render(&self) -> String {
        let inner = self.lock();
        let mut out = String::new();
        let _ = writeln!(out, "PROFILE");
        if let Some(facts) = &inner.facts {
            let _ = writeln!(out, "facts: {facts}");
        }
        let _ = writeln!(out, "{:<14} {:>12}  per host", "phase", "total_us");
        let mut phases: BTreeMap<Phase, (u64, Vec<String>)> = BTreeMap::new();
        for ((phase, host), micros) in &inner.phases {
            let entry = phases.entry(*phase).or_default();
            entry.0 += micros;
            if let Some(host) = host {
                entry.1.push(format!("{host}={micros}"));
            }
        }
        for (phase, (total, hosts)) in &phases {
            let line = format!("{:<14} {total:>12}  {}", phase.name(), hosts.join(" "));
            let _ = writeln!(out, "{}", line.trim_end());
        }
        let _ = writeln!(
            out,
            "{:<24} {:<10} {:>5} {:>10} {:>12} {:>9} {:>9} {:>9}  reasons",
            "module", "path", "n", "median_us", "sum_us", "fork_us", "import_us", "module_us"
        );
        let mut modules: BTreeMap<(&str, &str), Vec<&TaskRecord>> = BTreeMap::new();
        let mut by_path: BTreeMap<&str, usize> = BTreeMap::new();
        for record in &inner.tasks {
            let path = path_name(record.ran.as_ref());
            modules
                .entry((record.module.as_str(), path))
                .or_default()
                .push(record);
            *by_path.entry(path).or_default() += 1;
        }
        for ((module, path), records) in &modules {
            let ran: Vec<&Ran> = records.iter().filter_map(|r| r.ran.as_ref()).collect();
            let mut total: Vec<u64> = ran.iter().map(|r| r.micros).collect();
            // Figures a host reported, so summed without trusting them to fit.
            let sum = total.iter().copied().fold(0, u64::saturating_add);
            let part = |f: fn(&Ran) -> Option<u64>| {
                median(&mut ran.iter().filter_map(|r| f(r)).collect::<Vec<_>>())
            };
            let (fork, import, module_run) = (
                part(|r| r.fork_micros),
                part(|r| r.import_micros),
                part(|r| r.module_micros),
            );
            let mut reasons: BTreeMap<&str, usize> = BTreeMap::new();
            for r in &ran {
                if let Some(reason) = &r.reason {
                    *reasons.entry(reason.as_str()).or_default() += 1;
                }
            }
            let reasons: Vec<String> = reasons
                .iter()
                .map(|(reason, n)| format!("{reason} x{n}"))
                .collect();
            let line = format!(
                "{module:<24} {path:<10} {:>5} {:>10} {sum:>12} {:>9} {:>9} {:>9}  {}",
                records.len(),
                shown(median(&mut total)),
                shown(fork),
                shown(import),
                shown(module_run),
                reasons.join(", ")
            );
            let _ = writeln!(out, "{}", line.trim_end());
        }
        let counts: Vec<String> = by_path
            .iter()
            .map(|(path, n)| format!("{path}={n}"))
            .collect();
        let _ = writeln!(out, "tasks by path: {}", counts.join(" "));
        out
    }

    /// The JSON lines `VOLANT_PROFILE_JSON` names: `{"natives": {host: [...]}}` first, then
    /// `{"facts": ...}` once the run has decided it, then one
    /// `{"phase", "host", "micros"}` per phase and host, then one [`TaskRecord`] per task.
    pub fn write_json(&self, path: &Path) -> std::io::Result<()> {
        let inner = self.lock();
        let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
        writeln!(out, "{}", json!({ "natives": inner.natives }))?;
        if let Some(facts) = &inner.facts {
            writeln!(out, "{}", json!({ "facts": facts }))?;
        }
        for ((phase, host), micros) in &inner.phases {
            writeln!(
                out,
                "{}",
                json!({ "phase": phase.name(), "host": host, "micros": micros })
            )?;
        }
        for record in &inner.tasks {
            // `ran`'s fields spread into the line, so `path` sits beside `module`.
            let mut line = match serde_json::to_value(&record.ran)? {
                Value::Object(ran) => ran,
                _ => serde_json::Map::new(),
            };
            line.insert("host".into(), json!(record.host));
            line.insert("index".into(), json!(record.index));
            line.insert("task".into(), json!(record.task));
            line.insert("module".into(), json!(record.module));
            line.insert("prepare_micros".into(), json!(record.prepare_micros));
            writeln!(out, "{}", Value::Object(line))?;
        }
        out.flush()
    }
}

#[cfg(test)]
mod tests;
