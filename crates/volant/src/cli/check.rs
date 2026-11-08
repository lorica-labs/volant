// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant check`: a verdict for every playbook of a repository, with no host touched.
//!
//! Each playbook goes through the stages `volant playbook` runs before its first connection,
//! the same functions in the same order, and the first refusal is its verdict. What only a run
//! can know (an include named by a template, a collection's module with no ansible-core to ask)
//! is `unknown`. Nothing connects, nothing is rendered for a host, nothing is written.

mod discover;
mod follow;
mod report;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Map, Value};

use super::stages;
use crate::bridge::Bridge;
use crate::compile::{Compiled, StepKind, TagSelection};
use crate::config::Config;
use crate::playbook::{self, Origin, PlayTask, TaskOrBlock};
use crate::python::{PythonBuilder, Resolved};
use crate::stats::Refusal;
use crate::template::Templar;
use crate::{preflight, vars};
use report::{Kind, Reason, Verdict};

/// The output formats of `volant check`.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
}

/// Find the playbooks of a repository and say for each one whether Volant can run it, with no
/// host contacted.
#[derive(clap::Args, Debug)]
pub struct CheckArgs {
    /// Files or directories to check. Default: the working directory.
    pub paths: Vec<PathBuf>,
    /// Inventory sources, read for the private key file of each host.
    #[arg(short = 'i', long = "inventory", value_name = "INVENTORY")]
    pub inventory: Vec<String>,
    /// Extra variables, for the names an `import_playbook` renders.
    #[arg(short = 'e', long = "extra-vars", value_name = "VARS")]
    pub extra_vars: Vec<String>,
    /// Never start Python: a collection's module is `unknown` instead of resolved.
    #[arg(long)]
    pub runtime_less: bool,
    /// Exit 1 when a playbook is `unknown`, as when one is `refused`.
    #[arg(long)]
    pub strict: bool,
    #[arg(long, value_enum, default_value = "text")]
    pub format: Format,
    /// The same as `--format json`.
    #[arg(long)]
    pub json: bool,
    /// The most files a check walks.
    #[arg(long, value_name = "N", default_value_t = 5000)]
    pub max_files: usize,
    /// `-v` also counts the YAML files that are not playbooks.
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    pub verbose: u8,
}

/// Runs `volant check` and returns the exit code: 0 when no playbook is refused, 1 when one is
/// (or is `unknown` under `--strict`), 3 when there is no playbook, 2 for a usage error.
pub fn check(args: &CheckArgs) -> i32 {
    match run(args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("[ERROR]: {err:#}");
            2
        }
    }
}

fn run(args: &CheckArgs) -> anyhow::Result<i32> {
    let paths = if args.paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        args.paths.clone()
    };
    let found = discover::find(&paths, args.max_files).map_err(anyhow::Error::msg)?;
    if found.playbooks.is_empty() {
        let named: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        eprintln!("[ERROR]: no playbook found in {}", named.join(", "));
        return Ok(3);
    }
    let config = crate::runtime::resolve::configured(Config::load()?);
    playbook::set_deprecation_warnings(config.deprecation_warnings);
    let extra = vars::parse_extra_vars(&args.extra_vars, &std::env::current_dir()?)?;
    let mut judge = Judge::new(config, extra, args)?;
    let verdicts: Vec<Verdict> = found.playbooks.iter().map(|p| judge.judge(p)).collect();
    if args.json || args.format == Format::Json {
        println!("{}", report::json(&verdicts, found.skipped));
    } else {
        print!(
            "{}",
            report::text(&verdicts, (args.verbose > 0).then_some(found.skipped))
        );
    }
    let failed = verdicts
        .iter()
        .any(|v| v.refused() || (args.strict && !v.reasons.is_empty()));
    Ok(i32::from(failed))
}

/// What every playbook of one check shares.
struct Judge {
    config: Config,
    extra: crate::tags::Tagged,
    bridge: Arc<Bridge>,
    runtime_less: bool,
    /// Started for the first collection module to resolve, and kept: one ansible-core start for
    /// the whole check.
    helper: Option<Result<PythonBuilder, String>>,
    /// The private key files the configuration and the inventory name.
    keys: Vec<PathBuf>,
}

