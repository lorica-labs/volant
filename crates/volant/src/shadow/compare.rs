// SPDX-License-Identifier: GPL-3.0-or-later
//! What differs between a recorded run and its replay: by host, task by task, in five kinds
//! from the most serious, `inventory`, `flow`, `loop`, `args` and `judgment`.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde_json::{Map, Value, json};

use super::recording::Recording;
use super::replay::Replay;
use super::watch::{Done, Watched};
use crate::stats::HostStats;

/// The kinds, most serious first.
pub(super) const KINDS: [&str; 5] = ["inventory", "flow", "loop", "args", "judgment"];

/// Statements that run no module, whose result lines the two engines do not share.
const NOT_TASKS: [&str; 6] = [
    "ansible.builtin.include_tasks",
    "ansible.builtin.include_role",
    "ansible.builtin.meta",
    "include_tasks",
    "include_role",
    "meta",
];

/// The recap's columns, under the names the recording gives them.
const RECAP: [&str; 7] = [
    "ok",
    "changed",
    "unreachable",
    "failures",
    "skipped",
    "rescued",
    "ignored",
];

#[derive(Debug, PartialEq)]
pub(super) struct Divergence {
    pub(super) host: String,
    /// The task's `file:line`, or `inventory` and `recap` for those two.
    pub(super) path: String,
    pub(super) name: String,
    pub(super) kind: &'static str,
    pub(super) ansible: Value,
    pub(super) volant: Value,
}

#[derive(Debug, Default)]
pub(super) struct Report {
    pub(super) divergences: Vec<Divergence>,
    pub(super) compared: usize,
    pub(super) hosts: usize,
    /// Tasks not compared, or compared on their status alone, by reason.
    pub(super) skipped: BTreeMap<&'static str, usize>,
    /// Result lines whose arguments were not compared, by reason.
    pub(super) unargued: BTreeMap<&'static str, usize>,
    pub(super) truncated: bool,
}

/// One task as one engine reported it on one host: its `file:line`, which time that path ran
/// on the host, and its result lines, one per loop item or a single one.
#[derive(Debug)]
struct Task {
    path: String,
    occurrence: usize,
    name: String,
    lines: Vec<Line>,
}

#[derive(Debug)]
struct Line {
    item: Option<usize>,
    status: String,
    result: Value,
}

/// Compares `recording` with what `replay` watched Volant do.
pub(super) fn compare(recording: &Recording, replay: &Replay) -> Report {
    let watched = replay
        .watched
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (recorded, inventory, recap) = recorded(recording);
    let mut report = Report {
        truncated: recording.truncated,
        ..Report::default()
    };
    if let Some(replayed) = &watched.inventory {
        compare_inventory(&inventory, replayed, &mut report);
    }
    let replayed = replayed(&watched.results);
    let mut hosts: Vec<&String> = recorded.keys().chain(replayed.keys()).collect();
    hosts.sort();
    hosts.dedup();
    report.hosts = hosts.len();
    for host in hosts {
        let ansible = recorded.get(host).map_or(&[][..], Vec::as_slice);
        let volant = replayed.get(host).map_or(&[][..], Vec::as_slice);
        if compare_host(host, ansible, volant, &watched, &mut report) {
            compare_recap(host, recap.get(host), watched.recap.get(host), &mut report);
        }
    }
    report
        .divergences
        .sort_by_key(|d| KINDS.iter().position(|k| *k == d.kind));
    report
}

