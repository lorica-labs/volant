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

    /// The table `--profile` prints.
    pub fn render(&self) -> String {
        let inner = self.lock();
        let mut out = String::new();
        let _ = writeln!(out, "PROFILE");
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
            let sum: u64 = total.iter().sum();
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

    /// The JSON lines `VOLANT_PROFILE_JSON` names: `{"natives": {host: [...]}}` first, then one
    /// `{"phase", "host", "micros"}` per phase and host, then one [`TaskRecord`] per task.
    pub fn write_json(&self, path: &Path) -> std::io::Result<()> {
        let inner = self.lock();
        let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
        writeln!(out, "{}", json!({ "natives": inner.natives }))?;
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
mod tests {
    use super::*;

    fn ran(path: ExecPath, micros: u64) -> Ran {
        Ran {
            path,
            reason: None,
            micros,
            fork_micros: None,
            import_micros: None,
            module_micros: None,
        }
    }

    fn record(host: &str, module: &str, ran: Option<Ran>) -> TaskRecord {
        TaskRecord {
            host: host.into(),
            index: 1,
            task: "t".into(),
            module: module.into(),
            ran,
            prepare_micros: 7,
        }
    }

    fn filled() -> Profile {
        let profile = Profile::default();
        profile.phase(None, Phase::Compile, 1200);
        profile.phase(Some("h1"), Phase::Connect, 300);
        profile.phase(Some("h2"), Phase::Connect, 200);
        profile.phase(Some("h1"), Phase::Connect, 100);
        profile.natives("h1", &["stat".to_string()]);
        profile.natives("h2", &[]);
        profile.task(record("h1", "stat", Some(ran(ExecPath::Native, 40))));
        profile.task(record("h2", "stat", Some(ran(ExecPath::Native, 60))));
        profile.task(record("h1", "stat", Some(ran(ExecPath::Native, 50))));
        let python = Ran {
            fork_micros: Some(900),
            import_micros: Some(8000),
            module_micros: Some(3000),
            ..ran(ExecPath::Python, 12000)
        };
        profile.task(record("h1", "setup", Some(python)));
        let fallback = |reason: &str| Ran {
            reason: Some(reason.into()),
            ..ran(ExecPath::Fallback, 5000)
        };
        profile.task(record("h1", "copy", Some(fallback("validate"))));
        profile.task(record("h2", "copy", Some(fallback("validate"))));
        profile.task(record("h2", "copy", Some(fallback("remote_src"))));
        profile.task(record("h2", "ping", None));
        profile
    }

    /// The table: one line per phase with its total and each host's share, one line per module
    /// and path with the count, the median and the sum, Python's three parts, the hand-back
    /// reasons counted, and the tasks counted by path.
    ///
    /// What would make this red: a host's two connections not added up, a path folded into
    /// another, a reason dropped, or the median taken as the mean.
    #[test]
    fn the_table_has_a_line_per_phase_and_per_module_and_path() {
        let table = filled().render();
        let expected = "\
PROFILE
phase              total_us  per host
compile                1200
connect                 600  h1=400 h2=200
module                   path           n  median_us       sum_us   fork_us import_us module_us  reasons
copy                     fallback       3       5000        15000         -         -         -  remote_src x1, validate x2
ping                     unreported     1          -            0         -         -         -
setup                    python         1      12000        12000       900      8000      3000
stat                     native         3         50          150         -         -         -
tasks by path: fallback=3 native=3 python=1 unreported=1
";
        assert_eq!(table, expected, "\n{table}");
    }

    /// The first line names each host's natives, which is how the golden comparison knows which
    /// natives were on; then the phases; then one line per task, `ran` spread into it.
    ///
    /// What would make this red: the natives anywhere but first, or `ran` nested instead of
    /// flattened, which moves `path` out of the place a reader looks for it.
    #[test]
    fn the_json_starts_with_the_natives_and_has_a_line_per_task() {
        let dir = std::env::temp_dir().join(format!("volant-profile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("profile.jsonl");
        filled().write_json(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0], json!({"natives": {"h1": ["stat"], "h2": []}}));
        assert_eq!(
            lines[1],
            json!({"phase": "compile", "host": null, "micros": 1200})
        );
        assert_eq!(
            lines[2],
            json!({"phase": "connect", "host": "h1", "micros": 400})
        );
        assert_eq!(lines.len(), 1 + 3 + 8);
        assert_eq!(
            lines[4],
            json!({"host": "h1", "index": 1, "task": "t", "module": "stat", "path": "native",
                   "micros": 40, "prepare_micros": 7})
        );
        assert_eq!(
            lines[8],
            json!({"host": "h1", "index": 1, "task": "t", "module": "copy", "path": "fallback",
                   "reason": "validate", "micros": 5000, "prepare_micros": 7})
        );
    }
}
