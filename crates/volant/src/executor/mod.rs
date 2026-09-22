// SPDX-License-Identifier: GPL-3.0-or-later
//! Runs one play on its hosts with the `linear` strategy: consecutive remote tasks are grouped
//! into batches, `set_fact` and `debug` run on the controller, and output is shown task by task,
//! once every live host has reported that task. This file holds what the modules share - the
//! run's settings, the connections kept between plays, and the entry point the command line
//! calls.

mod coordinator;
mod driver;
mod include;
mod prepare;
mod report;
mod run;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::sync::watch;

use crate::agent::{AgentLink, AgentSource};
use crate::compile::Compiled;
use crate::inventory::Host;
use crate::playbook::Play;
use crate::render::Renderer;
use crate::stats::Stats;
use crate::template::{Templar, Vars};
use crate::transport::{ConnectionDefaults, Transport};
use crate::vars::{Scope, VarStore, load_vars_file};

use coordinator::run_batch;

/// Ansible's default `timeout`: seconds to establish a connection.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a host gets to confirm a cancel before it is abandoned.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Settings shared by every play of a run.
#[derive(Clone)]
pub struct RunOptions {
    /// Connection settings every host starts from; its host variables override them.
    pub defaults: ConnectionDefaults,
    /// How many hosts of a play run at once, Ansible's `forks`. Never zero.
    pub forks: usize,
    /// `--force-handlers`, or `[defaults] force_handlers`: whether a host that failed still runs
    /// the handlers it notified. A play saying so itself speaks over this.
    pub force_handlers: bool,
    /// `[volant] batching`, or `VOLANT_BATCHING`: whether a host may carry on through the tasks
    /// between two synchronisation points. Off by default, so the hosts of a batch meet in front
    /// of every task, which is what `linear` means.
    pub batching: bool,
    /// Flips to `true` once when the user interrupts the run.
    pub stop: watch::Receiver<bool>,
}

/// Which agent a kept connection belongs to. The escalated user is part of the identity
/// because two connections to one host under two users are two different agents, and so is the
/// transport: connection settings are ordinary variables, so two tasks of one host can resolve
/// different addresses, ports, users or keys, and a link reused across that difference would
/// send a task to the wrong machine.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LinkKey {
    pub host: String,
    pub become_user: Option<String>,
    pub transport: Transport,
}

/// What outlives a play: the templar, the variable store hosts write into, the hosts that
/// are out of the run, and the agents still connected.
pub struct RunState {
    pub templar: Arc<Templar>,
    pub vars: Arc<Mutex<VarStore>>,
    pub failed_hosts: HashSet<String>,
    pub verbosity: u8,
    /// Agents kept alive between plays, the way Ansible keeps its ssh connections open. Only
    /// the connecting user's own link per host is kept; see `keep_links`.
    pub links: HashMap<LinkKey, AgentLink>,
    /// The module payloads this run sends, built once before the first connection from every
    /// Python module the compiled plays name. `None` for a run of native tasks alone, which is
    /// why a controller without ansible-core still runs those.
    pub python: Option<Arc<crate::python::Union>>,
}

impl RunState {
    /// Closes every kept connection, concurrently: each `shutdown` waits up to a couple of
    /// seconds for its own agent, and a run of N hosts closing them one at a time would pay
    /// N times that before the recap ever shows. Draining the map makes a second call a no-op,
    /// so the recap paths can each ask for it without closing one agent twice.
    pub async fn shutdown_links(&mut self) {
        let mut closing = tokio::task::JoinSet::new();
        for (_, link) in self.links.drain() {
            closing.spawn(link.shutdown());
        }
        while closing.join_next().await.is_some() {}
    }
}

/// How a play ended, for the run that contains it.
pub struct PlayEnd {
    /// Every live host of a batch failed, so the run is over: no further batch of this play, no
    /// further play, and the recap prints where it stands. Measured on ansible-core 2.19.12,
    /// three shapes of the same rule - two hosts failing in a one-batch play, one host failing
    /// in a `serial: 1` batch, and a whole batch going unreachable - and the run stops at all
    /// three, at exit 2 for the failures and 4 for the unreachable batch.
    pub stop_run: bool,
}

