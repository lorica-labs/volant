// SPDX-License-Identifier: GPL-3.0-or-later
//! The `adhoc` command: one module run against the hosts of a pattern, as `ansible` of
//! ansible-core 2.19.12 runs it (`cli/adhoc.py`), its results shown by the `minimal` callback.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::PathBuf;

use serde_json::{Map, Value};
use volant_protocol::TaskResult;

use super::adhoc_options::{deprecated_options, tree_dir};
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
    // argparse acts on `--version` as it reads it, before the checks that follow the parse.
    if args.version {
        println!("{}", super::version::block("ansible", &[]));
        return 0;
    }
    if args.forks.is_some_and(|forks| forks < 1) {
        return refuse("The number of processes (--forks) must be >= 1", false);
    }
    // Clap requires the pattern unless `--version` was given.
    let pattern = args.pattern.clone().unwrap_or_default();
    // The reference accepts them: a negative `-B` or `-P` starts the job without polling, and a
    // negative `--task-timeout` fails the task on every host. This release refuses them by name.
    if let Some(flag) = [
        ("-B", args.background),
        ("-P", args.poll),
        ("--task-timeout", args.task_timeout),
    ]
    .into_iter()
    .find_map(|(flag, value)| value.filter(|v| *v < 0).map(|_| flag))
    {
        eprintln!("ERROR! a negative {flag} is not supported yet for ad hoc commands");
        return 4;
    }
    // The reference starts the job for real under `-C`; this release does not.
    if args.check && args.background.is_some_and(|seconds| seconds != 0) {
        eprintln!("ERROR! -C with -B is not supported yet for ad hoc commands");
        return 4;
    }
    let tree = match args.tree.as_deref().map(tree_dir).transpose() {
        Ok(tree) => tree,
        Err(what) => {
            eprintln!("ERROR! {what} is not supported yet for ad hoc commands");
            return 4;
        }
    };
    let adhoc = match Adhoc::new(&pattern, &args.module_name, &args.module_args) {
        Ok(mut adhoc) => {
            adhoc.one_line = args.one_line;
            adhoc.tree = tree;
            adhoc.list_hosts = args.list_hosts;
            adhoc.dir.clone_from(&args.playbook_dir);
            adhoc.background = args.background.unwrap_or(0).unsigned_abs();
            adhoc.poll = args.poll.map(i64::unsigned_abs);
            adhoc.timeout = args.task_timeout.unwrap_or(0).unsigned_abs();
            adhoc
        }
        Err((code, message)) => {
            eprintln!("[ERROR]: {message}");
            return code;
        }
    };
    let mut playbook_args = args.playbook_args();
    playbook_args.adhoc_deprecations =
        deprecated_options(&args, &std::env::args_os().collect::<Vec<_>>());
    let code = drive(&playbook_args, &Plays::Adhoc(&adhoc));
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
    /// `-B`, `-P` and `--task-timeout`: the task's `async`, `poll` and `timeout`.
    background: u64,
    poll: Option<u64>,
    timeout: u64,
    /// `-o`: the results are shown by the `oneline` callback.
    one_line: bool,
    /// `-t`: the directory the `tree` callback writes one file per host in, already absolute.
    tree: Option<PathBuf>,
    /// The play keywords of a console command (`remote_user`, `become`, ...), whose task the
    /// reference builds without `async_val` and `poll` and places nowhere.
    console: Option<Map<String, Value>>,
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
            background: 0,
            poll: None,
            timeout: 0,
            one_line: false,
            tree: None,
            console: None,
        })
    }

    /// The task as `ansible-console` builds it, under the session's play keywords.
    pub(super) fn in_console(
        mut self,
        keywords: Map<String, Value>,
        timeout: u64,
        dir: Option<PathBuf>,
    ) -> Self {
        self.console = Some(keywords);
        self.timeout = timeout;
        self.dir = dir;
        self
    }

    /// Hands the output options to the renderer.
    pub(super) fn show(&self, out: &mut Renderer) {
        out.set_oneline(self.one_line, self.tree.clone());
    }

    /// Whether `-o` and `-t` load a callback of their own.
    pub(super) fn callbacks(&self) -> (bool, bool) {
        (self.one_line, self.tree.is_some())
    }

    /// The notices of the callbacks `-o` and `-t` load, which the reference prints when the run
    /// starts: after the warnings of the pattern, and for no run that ends before it.
    fn deprecate_callbacks(&self) {
        for (shown, name) in [(self.one_line, "oneline"), (self.tree.is_some(), "tree")] {
            if shown {
                playbook::print_deprecation_line(&format!(
                    "The {name} callback plugin is deprecated. This feature will be removed from ansible-core version 2.23."
                ));
            }
        }
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
        let runs = !empty && !self.list_hosts && !self.missing_args;
        if runs && !self.one_line && self.tree.is_none() {
            return Ok(None);
        }
        // Prints the warnings of the pattern, once: the run's own pass has not happened, and with
        // a callback to load the notices follow them.
        let hosts = play_hosts(inventory, play, limit, out, &mut HashSet::new(), false)?;
        if runs {
            self.deprecate_callbacks();
            return Ok(None);
        }
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
        self.deprecate_callbacks();
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
        ds.insert("timeout".into(), self.timeout.into());
        if self.console.is_some() {
            return Origin::described("<unknown>", &playbook::python_repr(&Value::Object(ds)));
        }
        if !INCLUDES.contains(&self.module.as_str()) {
            ds.insert("async_val".into(), self.background.into());
            ds.insert("poll".into(), self.poll.unwrap_or(15).into());
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
        let mut keywords = String::new();
        for (name, value) in [
            ("async", self.background),
            ("poll", self.poll.unwrap_or(0)),
            ("timeout", self.timeout),
        ] {
            if value != 0 || (name == "poll" && self.poll == Some(0)) {
                writeln!(keywords, "    {name}: {value}").expect("a String takes a write");
            }
        }
        // `meta` reads its action from the statement, where the other modules read their options
        // from `args`: `-a noop` is `meta noop`.
        let meta = ["meta", "ansible.builtin.meta", "ansible.legacy.meta"]
            .contains(&self.module.as_str())
            .then(|| self.args.get("_raw_params").and_then(Value::as_str))
            .flatten()
            .filter(|_| self.args.len() == 1);
        let (action, args) = match meta {
            Some(action) => (format!("{} {action}", self.module), Map::new()),
            None => (self.module.clone(), self.args.clone()),
        };
        let mut play_keywords = String::new();
        for (name, value) in self.console.iter().flatten() {
            writeln!(play_keywords, "  {name}: {}", json(value.clone()))
                .expect("a String takes a write");
        }
        let text = format!(
            "- name: Ansible Ad-Hoc\n  hosts: {}\n  gather_facts: false\n{play_keywords}  tasks:\n  - action: {}\n    args: {}\n{keywords}",
            json(self.pattern.clone().into()),
            json(action.into()),
            json(Value::Object(args)),
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
            on_stderr: false,
        };
        let mut result = TaskResult::failed_with(message);
        result.0.insert("changed".into(), false.into());
        // What `oneline` says of a failure, and the reference's placeholder for it.
        result
            .0
            .insert("exception".into(), "(traceback unavailable)".into());
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
