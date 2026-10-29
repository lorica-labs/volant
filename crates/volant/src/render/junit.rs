// SPDX-License-Identifier: GPL-3.0-or-later
//! The `junit` callback of ansible-core 2.19.12: a JUnit XML file per playbook, written when the
//! run ends, with a test case per task and host. Its options are environment variables alone, as
//! the reference reads them.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use super::recap_json::dumps_indented;
use decimal::exact;
use xml::{Body, Case, Escape, suites};

mod decimal;
mod xml;
use crate::stats::Outcome;

/// What the callback collected, and its options.
pub struct Junit {
    dir: PathBuf,
    task_class: bool,
    relative_to: Option<String>,
    out_of_tree: Option<String>,
    fail_on_change: bool,
    fail_on_ignore: bool,
    include_setup: bool,
    hide_arguments: bool,
    prefix: String,
    escape: Escape,
    playbook: String,
    play: String,
    /// Each task once, in the order it first started, keyed as `profile_tasks` keys them.
    tasks: Vec<TaskData>,
    keys: HashMap<String, usize>,
    current: Option<usize>,
}

struct TaskData {
    name: String,
    path: Option<String>,
    play: String,
    action: String,
    start: f64,
    hosts: Vec<HostData>,
}

struct HostData {
    name: String,
    status: Status,
    finish: f64,
    body: Map<String, Value>,
    pretty: bool,
    /// The error block's message of a failure, which the reference files as an error.
    error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Failed,
    Skipped,
}

/// The wall clock as `time.time()` reads it.
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |since| since.as_secs_f64())
}