/// The recorded tasks of each host, the recorded inventory and the recorded recap.
#[expect(
    clippy::type_complexity,
    reason = "the three things a recording says of its hosts"
)]
fn recorded(
    recording: &Recording,
) -> (
    BTreeMap<String, Vec<Task>>,
    Map<String, Value>,
    Map<String, Value>,
) {
    let cwd = Path::new(&recording.header.cwd);
    let mut tasks: HashMap<&str, (String, &str, &str)> = HashMap::new();
    let mut hosts: BTreeMap<String, Vec<Task>> = BTreeMap::new();
    let mut last: HashMap<&str, &str> = HashMap::new();
    let mut count: HashMap<(&str, String), usize> = HashMap::new();
    let (mut inventory, mut recap) = (Map::new(), Map::new());
    for event in &recording.events {
        let text = |key: &str| event.get(key).and_then(Value::as_str).unwrap_or_default();
        let object = |key: &str| event.get(key).and_then(Value::as_object).cloned();
        match text("event") {
            "task" => {
                let path = Path::new(text("path"));
                let path = path.strip_prefix(cwd).unwrap_or(path);
                let path = path.to_string_lossy().into_owned();
                tasks.insert(text("uuid"), (path, text("name"), text("action")));
            }
            "inventory" => inventory = object("hosts").unwrap_or_default(),
            "stats" => recap = object("hosts").unwrap_or_default(),
            "result" => {
                let (host, uuid) = (text("host"), text("task"));
                let Some((path, name, action)) = tasks.get(uuid) else {
                    continue;
                };
                let result = event.get("result").cloned().unwrap_or(Value::Null);
                let item = event
                    .get("item")
                    .and_then(Value::as_u64)
                    .map(|i| i as usize);
                let whole = item.is_none() && result.get("results").is_some();
                if NOT_TASKS.contains(action) || whole {
                    continue;
                }
                let list = hosts.entry(host.to_owned()).or_default();
                if last.insert(host, uuid) != Some(uuid) {
                    let n = count.entry((host, path.clone())).or_default();
                    list.push(Task {
                        path: path.clone(),
                        occurrence: *n,
                        name: (*name).to_owned(),
                        lines: Vec::new(),
                    });
                    *n += 1;
                }
                let line = Line {
                    item,
                    status: text("status").to_owned(),
                    result,
                };
                list.last_mut().expect("pushed above").lines.push(line);
            }
            _ => {}
        }
    }
    (hosts, inventory, recap)
}

/// Volant's tasks of each host, from the result lines it reported, in the same shape.
fn replayed(results: &[Done]) -> BTreeMap<String, Vec<Task>> {
    let mut hosts: BTreeMap<String, Vec<Task>> = BTreeMap::new();
    let mut last: HashMap<&str, usize> = HashMap::new();
    let mut count: HashMap<(&str, &str), usize> = HashMap::new();
    let mut items: HashMap<&str, usize> = HashMap::new();
    for done in results {
        let host = done.host.as_str();
        let list = hosts.entry(done.host.clone()).or_default();
        // A loop's whole-task line, which follows its items' lines under the same step.
        let after_items = last.get(host) == Some(&done.step)
            && list
                .last()
                .is_some_and(|t| t.lines.iter().any(|l| l.item.is_some()));
        if !done.item && (after_items || done.result.contains_key("results")) {
            continue;
        }
        if last.insert(host, done.step) != Some(done.step) {
            let n = count.entry((host, &done.path)).or_default();
            list.push(Task {
                path: done.path.clone(),
                occurrence: *n,
                name: String::new(),
                lines: Vec::new(),
            });
            *n += 1;
            items.insert(host, 0);
        }
        let item = done.item.then(|| {
            let next = items.entry(host).or_default();
            *next += 1;
            *next - 1
        });
        let line = Line {
            item,
            status: done.status.to_owned(),
            result: Value::Object(done.result.clone()),
        };
        list.last_mut().expect("pushed above").lines.push(line);
    }
    hosts
}

fn compare_inventory(
    recorded: &Map<String, Value>,
    replayed: &BTreeMap<String, Vec<String>>,
    report: &mut Report,
) {
    let mut names: Vec<&String> = recorded.keys().chain(replayed.keys()).collect();
    names.sort();
    names.dedup();
    for host in names {
        let ansible = recorded.get(host).cloned().unwrap_or(Value::Null);
        let volant = replayed.get(host).map_or(Value::Null, |g| json!(g));
        if ansible != volant {
            report.divergences.push(Divergence {
                host: host.clone(),
                path: "inventory".to_owned(),
                name: "groups".to_owned(),
                kind: "inventory",
                ansible,
                volant,
            });
        }
    }
}