impl Judge {
    fn new(config: Config, extra: crate::tags::Tagged, args: &CheckArgs) -> anyhow::Result<Self> {
        let runtime_less = args.runtime_less;
        // Never installs a runtime: a check writes nothing.
        let bridge = Bridge::with_start(Box::new(move || {
            if runtime_less {
                anyhow::bail!("--runtime-less starts no Python");
            }
            PythonBuilder::start_installing(false)
        }));
        let mut keys: Vec<PathBuf> = config.private_key_file.iter().cloned().collect();
        if !args.inventory.is_empty() {
            let sources = super::announce::inventory_sources(&args.inventory, &config);
            let loaded =
                crate::inventory::load_sources_with(&sources, &config.inventory_plugins, &|| {
                    Arc::clone(&bridge)
                });
            if let Some(fatal) = loaded.error {
                anyhow::bail!(fatal.message);
            }
            for host in loaded.inventory.resolve("all").hosts {
                keys.extend(key_files(host.vars.as_map(), Path::new(".")));
            }
        }
        Ok(Judge {
            config,
            extra,
            bridge,
            runtime_less,
            helper: None,
            keys,
        })
    }

    fn judge(&mut self, path: &Path) -> Verdict {
        let mut verdict = Verdict {
            path: shown(path),
            plays: 0,
            tasks: 0,
            reasons: Vec::new(),
        };
        if let Err(refusal) = self.stages(path, &mut verdict) {
            verdict.reasons.push(refusal);
        }
        // A role two plays run, or a file two statements include, is one reason.
        let mut said = std::collections::HashSet::new();
        verdict
            .reasons
            .retain(|r| said.insert((r.message.clone(), r.file.clone(), r.line)));
        verdict
    }

    /// The stages of `volant playbook` before the first connection, in its order.
    fn stages(&mut self, path: &Path, verdict: &mut Verdict) -> Result<(), Reason> {
        let files = [path.to_path_buf()];
        let rendering = playbook::Rendering::new(Some(Arc::clone(&self.bridge)), &self.extra);
        let mut pb = rendering
            .add_plugin_dirs(&files)
            .and_then(|()| playbook::load_rendering(path, &rendering))
            .map_err(|err| reason_of(&err, None))?;
        verdict.plays = pb.plays.len();
        for play in &mut pb.plays {
            if play.strategy.is_none() {
                play.strategy.clone_from(&self.config.strategy);
            }
        }
        let mut playbooks = vec![pb];
        preflight::settle_strategies(&mut playbooks, &self.bridge);
        preflight::check(&playbooks[0]).map_err(|err| {
            let tasks = playbooks[0].plays.iter().flat_map(|p| own_tasks(p));
            reason_of(&err, first_refused(tasks))
        })?;
        let selection = TagSelection::default();
        let compiled = stages::compile_all(&playbooks, &self.config, &selection, &self.extra)
            .map_err(|err| reason_of(&err, None))?;
        verdict.tasks = compiled
            .iter()
            .flatten()
            .flat_map(|c| &c.steps)
            .filter(|s| !matches!(s.kind, StepKind::Flush { .. }))
            .count();
        stages::check_compiled(&playbooks, &compiled, |_| false).map_err(|err| {
            reason_of(
                &err,
                first_refused(compiled.iter().flatten().flat_map(tasks)),
            )
        })?;
        let expanded = follow::includes(compiled.iter().flatten(), &mut verdict.reasons)?;
        let every = || compiled.iter().flatten().chain(&expanded);
        for task in every().flat_map(tasks) {
            if task.deferred.module.is_some() {
                verdict.reasons.push(unknown(
                    "module named by a template: checked when the run reaches it".into(),
                    task.origin.as_ref(),
                ));
            }
        }
        let reach =
            stages::reach_of(&files, &playbooks, &compiled).map_err(|err| reason_of(&err, None))?;
        let named: Vec<(String, String)> =
            every().flat_map(preflight::collection_modules).collect();
        self.resolve(
            &reach.plugin_dirs,
            &named,
            &every().collect::<Vec<_>>(),
            verdict,
        )?;
        let mut keys = self.keys.clone();
        for play in &playbooks[0].plays {
            keys.extend(key_files(play.vars.as_map(), &play.dir));
        }
        for key in keys {
            if is_vaulted(&key) {
                return Err(refusal(
                    Kind::Unsupported,
                    format!(
                        "the private key file {} is vault-encrypted, and this release cannot decrypt a private key",
                        shown(&key)
                    ),
                    None,
                ));
            }
        }
        // The run refuses this before the first connection too, with the code it measured.
        let method = &self.config.become_method;
        let escalates =
            self.config.r#become || playbooks[0].plays.iter().any(|p| p.r#become == Some(true));
        if escalates && !playbook::BECOME_METHODS.contains(&method.as_str()) {
            let mut reason = refusal(
                Kind::Unsupported,
                format!("become_method '{method}' is not supported yet"),
                None,
            );
            reason.code = Some(2);
            return Err(reason);
        }
        Ok(())
    }

