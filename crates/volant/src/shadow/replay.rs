// SPDX-License-Identifier: GPL-3.0-or-later
//! The answers a replay gives in place of the hosts: each recorded result, found by the host,
//! the task's file and line, which time that task ran on the host, and the loop item.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Map, Value};
use volant_protocol::TaskResult;

use super::recording::Recording;
use crate::playbook::{Origin, PlayTask};

/// Where a result was recorded: host, task path relative to the run's directory, the n-th time
/// that path ran on that host, and the item index (`None` outside a loop).
type Key = (String, String, usize, Option<usize>);

/// The key the last recorded answer of an `until` carries, for [`crate::executor`]'s judge to
/// end the loop there when the condition reads an unknown value.
pub(crate) const FINAL: &str = "_volant_final";

/// The failed attempts of an `until`, in order, then the result the task ended with.
#[derive(Debug, Default)]
struct Answer {
    attempts: Vec<Value>,
    result: Value,
    status: String,
}

#[derive(Debug)]
pub(crate) struct Replay {
    answers: HashMap<Key, Answer>,
    /// The directory the replay runs in, which task paths are made relative to.
    root: PathBuf,
    /// Whether the recording stops before the run's end: a task it has no result for is where
    /// it stops, not a task ansible-core never ran.
    truncated: bool,
    /// What Volant did, gathered while the run reports it, for the comparison afterwards.
    pub(super) watched: std::sync::Mutex<super::watch::Watched>,
}

/// Under a replay, which time each task's `file:line` has reached one host, and the occurrence
/// each step of the host was given when its driver took it up: run, skipped or sent alike, since
/// the recording counts every task that has a result.
#[derive(Debug, Default)]
pub(crate) struct Seen {
    counts: HashMap<(Arc<str>, usize), usize>,
    steps: HashMap<usize, usize>,
}

impl Seen {
    /// Counts step `pos` as one more time `task`'s file and line reached the host; nothing
    /// outside a replay, where `seen` is `None`.
    pub(crate) fn mark(seen: &mut Option<Seen>, pos: usize, task: &PlayTask) {
        let (Some(seen), Some(origin)) = (seen.as_mut(), task.origin.as_ref()) else {
            return;
        };
        let count = seen
            .counts
            .entry((Arc::clone(&origin.path), origin.line))
            .or_default();
        seen.steps.insert(pos, *count);
        *count += 1;
    }

    /// The occurrence the next step of `task`'s file and line will be given.
    pub(crate) fn peek(seen: Option<&Seen>, task: &PlayTask) -> usize {
        let (Some(seen), Some(origin)) = (seen, task.origin.as_ref()) else {
            return 0;
        };
        seen.counts
            .get(&(Arc::clone(&origin.path), origin.line))
            .copied()
            .unwrap_or_default()
    }

    /// The occurrence step `pos` was given when it was marked.
    pub(crate) fn of(seen: Option<&Seen>, pos: usize) -> usize {
        seen.and_then(|s| s.steps.get(&pos).copied())
            .unwrap_or_default()
    }
}

impl Replay {
    /// The answers of `recording`, for a replay running in `root`.
    pub(crate) fn new(recording: &Recording, root: &Path) -> Self {
        let cwd = Path::new(&recording.header.cwd);
        let mut paths: HashMap<&str, String> = HashMap::new();
        // The occurrence each (host, task uuid) was given, and how many each (host, path) has.
        let mut occurrence: HashMap<(&str, &str), usize> = HashMap::new();
        let mut count: HashMap<(&str, String), usize> = HashMap::new();
        let mut retries: HashMap<(&str, &str), Vec<Value>> = HashMap::new();
        let mut answers = HashMap::new();
        for event in &recording.events {
            let text = |key: &str| event.get(key).and_then(Value::as_str).unwrap_or_default();
            match text("event") {
                "task" => {
                    let path = Path::new(text("path"));
                    let path = path.strip_prefix(cwd).unwrap_or(path);
                    paths.insert(text("uuid"), path.to_string_lossy().into_owned());
                }
                kind @ ("retry" | "result") => {
                    let (host, uuid) = (text("host"), text("task"));
                    let Some(path) = paths.get(uuid) else {
                        continue;
                    };
                    let n = *occurrence.entry((host, uuid)).or_insert_with(|| {
                        let next = count.entry((host, path.clone())).or_default();
                        *next += 1;
                        *next - 1
                    });
                    let result = event.get("result").cloned().unwrap_or(Value::Null);
                    if kind == "retry" {
                        retries.entry((host, uuid)).or_default().push(result);
                        continue;
                    }
                    let item = event
                        .get("item")
                        .and_then(Value::as_u64)
                        .map(|i| i as usize);
                    // A loop's whole-task line: Volant puts its own together from the items.
                    if item.is_none() && result.get("results").is_some() {
                        continue;
                    }
                    let answer = Answer {
                        attempts: retries.remove(&(host, uuid)).unwrap_or_default(),
                        result,
                        status: text("status").to_owned(),
                    };
                    answers.insert((host.to_owned(), path.clone(), n, item), answer);
                }
                _ => {}
            }
        }
        Replay {
            answers,
            root: root.to_path_buf(),
            truncated: recording.truncated,
            watched: std::sync::Mutex::default(),
        }
    }