/// Compares one host's tasks in order. The first `flow` divergence ends the comparison of the
/// host, since everything after it is shifted; so does a host the recording has as unreachable,
/// which the replay made unreachable too. Returns whether the host was compared to its end.
fn compare_host(
    host: &str,
    ansible: &[Task],
    volant: &[Task],
    watched: &Watched,
    report: &mut Report,
) -> bool {
    let total = ansible.len().max(volant.len());
    for i in 0..total {
        let (Some(a), Some(v)) = (ansible.get(i), volant.get(i)) else {
            flow(
                host,
                &ansible[i.min(ansible.len())..],
                &volant[i.min(volant.len())..],
                report,
            );
            *report.skipped.entry("after_flow").or_default() += total - i - 1;
            return false;
        };
        if (&a.path, a.occurrence) != (&v.path, v.occurrence) {
            flow(host, &ansible[i..], &volant[i..], report);
            *report.skipped.entry("after_flow").or_default() += total - i - 1;
            return false;
        }
        report.compared += 1;
        if a.lines.iter().any(|l| l.status == "unreachable") {
            return false;
        }
        compare_task(host, a, v, watched, report);
    }
    true
}

/// A task one engine ran where the other ran another one, or nothing: `ansible` and `volant`
/// are what each ran from there on. The divergence is Volant's task when the recording does not
/// have it further on, since Volant ran a task ansible-core did not run there; otherwise the
/// recorded task, which Volant left out.
fn flow(host: &str, ansible: &[Task], volant: &[Task], report: &mut Report) {
    let (a, v) = (ansible.first(), volant.first());
    let later = |task: &Task| {
        ansible
            .iter()
            .skip(1)
            .any(|t| (&t.path, t.occurrence) == (&task.path, task.occurrence))
    };
    let task = match (a, v) {
        (Some(_), Some(v)) if !later(v) => v,
        (None, Some(v)) => v,
        (Some(a), _) => a,
        (None, None) => return,
    };
    let name = a
        .filter(|a| std::ptr::eq(*a, task))
        .map(|a| a.name.clone())
        .unwrap_or_default();
    report.divergences.push(Divergence {
        host: host.to_owned(),
        path: task.path.clone(),
        name,
        kind: "flow",
        ansible: a.map_or(Value::Null, |a| json!(a.path)),
        volant: v.map_or(Value::Null, |v| json!(v.path)),
    });
}

fn compare_task(host: &str, a: &Task, v: &Task, watched: &Watched, report: &mut Report) {
    let mut found = Vec::new();
    if a.lines.len() == v.lines.len() {
        for (al, vl) in a.lines.iter().zip(&v.lines) {
            compare_line(host, a, al, vl, watched, report, &mut found);
        }
    } else {
        let items = |t: &Task| json!({"items": t.lines.len()});
        found.push(("loop", items(a), items(v)));
    }
    let censored = |l: &Line| l.result.get("censored") == Some(&Value::Bool(true));
    if a.lines.iter().all(censored) {
        *report.skipped.entry("no_log").or_default() += 1;
    }
    report
        .divergences
        .extend(found.into_iter().map(|(kind, ansible, volant)| Divergence {
            host: host.to_owned(),
            path: a.path.clone(),
            name: a.name.clone(),
            kind,
            ansible,
            volant,
        }));
}

