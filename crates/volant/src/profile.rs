// SPDX-License-Identifier: GPL-3.0-or-later
//! Where a run's time went. `--profile` prints it on stderr once the run is over, and
//! `VOLANT_PROFILE_JSON=<file>` writes it one JSON object per line: the natives each host's agent
//! reported first, then the phases, then one line per task an agent ran, then the counters.
//!
//! The counters are bumped from the template engine and the bridge, which never see the run's
//! [`Profile`], so they live in statics. They always count, a relaxed add each; what costs more,
//! the per-plugin bridge table and the repeated renders, is kept only once [`track_repeats`] is on.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::hash::{Hash as _, Hasher as _};
use std::io::Write as _;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// Something the run does often enough to be worth counting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counter {
    /// A `when`, `changed_when`, `failed_when` or `until` condition evaluated.
    RenderWhen,
    /// A task's arguments rendered for one item.
    RenderArgs,
    /// A loop's list, or a `with_` lookup's terms, rendered.
    RenderLoop,
    /// A host's variables resolved against each other.
    RenderVars,
    /// One host's entry of `hostvars` converted for a render.
    RenderHostvars,
    /// An expression compiled.
    Compile,
    /// What a template reads, found in the cache.
    ReadsHit,
    /// What a template reads, worked out.
    ReadsMiss,
    /// A host's variable layers built.
    LayersBuilt,
    /// The entries those layers copied.
    LayersEntries,
    /// A plugin run through the Python helper.
    BridgeCalls,
    /// The time those runs took.
    BridgeNanos,
    /// Frame payload bytes written to the agents.
    WireOutBytes,
    /// Frame payload bytes read from the agents.
    WireInBytes,
}

/// The counters' names, in declaration order.
const NAMES: [&str; 14] = [
    "render_when",
    "render_args",
    "render_loop",
    "render_vars",
    "render_hostvars",
    "compile",
    "reads_hit",
    "reads_miss",
    "layers_built",
    "layers_entries",
    "bridge_calls",
    "bridge_nanos",
    "wire_out_bytes",
    "wire_in_bytes",
];

static COUNTS: [AtomicU64; NAMES.len()] = [const { AtomicU64::new(0) }; NAMES.len()];

/// Whether the per-plugin table and the repeated renders are kept.
static TRACKING: AtomicBool = AtomicBool::new(false);

/// Calls and nanoseconds per bridge plugin, while tracking.
static BRIDGE: Mutex<BTreeMap<String, (u64, u64)>> = Mutex::new(BTreeMap::new());

static REPEATS: Mutex<Option<Repeats>> = Mutex::new(None);

#[derive(Default)]
struct Repeats {
    /// How often each (task, host, text) was rendered, by hash.
    seen: HashMap<u64, u32>,
    total: u64,
    /// The repeated renders of each text.
    texts: HashMap<String, u64>,
}

/// Adds `by` to `counter`.
pub fn count(counter: Counter, by: u64) {
    COUNTS[counter as usize].fetch_add(by, Ordering::Relaxed);
}

/// Every counter by its snake_case name, in declaration order.
pub fn counters() -> Vec<(&'static str, u64)> {
    NAMES
        .iter()
        .zip(&COUNTS)
        .map(|(name, value)| (*name, value.load(Ordering::Relaxed)))
        .collect()
}

/// Starts a run's counting: every counter back to zero, and the per-plugin bridge table and the
/// repeated renders kept only when `on`. A run turns them on when it is asked for a profile, and
/// never pays for them otherwise.
pub fn track_repeats(on: bool) {
    for value in &COUNTS {
        value.store(0, Ordering::Relaxed);
    }
    BRIDGE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    *REPEATS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    TRACKING.store(on, Ordering::Relaxed);
}

/// Whether [`track_repeats`] is on, for a caller that would walk a value to note its texts.
pub fn tracks_repeats() -> bool {
    TRACKING.load(Ordering::Relaxed)
}

/// Notes that the step at `task` rendered `text` for `host`. Nothing unless tracking.
pub fn note_render(task: usize, host: &str, text: &str) {
    if !tracks_repeats() {
        return;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (task, host, text).hash(&mut hasher);
    let key = hasher.finish();
    let mut guard = REPEATS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let repeats = guard.get_or_insert_with(Repeats::default);
    let seen = repeats.seen.entry(key).or_default();
    *seen = seen.saturating_add(1);
    if *seen > 1 {
        repeats.total += 1;
        *repeats.texts.entry(text.to_string()).or_default() += 1;
    }
}

/// The renders that repeated one already done for the same step and host, and the `top` texts
/// repeated most, most first.
pub fn repeats(top: usize) -> (u64, Vec<(String, u64)>) {
    let guard = REPEATS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(repeats) = guard.as_ref() else {
        return (0, Vec::new());
    };
    let mut texts: Vec<(String, u64)> = repeats
        .texts
        .iter()
        .map(|(text, n)| (text.clone(), *n))
        .collect();
    texts.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    texts.truncate(top);
    (repeats.total, texts)
}

/// Counts one bridge call to the plugin `name` that took `nanos`.
pub fn bridge_call(name: &str, nanos: u64) {
    count(Counter::BridgeCalls, 1);
    count(Counter::BridgeNanos, nanos);
    if !tracks_repeats() {
        return;
    }
    let mut table = BRIDGE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((calls, total)) = table.get_mut(name) {
        *calls += 1;
        *total = total.saturating_add(nanos);
    } else {
        table.insert(name.to_string(), (1, nanos));
    }
}

fn bridge_table() -> BTreeMap<String, (u64, u64)> {
    BRIDGE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The repeated texts a profile shows.
const TOP_REPEATS: usize = 20;

/// A text on one line of at most 80 characters.
fn clipped(text: &str) -> String {
    text.escape_debug().take(80).collect()
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
        let _ = writeln!(out, "counters");
        for (name, value) in counters() {
            let _ = writeln!(out, "{name:<24} {value:>14}");
        }
        let _ = writeln!(
            out,
            "{:<24} {:>14} {:>14}",
            "bridge plugin", "calls", "nanos"
        );
        for (name, (calls, nanos)) in bridge_table() {
            let _ = writeln!(out, "{name:<24} {calls:>14} {nanos:>14}");
        }
        let (total, top) = repeats(TOP_REPEATS);
        let _ = writeln!(out, "repeated renders: {total}");
        for (text, n) in top {
            let _ = writeln!(out, "{n:>8}  {}", clipped(&text));
        }
        out
    }

    /// The JSON lines `VOLANT_PROFILE_JSON` names: `{"natives": {host: [...]}}` first, then
    /// `{"facts": ...}` once the run has decided it, then one
    /// `{"phase", "host", "micros"}` per phase and host, then one [`TaskRecord`] per task, then
    /// `{"counters": {...}, "bridge": {name: {"calls", "nanos"}}, "repeats": {"total", "top"}}`.
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
        let counted: serde_json::Map<String, Value> = counters()
            .into_iter()
            .map(|(name, value)| (name.to_string(), json!(value)))
            .collect();
        let bridge: serde_json::Map<String, Value> = bridge_table()
            .into_iter()
            .map(|(name, (calls, nanos))| (name, json!({ "calls": calls, "nanos": nanos })))
            .collect();
        let (total, top) = repeats(TOP_REPEATS);
        writeln!(
            out,
            "{}",
            json!({
                "counters": counted,
                "bridge": bridge,
                "repeats": { "total": total, "top": top },
            })
        )?;
        out.flush()
    }
}

#[cfg(test)]
mod tests;
