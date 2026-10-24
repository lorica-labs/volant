// SPDX-License-Identifier: GPL-3.0-or-later
//! The `adhoc` command: one module run against the hosts of a pattern, as `ansible` of
//! ansible-core 2.19.12 runs it (`cli/adhoc.py`), its results shown by the `minimal` callback.

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::{Map, Value};
use volant_protocol::TaskResult;

use super::adhoc_usage::refuse;
use super::args::{AdhocArgs, PlaybookArgs};
use super::hosts::{limit_names, play_hosts};
use super::plays::Plays;
use super::run::drive;
use crate::config::Config;
use crate::inventory::{Host, Inventory};
use crate::playbook::{self, Origin, Playbook, TaskOrBlock};
use crate::render::{Cause, Dump, ErrorBlock, Renderer, task_failed};
use crate::splitter::{parse_kv, parse_kv_raw};
use crate::stats::{Outcome, Refusal};

/// Runs `args` and returns the process exit code.
pub fn adhoc(args: AdhocArgs) -> i32 {
    if args.forks.is_some_and(|forks| forks < 1) {
        return refuse("The number of processes (--forks) must be >= 1", false);
    }
    // `-vv` and above print what the default callback prints at those levels, which is not the
    // reference's ad hoc output.
    if args.verbose > 1 {
        eprintln!("ERROR! -vv and above are not supported yet for ad hoc commands");
        return 4;
    }
    let adhoc = match Adhoc::new(&args.pattern, &args.module_name, &args.module_args) {
        Ok(mut adhoc) => {
            adhoc.list_hosts = args.list_hosts;
            adhoc.dir.clone_from(&args.playbook_dir);
            adhoc
        }
        Err((code, message)) => {
            eprintln!("[ERROR]: {message}");
            return code;
        }
    };
    let code = drive(&args.playbook_args(), &Plays::Adhoc(&adhoc));
    // Measured: a run with failed and unreachable hosts exits 4. The reference's strategy returns
    // `RUN_UNREACHABLE_HOSTS` before it looks at the failed ones, where a playbook's codes add up.
    if code == 6 { 4 } else { code }
}

/// The `minimal` callback's `result_format` option, checked against its choices when the
/// callback loads: `Err` is the exit code of a value it refuses, already said. An empty value is
/// not a choice either (measured: exit 5).
pub(super) fn result_format(config: &Config, out: &mut Renderer) -> Result<(), i32> {
    match config.callback_result_format.as_deref() {
        None | Some("json") => Ok(()),
        Some("yaml") => {
            out.set_result_yaml();
            Ok(())
        }
        Some(other) => {
            eprintln!(
                "[ERROR]: Invalid value '{other}' for config 'result_format' for 'minimal' callback plugin. Valid values are: json, yaml"
            );
            Err(5)
        }
    }
}

/// The one task `_play_ds` builds from `-m` and `-a`, and the hosts it runs on.
pub(super) struct Adhoc {
    pattern: String,
    module: String,
    args: Map<String, Value>,
    /// `--list-hosts`: the hosts are listed and nothing runs.
    list_hosts: bool,
    /// `--playbook-dir`: where a playbook would have sat.
    dir: Option<PathBuf>,
    /// A module that reads `-a` as a command line, given none.
    missing_args: bool,
}

/// `C.MODULE_REQUIRE_ARGS`: the modules whose `-a` is read with `check_raw`, so that only their
/// own options leave the command line.
const REQUIRE_ARGS: [&str; 20] = [
    "command",
    "ansible.builtin.command",
    "ansible.legacy.command",
    "raw",
    "ansible.builtin.raw",
    "ansible.legacy.raw",
    "script",
    "ansible.builtin.script",
    "ansible.legacy.script",
    "shell",
    "ansible.builtin.shell",
    "ansible.legacy.shell",
    "win_command",
    "ansible.builtin.win_command",
    "ansible.legacy.win_command",
    "win_shell",
    "ansible.builtin.win_shell",
    "ansible.legacy.win_shell",
    "ansible.windows.win_command",
    "ansible.windows.win_shell",
];

/// `C._ACTION_ALL_INCLUDE_ROLE_TASKS`: the actions whose task carries no `async_val` and `poll`.
const INCLUDES: [&str; 6] = [
    "include_role",
    "ansible.builtin.include_role",
    "ansible.legacy.include_role",
    "include_tasks",
    "ansible.builtin.include_tasks",
    "ansible.legacy.include_tasks",
];