    /// What the host answered for `task`'s item `item` at attempt `attempt` (from 1), as a
    /// module's answer: the keys ansible-core adds after the module are removed, so Volant
    /// applies `changed_when`, `failed_when`, `until` and the loop keys itself. `Err` is a host
    /// the recording has as unreachable.
    pub(crate) fn answer(
        &self,
        host: &str,
        task: &PlayTask,
        occurrence: usize,
        item: usize,
        attempt: usize,
    ) -> Result<TaskResult, String> {
        let path = self.path_of(task.origin.as_ref());
        let key = |item| (host.to_owned(), path.clone(), occurrence, item);
        let (answer, looped) = match self.answers.get(&key(Some(item))) {
            Some(answer) => (answer, true),
            None => match self.answers.get(&key(None)).filter(|_| item == 0) {
                Some(answer) => (answer, false),
                None if self.truncated => {
                    return Err("the recording is truncated before this task".to_owned());
                }
                None => {
                    return Ok(TaskResult::failed_with(format!(
                        "the recording has no result for {path} on {host}"
                    )));
                }
            },
        };
        let raw = answer
            .attempts
            .get(attempt.saturating_sub(1))
            .unwrap_or(&answer.result);
        if answer.status == "unreachable" && attempt > answer.attempts.len() {
            let msg = raw.get("msg").and_then(Value::as_str).unwrap_or_default();
            return Err(format!("recorded as unreachable: {msg}"));
        }
        // A `no_log` result keeps only its status: the answer is that status, which Volant
        // hides again under the task's own `no_log`. What else it held was never recorded, so
        // any other key read from it is unknown: see `crate::template::unknown::CENSORED`.
        if raw.get("censored") == Some(&Value::Bool(true)) {
            let mut map = Map::new();
            map.insert(crate::template::unknown::CENSORED.into(), Value::Bool(true));
            map.insert("changed".into(), Value::Bool(answer.status == "changed"));
            if answer.status == "failed" {
                map.insert("failed".into(), Value::Bool(true));
            }
            return Ok(TaskResult(map));
        }
        let Value::Object(map) = raw else {
            return Ok(TaskResult::failed_with(format!(
                "the recording's result for {path} on {host} is not an object"
            )));
        };
        let mut reply = module_answer(map, looped);
        // An `until` that reads an unknown value ends at the attempt the recording ended at.
        if !task.until.is_empty() && attempt > answer.attempts.len() {
            reply.insert(FINAL.into(), Value::Bool(true));
        }
        Ok(TaskResult(reply))
    }

    /// `origin`'s `file:line`, relative to the replay's directory as the recording's paths are.
    pub(super) fn path_of(&self, origin: Option<&Origin>) -> String {
        let Some(origin) = origin else {
            return String::new();
        };
        let path = Path::new(&*origin.path);
        let path = path.strip_prefix(&self.root).unwrap_or(path);
        format!("{}:{}", path.to_string_lossy(), origin.line)
    }
}

thread_local! {
    /// The recorded step a replay prepares on this thread: see [`preparing`].
    static STEP: std::cell::RefCell<Option<(Arc<Replay>, String, usize)>> =
        const { std::cell::RefCell::new(None) };
}

/// Runs `prepare`, the synchronous preparation of `task` on `host`, with the step the recording
/// has for it in scope, so a `when` or a `loop` that reads an unknown value takes the recorded
/// branch: see [`recorded_skip`] and [`recorded_items`].
pub(crate) fn preparing<T>(
    replay: Option<&Arc<Replay>>,
    host: &str,
    occurrence: usize,
    prepare: impl FnOnce() -> T,
) -> T {
    let Some(replay) = replay else {
        return prepare();
    };
    let step = (Arc::clone(replay), host.to_owned(), occurrence);
    let before = STEP.with(|s| s.borrow_mut().replace(step));
    let out = prepare();
    STEP.with(|s| *s.borrow_mut() = before);
    out
}

/// Whether the recording skipped item `item` of `task`, in the step being prepared.
pub(crate) fn recorded_skip(task: &PlayTask, item: usize) -> Option<bool> {
    STEP.with(|s| {
        let s = s.borrow();
        let (replay, host, occurrence) = s.as_ref()?;
        let path = replay.path_of(task.origin.as_ref());
        let key = |item| (host.clone(), path.clone(), *occurrence, item);
        let answer = replay
            .answers
            .get(&key(Some(item)))
            .or_else(|| replay.answers.get(&key(None)).filter(|_| item == 0))?;
        Some(answer.status == "skipped")
    })
}

/// The items the recording looped over for `task`, in the step being prepared.
pub(crate) fn recorded_items(task: &PlayTask) -> Option<Vec<Value>> {
    STEP.with(|s| {
        let s = s.borrow();
        let (replay, host, occurrence) = s.as_ref()?;
        let path = replay.path_of(task.origin.as_ref());
        let mut items: Vec<(usize, Value)> = replay
            .answers
            .iter()
            .filter(|((h, p, o, _), _)| h == host && *p == path && o == occurrence)
            .filter_map(|((.., i), a)| Some((((*i)?), a.result.get("item")?.clone())))
            .collect();
        items.sort_by_key(|(i, _)| *i);
        (!items.is_empty()).then(|| items.into_iter().map(|(_, v)| v).collect())
    })
}

/// `result` without what ansible-core adds after the module ran. `changed` and `failed` stay as
/// ansible-core decided them: Volant decides them again from `changed_when` and `failed_when`,
/// except when those read an unknown value, where the recorded decision is the one kept.
fn module_answer(result: &Map<String, Value>, looped: bool) -> Map<String, Value> {
    const LOOP_KEYS: [&str; 4] = [
        "item",
        "ansible_loop_var",
        "ansible_index_var",
        "ansible_loop",
    ];
    result
        .iter()
        .filter(|(key, _)| {
            !(key.starts_with("_ansible_")
                || *key == "failed_when_result"
                || *key == "attempts"
                || (looped && LOOP_KEYS.contains(&key.as_str())))
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

#[cfg(test)]
#[path = "replay_tests.rs"]
mod tests;