/// One result line of each engine: the loop item, the status, the condition that skipped it,
/// the attempts, then the arguments the module ran with.
fn compare_line(
    host: &str,
    task: &Task,
    a: &Line,
    v: &Line,
    watched: &Watched,
    report: &mut Report,
    found: &mut Vec<(&'static str, Value, Value)>,
) {
    if a.item.is_some() && a.result.get("item") != v.result.get("item") {
        let item = |l: &Line| json!({"item": l.result.get("item").cloned().unwrap_or(Value::Null)});
        found.push(("loop", item(a), item(v)));
        return;
    }
    let censored = a.result.get("censored") == Some(&Value::Bool(true));
    let (mut ansible, mut volant) = (Map::new(), Map::new());
    if a.status != v.status {
        ansible.insert("status".into(), json!(a.status));
        volant.insert("status".into(), json!(v.status));
    }
    for key in ["false_condition", "attempts"] {
        let (x, y) = (a.result.get(key), v.result.get(key));
        if !censored && x != y {
            ansible.insert(key.into(), x.cloned().unwrap_or(Value::Null));
            volant.insert(key.into(), y.cloned().unwrap_or(Value::Null));
        }
    }
    if !ansible.is_empty() {
        found.push(("judgment", Value::Object(ansible), Value::Object(volant)));
    }
    let key = (
        host.to_owned(),
        task.path.clone(),
        task.occurrence,
        a.item.unwrap_or(0),
    );
    let module_args = a
        .result
        .pointer("/invocation/module_args")
        .and_then(Value::as_object);
    let reason = match (censored, module_args, watched.args.get(&key)) {
        (true, ..) => "no_log",
        (false, None, _) => "not_recorded",
        (false, Some(_), None) => "controller",
        (false, Some(recorded), Some(sent)) => {
            let (x, y) = different_args(recorded, sent);
            if !x.is_empty() {
                found.push(("args", Value::Object(x), Value::Object(y)));
            }
            return;
        }
    };
    *report.unargued.entry(reason).or_default() += 1;
}

/// The keys Volant sent whose value differs from the one the module ran with. Keys only in the
/// recording are the module's defaults, Volant's own keys start with `_volant`, and a value the
/// module hid is not compared.
fn different_args(
    recorded: &Map<String, Value>,
    sent: &Map<String, Value>,
) -> (Map<String, Value>, Map<String, Value>) {
    let (mut ansible, mut volant) = (Map::new(), Map::new());
    for (key, value) in sent {
        let Some(theirs) = recorded.get(key) else {
            continue;
        };
        if key.starts_with("_volant")
            || theirs == "VALUE_SPECIFIED_IN_NO_LOG_PARAMETER"
            || alike(theirs, value)
        {
            continue;
        }
        ansible.insert(key.clone(), theirs.clone());
        volant.insert(key.clone(), value.clone());
    }
    (ansible, volant)
}

/// Equal once the module's own conversion of what it was given is allowed for: `yes` for
/// `true`, `"8"` for `8`, `"a,b"` for a list.
fn alike(theirs: &Value, ours: &Value) -> bool {
    match (theirs, ours) {
        (Value::Bool(b), Value::String(s)) => {
            let s = s.to_ascii_lowercase();
            let words: &[&str] = if *b {
                &["yes", "on", "true", "1", "y", "t"]
            } else {
                &["no", "off", "false", "0", "n", "f"]
            };
            words.contains(&s.as_str())
        }
        (Value::Number(n), Value::String(s)) | (Value::String(s), Value::Number(n)) => {
            s.trim() == n.to_string()
        }
        (Value::String(s), Value::Bool(b)) => s == &b.to_string(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| alike(a, b))
        }
        (Value::Array(x), Value::String(s)) => {
            let parts: Vec<&str> = s.split(',').map(str::trim).collect();
            x.len() == parts.len() && x.iter().zip(parts).all(|(a, b)| alike(a, &json!(b)))
        }
        (Value::Object(x), Value::Object(y)) => {
            y.iter().all(|(k, b)| x.get(k).is_none_or(|a| alike(a, b)))
        }
        _ => theirs == ours,
    }
}

/// The recap line, compared once the host's tasks all were.
fn compare_recap(
    host: &str,
    recorded: Option<&Value>,
    replayed: Option<&HostStats>,
    report: &mut Report,
) {
    let (Some(recorded), Some(s)) = (recorded, replayed) else {
        return;
    };
    let counts = [
        s.ok,
        s.changed,
        s.unreachable,
        s.failed,
        s.skipped,
        s.rescued,
        s.ignored,
    ];
    let volant: Map<String, Value> = RECAP
        .iter()
        .zip(counts)
        .map(|(k, n)| ((*k).to_owned(), json!(n)))
        .collect();
    if RECAP.iter().any(|k| recorded.get(k) != volant.get(*k)) {
        report.divergences.push(Divergence {
            host: host.to_owned(),
            path: "recap".to_owned(),
            name: String::new(),
            kind: "judgment",
            ansible: recorded.clone(),
            volant: Value::Object(volant),
        });
    }
}

#[cfg(test)]
#[path = "compare_tests.rs"]
mod tests;