impl Adhoc {
    /// The task of `module` with `raw` read as `_play_ds` reads it: a JSON object when it starts
    /// with `{` and ends with `}`, `key=value` words otherwise. `Err` holds the exit code and the
    /// message of an `-a` that reads as neither.
    pub(super) fn new(pattern: &str, module: &str, raw: &str) -> Result<Self, (i32, String)> {
        Ok(Self {
            pattern: pattern.to_string(),
            module: module.to_string(),
            args: module_args(module, raw)?,
            list_hosts: false,
            dir: None,
            missing_args: raw.is_empty() && REQUIRE_ARGS.contains(&module),
        })
    }

    /// The path the play is read from, so that its directory is `--playbook-dir`; nothing at all
    /// when it was not given, and the directory is the working one.
    pub(super) fn file(&self) -> PathBuf {
        self.dir
            .as_ref()
            .map_or_else(PathBuf::new, |dir| dir.join("<adhoc>"))
    }

    /// What `--limit` names, and what the reference settles once the pattern is resolved, before
    /// it runs anything. `Err(code)` ends the command. The hosts are listed for `--list-hosts`; a
    /// pattern that leaves no host is a warning, or the error of a `--limit` that leaves none
    /// (exit 1), unless the inventory has no host at all; and a command module with no `-a` is
    /// refused (exit 5).
    pub(super) fn settle_hosts(
        &self,
        args: &PlaybookArgs,
        playbooks: &[Playbook],
        inventory: &Inventory,
        out: &mut Renderer,
    ) -> anyhow::Result<Result<Option<HashSet<String>>, i32>> {
        let limit =
            limit_names(args, inventory, out).map_err(|e| Refusal::plain(1, format!("{e:#}")))?;
        let Some(play) = playbooks.first().and_then(|pb| pb.plays.first()) else {
            return Ok(Ok(limit));
        };
        let limit_ref = limit.as_ref();
        Ok(match self.settle(play, inventory, limit_ref, out)? {
            Some(code) => Err(code),
            None => Ok(limit),
        })
    }

    fn settle(
        &self,
        play: &playbook::Play,
        inventory: &Inventory,
        limit: Option<&HashSet<String>>,
        out: &mut Renderer,
    ) -> anyhow::Result<Option<i32>> {
        let found = inventory.resolve(&play.hosts);
        let reaches = |host: &Host| limit.is_none_or(|allowed| allowed.contains(&host.name));
        let empty = !found.hosts.iter().any(reaches);
        if !empty && !self.list_hosts && !self.missing_args {
            return Ok(None);
        }
        // Prints the warnings of the pattern, once: the run's own pass has not happened.
        let hosts = play_hosts(inventory, play, limit, out, &mut HashSet::new(), false)?;
        let bare = inventory.resolve("all").hosts.is_empty();
        if empty && !bare && limit.is_some() {
            return Err(Refusal::plain(
                1,
                "Specified inventory, host pattern and/or --limit leaves us with no hosts to target."
                    .into(),
            ));
        }
        if empty && !bare && self.list_hosts {
            out.warning("No hosts matched, nothing to do", false);
        }
        if self.list_hosts {
            println!("  hosts ({}):", hosts.len());
            for host in &hosts {
                println!("    {}", host.name);
            }
            return Ok(Some(0));
        }
        if self.missing_args {
            eprintln!("[ERROR]: No argument passed to {} module", self.module);
            return Ok(Some(5));
        }
        if !bare {
            out.warning("No hosts matched, nothing to do", false);
        }
        Ok(Some(0))
    }

    pub(super) fn module(&self) -> &str {
        &self.module
    }

    /// Where the reference says the task comes from in an error block, and the task as it shows
    /// it there: `{'action': ..., 'args': ..., 'timeout': 0, 'async_val': 0, 'poll': 15}`.
    fn task_origin(&self) -> Origin {
        let mut ds = Map::new();
        ds.insert("action".into(), self.module.clone().into());
        ds.insert("args".into(), Value::Object(self.args.clone()));
        ds.insert("timeout".into(), 0.into());
        if !INCLUDES.contains(&self.module.as_str()) {
            ds.insert("async_val".into(), 0.into());
            ds.insert("poll".into(), 15.into());
        }
        Origin::described(
            &format!("<adhoc {} task>", repr(&self.module)),
            &playbook::python_repr(&Value::Object(ds)),
        )
    }