    /// The collection modules `named`, resolved by the controller's ansible-core as the run
    /// resolves them; `unknown` when there is none to ask.
    fn resolve(
        &mut self,
        dirs: &[PathBuf],
        named: &[(String, String)],
        plays: &[&Compiled],
        verdict: &mut Verdict,
    ) -> Result<(), Reason> {
        if named.is_empty() {
            return Ok(());
        }
        let origin = |task: &str, module: &str| {
            plays
                .iter()
                .flat_map(|c| tasks(c))
                .find(|t| t.name == task && t.module == module)
                .and_then(|t| t.origin.clone())
        };
        let runtime_less = self.runtime_less;
        let helper = self.helper.get_or_insert_with(|| {
            if runtime_less {
                return Err("--runtime-less starts no ansible-core to resolve it".into());
            }
            PythonBuilder::start_installing(false).map_err(|err| {
                format!(
                    "no ansible-core on this controller resolves it ({err:#}); volant runtime install sets one up"
                )
            })
        });
        let answers = match helper {
            Ok(helper) => {
                let mut modules: Vec<String> = named.iter().map(|(_, m)| m.clone()).collect();
                modules.sort();
                modules.dedup();
                helper
                    .add_plugin_dirs(dirs)
                    .and_then(|()| helper.resolve(&modules))
                    .map_err(|err| format!("ansible-core could not resolve it: {err:#}"))
            }
            Err(why) => Err(why.clone()),
        };
        let answers = match answers {
            Ok(answers) => answers,
            Err(why) => {
                let mut said = std::collections::HashSet::new();
                for (task, module) in named {
                    if said.insert(module) {
                        verdict.reasons.push(unknown(
                            format!("module '{module}': {why}"),
                            origin(task, module).as_ref(),
                        ));
                    }
                }
                return Ok(());
            }
        };
        for (task, module) in named {
            let Some(answer) = answers.get(module) else {
                continue;
            };
            if let Err(err) = preflight::check_resolved(task, module, answer) {
                let mut reason = reason_of(&err, origin(task, module).as_ref());
                if matches!(answer, Resolved::Missing { .. }) {
                    reason.kind = Kind::Invalid;
                }
                return Err(reason);
            }
        }
        Ok(())
    }
}

/// Every task of a compiled play, its handlers included.
fn tasks(compiled: &Compiled) -> impl Iterator<Item = &PlayTask> {
    compiled
        .steps
        .iter()
        .filter(|s| !matches!(s.kind, StepKind::Flush { .. }))
        .map(|s| &s.task)
        .chain(compiled.handlers.iter().map(|h| &h.task))
}

/// Every task a play writes itself, blocks and handlers included.
fn own_tasks(play: &playbook::Play) -> Vec<&PlayTask> {
    let mut out: Vec<&PlayTask> = play.handlers.iter().map(|h| &h.task).collect();
    for items in [&play.pre_tasks, &play.tasks, &play.post_tasks] {
        out.extend(listed(items));
    }
    out
}