/// Runs one play, batch by batch.
///
/// `hosts` is the play's pattern resolved and narrowed by `--limit`, with **nothing filtered
/// out**: the hosts that failed in an earlier play are still in it, because the reference cuts
/// its batches from that list and only then drops the failures from each batch. Cutting the
/// filtered list instead merges two batches into one whenever an earlier play lost a host.
#[expect(
    clippy::too_many_arguments,
    reason = "the play's whole context: eight values with no natural grouping until the controller is split"
)]
pub async fn run_play(
    play: &Play,
    compiled: &Compiled,
    hosts: Vec<Host>,
    agents: &AgentSource,
    options: &RunOptions,
    state: &mut RunState,
    out: &mut Renderer,
    stats: &mut Stats,
) -> anyhow::Result<PlayEnd> {
    // A pattern that matched nothing is the one case with no batch at all: the reference prints
    // the banner and says so, and the play keyword that would have cut the batches is never read.
    if hosts.is_empty() {
        out.play(&play.name);
        out.no_hosts();
        return Ok(PlayEnd { stop_run: false });
    }
    let play_vars = {
        let store = state.vars.lock().expect("vars lock");
        store.play_scope(&play.vars)
    };
    let batches =
        crate::compile::batches(&hosts, play.serial.as_ref(), &state.templar, &play_vars)?;
    let all: Vec<String> = hosts.iter().map(|h| h.name.clone()).collect();
    let playbook_dir = state
        .vars
        .lock()
        .expect("vars lock")
        .playbook_dir()
        .to_path_buf();
    for batch in batches {
        // A batch that starts after the operator interrupted the run is a batch that should not
        // start: the drivers of the batch before it have already stopped for the same reason.
        if *options.stop.borrow() {
            break;
        }
        // One banner per batch, measured - and it prints even when the batch has nothing left to
        // run, which is the shape a play gets when every host it resolved has already failed.
        out.play(&play.name);
        let live: Vec<Host> = batch
            .into_iter()
            .filter(|h| !state.failed_hosts.contains(&h.name))
            .collect();
        if live.is_empty() {
            continue;
        }
        // Read here, once per batch about to run and not once for the whole play: see
        // `load_play_vars_files`. `play_hosts` is the play's still-live hosts computed from
        // `state.failed_hosts` as it stands right before this batch starts (this batch and every
        // batch still to come); `batch_hosts` is this batch's own live hosts; `all` stays the
        // play's whole resolved list, batches and failures alike.
        let play_hosts: Vec<String> = hosts
            .iter()
            .filter(|h| !state.failed_hosts.contains(&h.name))
            .map(|h| h.name.clone())
            .collect();
        let batch_hosts: Vec<String> = live.iter().map(|h| h.name.clone()).collect();
        let vars_files = load_play_vars_files(
            play,
            &live,
            &play_hosts,
            &batch_hosts,
            &all,
            &playbook_dir,
            state,
            out,
        )?;
        run_batch(
            play,
            compiled,
            &live,
            &all,
            &vars_files,
            agents,
            options,
            state,
            out,
            stats,
        )
        .await?;
        // The run ends where the reference ends it: a batch that had hosts to run and lost every
        // one of them. A batch that had none to begin with is not that - measured, `serial: 1`
        // over a host that failed in an earlier play prints its banner and the next batch runs.
        if live.iter().all(|h| state.failed_hosts.contains(&h.name)) {
            return Ok(PlayEnd { stop_run: true });
        }
    }
    Ok(PlayEnd { stop_run: false })
}

/// A variable's boolean, whether the inventory typed it as one or spelled it the way Ansible
/// content spells one (`yes`, `on`, `"true"`).
pub(crate) fn as_bool_value(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::String(s) => crate::yaml::bool_from_str(s.trim()),
        _ => None,
    }
}