/// Whether the controller's Python is older than 3.13, whose minidom escapes otherwise. With no
/// controller to ask, the managed runtime's 3.13 is assumed.
fn older_python() -> bool {
    crate::runtime::resolve::controller_python().is_some_and(|python| {
        std::process::Command::new(python)
            .args(["-c", "import sys; sys.exit(sys.version_info >= (3, 13))"])
            .stdin(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// A `JUNIT_*` flag, read as the reference reads it: `true` in any case is on.
fn flag(name: &str, default: bool) -> bool {
    std::env::var(name).map_or(default, |value| value.eq_ignore_ascii_case("true"))
}

impl Junit {
    /// The callback with its options from the environment, its directory made as the reference
    /// makes it when it loads.
    ///
    /// # Errors
    /// The directory cannot be made.
    pub fn new() -> Result<Self, String> {
        let dir = std::env::var("JUNIT_OUTPUT_DIR").map_or_else(
            |_| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default()
                    .join(".ansible.log")
            },
            PathBuf::from,
        );
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        Ok(Self {
            dir,
            task_class: flag("JUNIT_TASK_CLASS", false),
            relative_to: std::env::var("JUNIT_TASK_RELATIVE_PATH")
                .ok()
                .filter(|path| !path.is_empty()),
            out_of_tree: std::env::var("JUNIT_REPLACE_OUT_OF_TREE_PATH").ok(),
            fail_on_change: flag("JUNIT_FAIL_ON_CHANGE", false),
            fail_on_ignore: flag("JUNIT_FAIL_ON_IGNORE", false),
            include_setup: flag("JUNIT_INCLUDE_SETUP_TASKS_IN_REPORT", true),
            hide_arguments: flag("JUNIT_HIDE_TASK_ARGUMENTS", false),
            prefix: std::env::var("JUNIT_TEST_CASE_PREFIX").unwrap_or_default(),
            escape: Escape {
                legacy: older_python(),
            },
            playbook: String::new(),
            play: String::new(),
            tasks: Vec::new(),
            keys: HashMap::new(),
            current: None,
        })
    }

    pub(super) fn playbook(&mut self, file: &std::path::Path) {
        self.playbook = file
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
    }

    pub(super) fn play(&mut self, name: &str) {
        self.play = name.to_string();
    }

    /// A task starts: recorded once, its name followed by its arguments unless they are hidden.
    pub(super) fn task(&mut self, key: &str, begun: &super::Begun<'_>) {
        if let Some(index) = self.keys.get(key) {
            self.current = Some(*index);
            return;
        }
        let mut name = begun.name.trim().to_string();
        if let Some(args) = begun
            .args
            .filter(|_| !self.hide_arguments && !args_empty(begun.args))
        {
            name.push(' ');
            name.push_str(args);
        }
        self.tasks.push(TaskData {
            name,
            path: begun.path.clone(),
            play: self.play.clone(),
            action: begun.action.to_string(),
            start: now(),
            hosts: Vec::new(),
        });
        self.keys.insert(key.to_string(), self.tasks.len() - 1);
        self.current = Some(self.tasks.len() - 1);
    }

    /// One host's result for the task running. An unreachable host is not recorded, as there.
    pub(super) fn result(
        &mut self,
        host: &str,
        outcome: Outcome,
        body: Map<String, Value>,
        pretty: bool,
        error: Option<&str>,
    ) {
        let Some(task) = self.current.and_then(|index| self.tasks.get_mut(index)) else {
            return;
        };
        let changed = body
            .get("changed")
            .is_some_and(|c| c.as_bool() == Some(true));
        let mut status = match outcome {
            Outcome::Ok | Outcome::Changed => Status::Ok,
            Outcome::Ignored if !self.fail_on_ignore => Status::Ok,
            Outcome::Skipped => Status::Skipped,
            Outcome::Ignored | Outcome::Failed | Outcome::Rescued => Status::Failed,
        };
        if self.fail_on_change && status == Status::Ok && changed {
            status = Status::Failed;
        }
        if status == Status::Failed && task.name.contains("EXPECTED FAILURE") {
            status = Status::Ok;
        } else if task.name.contains("TOGGLE RESULT") {
            status = match status {
                Status::Failed => Status::Ok,
                Status::Ok => Status::Failed,
                Status::Skipped => Status::Skipped,
            };
        }
        if !task.name.starts_with(&self.prefix) && status != Status::Failed {
            return;
        }
        task.hosts.retain(|h| h.name != host);
        task.hosts.push(HostData {
            name: host.to_string(),
            status,
            finish: now(),
            body,
            pretty,
            error: error
                .filter(|_| status == Status::Failed)
                .map(str::to_string),
        });
    }

    /// `_generate_report`: the file `<playbook>-<time>.xml` in the output directory.
    pub(super) fn report(&self) -> Result<(), String> {
        let mut cases = Vec::new();
        for task in &self.tasks {
            let setup = matches!(
                task.action.as_str(),
                "setup"
                    | "ansible.builtin.setup"
                    | "ansible.legacy.setup"
                    | "gather_facts"
                    | "ansible.builtin.gather_facts"
                    | "ansible.legacy.gather_facts"
            );
            if setup && !self.include_setup {
                continue;
            }
            for host in &task.hosts {
                cases.push(self.case(task, host));
            }
        }
        let xml = suites(&self.playbook, &cases, self.escape);
        let file = self.dir.join(format!("{}-{}.xml", self.playbook, now()));
        std::fs::write(&file, xml).map_err(|e| format!("cannot write {}: {e}", file.display()))
    }

    fn case(&self, task: &TaskData, host: &HostData) -> Case {
        let name = format!("[{}] {}: {}", host.name, task.play, task.name);
        let mut classname = task.path.clone().unwrap_or_default();
        if let (Some(base), Some(path)) = (&self.relative_to, &task.path) {
            classname = relative(path, base);
        }
        if let Some(placeholder) = &self.out_of_tree
            && classname.starts_with("../")
        {
            let base = classname.rsplit('/').next().unwrap_or_default().to_string();
            classname = format!("{placeholder}{base}");
        }
        if self.task_class
            && let Some(at) = classname.rfind(".yml:")
            && classname[at + 5..].bytes().all(|b| b.is_ascii_digit())
            && at + 5 < classname.len()
        {
            classname.truncate(at);
        }
        let dump = dumps_indented(
            &Value::Object(host.body.clone()),
            0,
            if host.pretty { 4 } else { 0 },
        );
        let mut case = Case {
            name,
            classname,
            time: exact(host.finish - task.start),
            body: Body::Out(dump.clone()),
        };
        match host.status {
            Status::Ok => {}
            Status::Skipped => {
                let reason = host
                    .body
                    .get("skip_reason")
                    .map_or_else(|| "skipped".to_string(), text);
                case.body = Body::Skipped(reason);
            }
            Status::Failed => {
                // A failure the task raised carries its error, as every failed result does in
                // ansible-core 2.19.12; one an option turned into a failure has none.
                case.body = match &host.error {
                    Some(message) => Body::Error {
                        message: message.clone(),
                        output: traceback(message, host.body.get("exception")),
                    },
                    None => Body::Failure {
                        message: host.body.get("msg").map_or_else(
                            || {
                                format!(
                                    "rc={}",
                                    host.body.get("rc").map_or_else(|| "0".to_string(), text)
                                )
                            },
                            text,
                        ),
                        output: dump,
                    },
                };
            }
        }
        case
    }
}

/// `format_event_traceback` of a failure's error: each link of its chain, the module's own
/// traceback when it sent one, and `(traceback missing)` for the others.
fn traceback(message: &str, exception: Option<&Value>) -> String {
    let first = exception.filter(|e| truthy(e)).map_or_else(
        || "(traceback missing)\n".to_string(),
        |e| {
            let text = text(e);
            if text.ends_with('\n') {
                text
            } else {
                format!("{text}\n")
            }
        },
    );
    let cause = "The above exception was the direct cause of the following exception:";
    // A failure a module or an action plugin returned was captured: one link more.
    let captured = if message.starts_with("Task failed: Module failed") {
        Some("target")
    } else if message.starts_with("Task failed: Action failed") {
        Some("action")
    } else {
        None
    };
    match captured {
        Some(context) => format!(
            "{first}\nThe above {context} exception was the direct cause of the following \
             controller exception:\n\n(traceback missing)\n\n{cause}\n\n(traceback missing)\n"
        ),
        None => format!("{first}\n{cause}\n\n(traceback missing)\n"),
    }
}

/// Whether a task's argument text says nothing.
fn args_empty(args: Option<&str>) -> bool {
    args.is_none_or(str::is_empty)
}

fn truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false))
        && value.as_str().is_none_or(|s| !s.is_empty())
}

/// A value as Python's `str()` writes it.
fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => crate::playbook::python_repr(other),
    }
}

/// `os.path.relpath(path, base)` for two absolute or two relative paths, path separators `/`.
fn relative(path: &str, base: &str) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let absolute = |p: &str| {
        let p = std::path::Path::new(p);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        }
    };
    let (path, base) = (absolute(path), absolute(base));
    let (from, to): (Vec<_>, Vec<_>) = (base.components().collect(), path.components().collect());
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".to_string(); from.len() - common];
    parts.extend(
        to[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}