/// Every task of a task list, blocks walked.
pub(super) fn listed(items: &[TaskOrBlock]) -> Vec<&PlayTask> {
    let mut out = Vec::new();
    for item in items {
        match item {
            TaskOrBlock::Task(task) => out.push(task),
            TaskOrBlock::Block(block) => {
                for section in [&block.body, &block.rescue, &block.always] {
                    out.extend(listed(section));
                }
            }
        }
    }
    out
}

/// Where the first task the pre-flight refuses on its own was written.
pub(super) fn first_refused<'a>(
    mut tasks: impl Iterator<Item = &'a PlayTask>,
) -> Option<&'a Origin> {
    tasks
        .find(|t| preflight::check_task(t).is_err())
        .and_then(|t| t.origin.as_ref())
}

/// The private key files `vars` names, relative to `dir`, when written as plain text.
fn key_files(vars: &Map<String, Value>, dir: &Path) -> Vec<PathBuf> {
    ["ansible_private_key_file", "ansible_ssh_private_key_file"]
        .iter()
        .filter_map(|name| vars.get(*name).and_then(Value::as_str))
        .filter(|path| !Templar::is_template(path))
        .map(|path| dir.join(path))
        .collect()
}

/// Whether `path` is a vault-encrypted file. Read, never decrypted.
fn is_vaulted(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 14];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut head))
        .is_ok_and(|()| &head == b"$ANSIBLE_VAULT")
}

/// `path` as the report prints it: relative to the working directory when under it.
fn shown(path: &Path) -> String {
    // Absolute, with `.` and `..` folded out: an imported playbook's tasks name `a/../b.yml`.
    let path = &crate::roles::folded(path);
    // A play's directory is canonical, and so a verbatim path on Windows.
    let cwd = std::env::current_dir().ok();
    let canonical = cwd
        .as_deref()
        .and_then(|cwd| std::fs::canonicalize(cwd).ok());
    let path = [cwd, canonical]
        .iter()
        .flatten()
        .find_map(|base| path.strip_prefix(base).ok())
        .unwrap_or(path);
    path.display().to_string().replace('\\', "/")
}

/// Whether a refusal's message says this engine lacks something, rather than that the playbook
/// is wrong. Every pre-flight sentence says one of these.
fn says_unsupported(message: &str) -> bool {
    ["not supported", "this release", "Volant"]
        .iter()
        .any(|marker| message.contains(marker))
}

fn refusal(kind: Kind, message: String, at: Option<&Origin>) -> Reason {
    Reason {
        kind,
        code: Some(4),
        message,
        file: at.map(|o| shown(Path::new(&*o.path))),
        line: at.map(|o| o.line).filter(|line| *line > 0),
    }
}

fn unknown(message: String, at: Option<&Origin>) -> Reason {
    Reason {
        code: None,
        ..refusal(Kind::Unknown, message, at)
    }
}

/// The reason a stage's error gives: its code, its first line, and the place it names or `at`.
fn reason_of(err: &anyhow::Error, at: Option<&Origin>) -> Reason {
    let text = err
        .chain()
        .find_map(|e| e.downcast_ref::<Refusal>())
        .map_or_else(|| format!("{err:#}"), |r| r.message.clone());
    let message = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .to_string();
    // An error block names its place on an `Origin:` line.
    let named = text.lines().find_map(|line| {
        let place = line.trim().strip_prefix("Origin: ")?;
        let mut parts = place.rsplitn(3, ':');
        let (_col, line, file) = (parts.next()?, parts.next()?, parts.next()?);
        Some((shown(Path::new(file)), line.parse().ok()?))
    });
    let kind = if says_unsupported(&message) {
        Kind::Unsupported
    } else {
        Kind::Invalid
    };
    let mut reason = refusal(kind, message, at);
    reason.code = Some(crate::stats::error_code(err));
    if let Some((file, line)) = named {
        reason.file = Some(file);
        reason.line = Some(line);
    }
    reason
}