/// `vars_files` paths are templates over play vars and the host's own variables, and Ansible
/// resolves them per host, so two hosts can read two different files. Each rendered path is
/// read once per call.
///
/// Called once per batch, not once per play: measured against ansible-core 2.19.12, a
/// `vars_files` path templated on `ansible_play_batch` reads a different file for each batch
/// (`serial/two.yml`'s shape, extended with a `vars_files` entry), so the reference re-renders
/// and re-reads it per batch rather than once at the play's start. `hosts` is therefore the
/// batch's own live hosts, and `play_hosts` / `batch_hosts` / `all_play_hosts` are the same three
/// lists `ansible_play_hosts`, `ansible_play_batch` and `ansible_play_hosts_all` are served from
/// everywhere else.
///
/// An entry that names nothing is not an error: the reference skips a `vars_files` path that
/// does not exist without a word and runs the play, and warns once for an entry whose template
/// has no value. Every other template failure stops the run, as it does there. A file that is
/// there but cannot be read stops the run too.
#[expect(
    clippy::too_many_arguments,
    reason = "three host lists the reference distinguishes; folding them loses which is which"
)]
fn load_play_vars_files(
    play: &Play,
    hosts: &[Host],
    play_hosts: &[String],
    batch_hosts: &[String],
    all_play_hosts: &[String],
    playbook_dir: &Path,
    state: &RunState,
    out: &mut Renderer,
) -> anyhow::Result<HashMap<String, Vec<Map<String, Value>>>> {
    let mut per_host = HashMap::new();
    if play.vars_files.is_empty() {
        return Ok(per_host);
    }
    let mut loaded: HashMap<PathBuf, Map<String, Value>> = HashMap::new();
    // One warning per entry, not one per host: two hosts reading the same unresolvable entry
    // are one complaint.
    let mut warned: HashSet<&String> = HashSet::new();
    for host in hosts {
        let scope = Scope {
            play_vars: play.vars.clone(),
            play_hosts: play_hosts.to_vec(),
            batch_hosts: batch_hosts.to_vec(),
            all_play_hosts: all_play_hosts.to_vec(),
            ..Scope::default()
        };
        let (raw, shared, untrusted, untrusted_hosts) = {
            let mut store = state.vars.lock().expect("vars lock");
            let shared = store.shared_values(&scope);
            let untrusted = store.untrusted_of(&host.name, &scope);
            let untrusted_hosts = store.untrusted_hosts();
            (
                store.for_host(&host.name, &scope),
                shared,
                untrusted,
                untrusted_hosts,
            )
        };
        let (resolved, untrusted) = state.templar.resolve_vars_tainted(Vars {
            map: &raw,
            hostvars: None,
            shared: Some(&shared),
            untrusted: Some(&untrusted),
            untrusted_hosts: Some(&untrusted_hosts),
        });
        let vars = Vars {
            map: &resolved,
            hostvars: None,
            shared: Some(&shared),
            untrusted: Some(&untrusted),
            untrusted_hosts: Some(&untrusted_hosts),
        };
        let mut files = Vec::new();
        for raw in &play.vars_files {
            let rendered = match state.templar.render(raw, vars) {
                Ok(rendered) => rendered,
                // Only a variable without a value is recoverable. Every other template failure
                // stops the run there, and swallowing them all as one undefined variable both
                // named the wrong cause and let a broken playbook exit 0.
                Err(err) if err.is_undefined() => {
                    if warned.insert(raw) {
                        out.warning(
                            "skipping vars_files item due to an undefined variable",
                            false,
                        );
                    }
                    continue;
                }
                Err(err) => anyhow::bail!("rendering vars_files entry {raw}: {err}"),
            };
            // Exit 4, the reference's own code for a playbook it cannot make sense of, measured
            // alongside the wording. A template that fails to render stays at 1, also measured:
            // the reference splits those two the same way.
            let path = rendered.as_str().ok_or_else(|| {
                crate::stats::Refusal::at(
                    4,
                    format!(
                        "Invalid `vars_files` value of type '{}'. A `vars_files` value should \
                         either be a string or list of strings.",
                        python_type(&rendered)
                    ),
                )
            })?;
            let path = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                playbook_dir.join(path)
            };
            // `exists` answers "no" both for a path that is not there and for one it cannot
            // stat, a parent directory refusing access included. The reference asks the same
            // question the same way and skips either without a word, so both stay a skip here.
            if !path.exists() {
                continue;
            }
            files.push(if let Some(file) = loaded.get(&path) {
                file.clone()
            } else {
                // Exit 4 as well: measured, a `vars_files` entry naming a file whose YAML
                // does not parse stops the reference with the same code a broken playbook
                // gets.
                let file =
                    load_vars_file(&path).map_err(|err| crate::stats::Refusal::or(4, err))?;
                loaded.insert(path, file.clone());
                file
            });
        }
        per_host.insert(host.name.clone(), files);
    }
    Ok(per_host)
}

/// The name Python would give a value, so a message about a mistyped playbook key reads the way
/// the reference's does.
fn python_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// Shapes more than one module's tests build.
#[cfg(test)]
mod testing {
    use super::*;
    use crate::playbook::PlayTask;
    use crate::vars::HostVars;

    pub(super) fn task(module: &str) -> PlayTask {
        PlayTask {
            name: "t".into(),
            module: module.into(),
            ..PlayTask::empty()
        }
    }

    /// A host's variables as the render path carries them: the map, and an empty shared view.
    pub(super) fn hvars(v: Value) -> HostVars {
        HostVars {
            map: vars(v),
            ..HostVars::default()
        }
    }

    pub(super) fn vars(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }
}