    /// Where the reference says the module name comes from: the `-m` option.
    fn module_origin(&self) -> Origin {
        Origin::described("<CLI option '-m'>", &self.module)
    }

    /// The play `_play_ds` builds: `hosts` the pattern, no facts, the one task. It is written as
    /// YAML, every value a JSON scalar or object, and read by the playbook loader; the places the
    /// loader records point into that text, so they are replaced by the reference's own.
    pub(super) fn playbook(&self) -> anyhow::Result<Playbook> {
        let json = |v: Value| serde_json::to_string(&v).expect("JSON values serialize");
        let text = format!(
            "- name: Ansible Ad-Hoc\n  hosts: {}\n  gather_facts: false\n  tasks:\n  - action: {}\n    args: {}\n",
            json(self.pattern.clone().into()),
            json(self.module.clone().into()),
            json(Value::Object(self.args.clone())),
        );
        let source = match &self.dir {
            Some(_) => self.file().display().to_string(),
            None => "<adhoc>".to_string(),
        };
        let mut parsed = playbook::parse(&text, &source)?;
        for play in &mut parsed.plays {
            play.origin = None;
            for entry in &mut play.tasks {
                if let TaskOrBlock::Task(task) = entry {
                    task.origin = Some(self.task_origin());
                    task.value_origins.clear();
                    task.value_origins
                        .insert("action.value".into(), self.module_origin());
                }
            }
        }
        Ok(parsed)
    }

    /// Whether `err` is the run's refusal of this task's module, a name nothing answers to.
    pub(super) fn is_unresolved(&self, err: &anyhow::Error) -> bool {
        err.downcast_ref::<Refusal>()
            .and_then(|r| r.unresolved.as_ref())
            .is_some_and(|u| u.is_plain(&self.module))
    }

    /// What every host reports for a module nothing answers to, measured: the error block with
    /// its cause at `-m`, then `FAILED!`, and exit 2. The reference fails the action before it
    /// connects, so a host it could not reach fails the same way.
    pub(super) fn fail_unresolved(&self, hosts: &[Host], out: &mut Renderer) -> i32 {
        let msg = format!("Cannot resolve '{}' to an action or module.", self.module);
        let message = task_failed(&msg);
        let block = ErrorBlock {
            message: message.clone(),
            origin: Some(self.task_origin()),
            unknown: None,
            causes: vec![Cause {
                msg,
                origin: Some(self.module_origin()),
                help: None,
            }],
            lead: None,
        };
        let mut result = TaskResult::failed_with(message);
        result.0.insert("changed".into(), false.into());
        for host in hosts {
            out.error(&block);
            out.result(
                &host.name,
                Outcome::Failed,
                &result,
                None,
                Dump::No,
                false,
                None,
            );
        }
        if hosts.is_empty() { 0 } else { 2 }
    }
}

/// `-a` as `_play_ds` reads it. A JSON object that is not valid JSON ends the reference with an
/// unexpected exception, exit 250; words with an unbalanced quote are its parser error, exit 4.
fn module_args(module: &str, raw: &str) -> Result<Map<String, Value>, (i32, String)> {
    if raw.starts_with('{') && raw.ends_with('}') {
        let parsed: Map<String, Value> = serde_json::from_str(raw)
            .map_err(|e| (250, format!("the module arguments are not valid JSON: {e}")))?;
        // An empty object falls through to `parse_kv`, as `if not module_args` lets it.
        if !parsed.is_empty() {
            return Ok(parsed);
        }
    }
    let split = if REQUIRE_ARGS.contains(&module) {
        parse_kv_raw(raw)
    } else {
        parse_kv(raw)
    };
    let (options, rest) = split.map_err(|e| (4, format!("{e:#}")))?;
    let mut args = Map::new();
    for (key, value) in options {
        args.insert(key, value.into());
    }
    if let Some(rest) = rest {
        args.insert("_raw_params".into(), rest.into());
    }
    Ok(args)
}

/// Python's `repr` of a string.
fn repr(text: &str) -> String {
    playbook::python_repr(&Value::String(text.to_string()))
}

#[cfg(test)]
mod tests;
