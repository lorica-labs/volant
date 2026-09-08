// SPDX-License-Identifier: GPL-3.0-or-later
//! Runs one play on its hosts with the `linear` strategy. Each host renders its own tasks,
//! groups consecutive remote tasks into batches, and runs `set_fact` and `debug` locally.
//! Output is shown task by task, once every live host has reported that task.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use volant_protocol::modules::short_name;
use volant_protocol::{BatchOutcome, FromAgent, Task, TaskResult, ToAgent};

use crate::agent::{AgentLink, AgentSource};
use crate::inventory::Host;
use crate::playbook::{Play, PlayTask};
use crate::render::{Renderer, ansible_json};
use crate::stats::{Outcome, Stats};
use crate::template::{Templar, TemplateError};
use crate::transport::{ConnectError, ConnectionDefaults, Escalation, Transport};
use crate::vars::{Scope, VarStore, load_vars_file, omit_token};

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
    /// Flips to `true` once when the user interrupts the run.
    pub stop: watch::Receiver<bool>,
}

/// What the coordinator knows about the play's shared progress and every host driver may wait
/// on. Republished after every event that can change the live set, so a driver blocked on it is
/// never blocked on a host that has left the play.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    /// Highest task index every live host has finished, if any.
    pub completed_through: Option<usize>,
    /// Hosts still in the play, in inventory order.
    pub live_hosts: Vec<String>,
}

/// Names whose value depends on what the other hosts have done: another host's variables, and
/// the play's own live host list. A task whose raw text mentions one of them must not run ahead
/// of the others, so `linear` puts a boundary in front of it.
const CROSS_HOST_NAMES: [&str; 3] = ["hostvars", "play_hosts", "play_batch"];

/// Whether a task reads across hosts, decided once per play from its unrendered text.
fn reads_across_hosts(task: &PlayTask) -> bool {
    // `ansible_play_hosts_all` is a static copy of the play's starting host list: no host can
    // ever change it, so matching it buys no ordering guarantee and only costs a barrier. Strip
    // it before matching so it does not trip the `play_hosts` needle on its own; a task that
    // separately mentions `ansible_play_hosts` or `play_batch` still is a boundary.
    let mentions = |s: &str| {
        let s = s.replace("play_hosts_all", "");
        CROSS_HOST_NAMES.iter().any(|n| s.contains(n))
    };
    if mentions(&task.name) {
        return true;
    }
    if task
        .when
        .iter()
        .chain(&task.changed_when)
        .chain(&task.failed_when)
        .any(|s| mentions(s))
    {
        return true;
    }
    let mut text = serde_json::to_string(&task.args).unwrap_or_default();
    text.push_str(&serde_json::to_string(&task.vars).unwrap_or_default());
    if let Some(items) = &task.loop_items {
        text.push_str(&items.to_string());
    }
    mentions(&text)
}

/// Which agent a kept connection belongs to. The escalated user is part of the identity
/// because two connections to one host under two users are two different agents.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LinkKey {
    pub host: String,
    pub become_user: Option<String>,
}

/// What outlives a play: the templar, the variable store hosts write into, the hosts that
/// are out of the run, and the agents still connected.
pub struct RunState {
    pub templar: Arc<Templar>,
    pub vars: Arc<Mutex<VarStore>>,
    pub failed_hosts: HashSet<String>,
    pub verbosity: u8,
    /// Agents kept alive between plays, the way Ansible keeps its ssh connections open.
    pub links: HashMap<LinkKey, AgentLink>,
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

enum Event {
    /// One result line. `counts` says whether it enters the recap (loop items do not, their
    /// aggregate does); `show` whether a line is printed (a loop aggregate that succeeded is not).
    Result {
        host: String,
        index: usize,
        label: Option<String>,
        outcome: Outcome,
        result: TaskResult,
        dump: bool,
        show: bool,
        counts: bool,
    },
    /// Every result of task `index` on `host` has been sent.
    TaskDone {
        host: String,
        index: usize,
    },
    /// A host is done with this play and hands back the connections it wants kept open.
    Finished {
        host: String,
        failed: bool,
        links: Vec<(LinkKey, AgentLink)>,
    },
    Unreachable {
        host: String,
        msg: String,
    },
}

/// Everything a host driver needs about the play, shared read-only.
struct PlayPlan {
    tasks: Vec<PlayTask>,
    /// Parallel to `tasks`: whether the task is a `linear` boundary because it reads across
    /// hosts. Decided here rather than per host, since it depends only on the task's text.
    barriers: Vec<bool>,
    play_vars: Map<String, Value>,
    /// The `vars_files` maps of each host, in the order the play lists the files.
    vars_files: HashMap<String, Vec<Map<String, Value>>>,
    play_hosts: Vec<String>,
    r#become: Option<bool>,
    become_user: Option<String>,
}

pub async fn run_play(
    play: &Play,
    hosts: Vec<Host>,
    agents: &AgentSource,
    options: &RunOptions,
    state: &mut RunState,
    out: &mut Renderer,
    stats: &mut Stats,
) -> anyhow::Result<()> {
    out.play(&play.name);
    let hosts: Vec<Host> = hosts
        .into_iter()
        .filter(|h| !state.failed_hosts.contains(&h.name))
        .collect();
    if hosts.is_empty() {
        out.no_hosts();
        return Ok(());
    }
    if play.gather_facts {
        out.warning("gather_facts is not available in this release; continuing without facts");
    }
    let play_hosts: Vec<String> = hosts.iter().map(|h| h.name.clone()).collect();
    let playbook_dir = state
        .vars
        .lock()
        .expect("vars lock")
        .playbook_dir()
        .to_path_buf();
    let vars_files = load_play_vars_files(play, &hosts, &play_hosts, &playbook_dir, state)?;
    let plan = Arc::new(PlayPlan {
        tasks: play.tasks.iter().map(clone_task).collect(),
        barriers: play.tasks.iter().map(reads_across_hosts).collect(),
        play_vars: play.vars.clone(),
        vars_files,
        play_hosts: play_hosts.clone(),
        r#become: play.r#become,
        become_user: play.become_user.clone(),
    });

    let (tx, mut rx) = mpsc::channel::<Event>(64);
    // The play's shared progress. Every driver reads it; only this loop writes it.
    let (progress_tx, progress_rx) = watch::channel(Progress {
        completed_through: None,
        live_hosts: play_hosts.clone(),
    });
    // Ansible's `forks`, as permits. More permits than hosts would only raise the ceiling
    // above what this play can use, and `Semaphore` refuses a count near `usize::MAX`. Written
    // as `min` then `max` rather than `clamp(1, hosts.len())`: `clamp` panics whenever its
    // minimum exceeds its maximum, which an empty `hosts` would trigger here, and the only thing
    // preventing that today is the `is_empty` return above, invisible from this line.
    let forks = Arc::new(Semaphore::new(options.forks.min(hosts.len()).max(1)));
    let mut workers = Vec::new();
    for host in &hosts {
        let tx = tx.clone();
        let watchdog_tx = tx.clone();
        // Connections kept from an earlier play. Taken out of the map for the duration of the
        // play so the driver owns them, and handed back with `Finished`.
        let existing = take_links(&mut state.links, &host.name);
        let (host, plan, agents, options) = (
            host.clone(),
            Arc::clone(&plan),
            agents.clone(),
            options.clone(),
        );
        let (templar, vars) = (Arc::clone(&state.templar), Arc::clone(&state.vars));
        let verbosity = state.verbosity;
        let forks = Arc::clone(&forks);
        let progress = progress_rx.clone();
        let name = host.name.clone();
        let reported = name.clone();
        workers.push((
            name,
            tokio::spawn(async move {
                // A driver that panics would never report, so it would stay in the live set
                // for good and a host waiting at a barrier would wait for a host that is gone.
                // Reporting the panic from outside the driver keeps that promise: every host
                // in the live set either reports or leaves it.
                let driver = tokio::spawn(async move {
                    drive_host(
                        host, plan, agents, options, templar, vars, verbosity, existing, forks,
                        progress, tx,
                    )
                    .await
                });
                if let Err(err) = driver.await {
                    let _ = watchdog_tx
                        .send(Event::Unreachable {
                            host: reported.clone(),
                            msg: format!("driver panicked: {err}"),
                        })
                        .await;
                    let _ = watchdog_tx
                        .send(Event::Finished {
                            host: reported,
                            failed: true,
                            links: Vec::new(),
                        })
                        .await;
                }
            }),
        ));
    }
    drop(tx);

    let mut pending: HashMap<(String, usize), Vec<Event>> = HashMap::new();
    let mut done: HashSet<(String, usize)> = HashSet::new();
    let mut gone: HashSet<String> = HashSet::new();
    // Highest task index each host has reported. Reports arrive in task order over one channel,
    // so for a host still in the play this is also the index it has finished every task through.
    let mut last_done: HashMap<String, usize> = HashMap::new();
    for (index, task) in plan.tasks.iter().enumerate() {
        let mut header_shown = false;
        for host in &play_hosts {
            loop {
                let key = (host.clone(), index);
                if done.contains(&key) {
                    if !header_shown {
                        let live = progress_tx.borrow().live_hosts.clone();
                        out.task(&task_name(task, host, &plan, &live, state));
                        header_shown = true;
                    }
                    for event in pending.remove(&key).unwrap_or_default() {
                        if let Event::Result {
                            outcome,
                            result,
                            label,
                            dump,
                            show,
                            counts,
                            ..
                        } = event
                        {
                            if counts {
                                stats.record(host, outcome, result.changed());
                            }
                            if show {
                                out.result(host, outcome, &result, label.as_deref(), dump);
                            }
                        }
                    }
                    break;
                }
                if gone.contains(host) {
                    break;
                }
                match rx.recv().await {
                    Some(event @ Event::Result { .. }) => {
                        let (key, lost) = if let Event::Result {
                            host,
                            index,
                            outcome,
                            ..
                        } = &event
                        {
                            ((host.clone(), *index), *outcome == Outcome::Failed)
                        } else {
                            unreachable!()
                        };
                        // A failed result is the host leaving the play, and it arrives before
                        // that task's `TaskDone`. Taking it out of the live set here, rather
                        // than waiting for its `Finished`, is what lets the next task read the
                        // shrunken host list without racing the driver that is shutting down.
                        if lost {
                            state.failed_hosts.insert(key.0.clone());
                            publish(&progress_tx, &play_hosts, &state.failed_hosts, &last_done);
                        }
                        pending.entry(key).or_default().push(event);
                    }
                    Some(Event::TaskDone { host, index }) => {
                        let seen = last_done.entry(host.clone()).or_insert(index);
                        *seen = (*seen).max(index);
                        done.insert((host, index));
                        publish(&progress_tx, &play_hosts, &state.failed_hosts, &last_done);
                    }
                    Some(Event::Finished {
                        host,
                        failed,
                        links,
                    }) => {
                        if failed {
                            // The host is leaving the run for good: keeping its connection open
                            // would just idle until the run ends.
                            state.failed_hosts.insert(host.clone());
                            for (_, link) in links {
                                link.shutdown().await;
                            }
                        } else {
                            state.links.extend(links);
                        }
                        gone.insert(host);
                        publish(&progress_tx, &play_hosts, &state.failed_hosts, &last_done);
                    }
                    Some(Event::Unreachable { host, msg }) => {
                        if !header_shown {
                            let live = progress_tx.borrow().live_hosts.clone();
                            out.task(&task_name(task, &host, &plan, &live, state));
                            header_shown = true;
                        }
                        stats.unreachable(&host);
                        out.unreachable(&host, &msg);
                        state.failed_hosts.insert(host.clone());
                        gone.insert(host);
                        publish(&progress_tx, &play_hosts, &state.failed_hosts, &last_done);
                    }
                    None => break,
                }
            }
        }
        if gone.len() == play_hosts.len() && pending.is_empty() {
            break;
        }
    }
    // The channel is bounded, so a host still owing a send would block forever if reading
    // stopped here. Drain until every worker has dropped its sender.
    while let Some(event) = rx.recv().await {
        match event {
            Event::Unreachable { host, msg } => {
                stats.unreachable(&host);
                out.unreachable(&host, &msg);
                state.failed_hosts.insert(host);
                publish(&progress_tx, &play_hosts, &state.failed_hosts, &last_done);
            }
            Event::Finished {
                host,
                failed,
                links,
            } => {
                if failed {
                    state.failed_hosts.insert(host);
                    for (_, link) in links {
                        link.shutdown().await;
                    }
                } else {
                    state.links.extend(links);
                }
                publish(&progress_tx, &play_hosts, &state.failed_hosts, &last_done);
            }
            Event::TaskDone { host, index } => {
                let seen = last_done.entry(host).or_insert(index);
                *seen = (*seen).max(index);
                publish(&progress_tx, &play_hosts, &state.failed_hosts, &last_done);
            }
            _ => {}
        }
    }
    for (host, worker) in workers {
        if let Err(err) = worker.await
            && !gone.contains(&host)
        {
            stats.unreachable(&host);
            out.unreachable(&host, &format!("driver panicked: {err}"));
            state.failed_hosts.insert(host);
        }
    }
    Ok(())
}

/// Republishes the play's progress. `live_hosts` is the play's starting list minus the hosts
/// that have failed or gone unreachable, which is what `ansible_play_hosts` reports;
/// `completed_through` is the lowest task index reached by any of them, so a driver waiting for
/// it to reach `i - 1` is waiting only on hosts that are still expected to report.
fn publish(
    tx: &watch::Sender<Progress>,
    play_hosts: &[String],
    lost: &HashSet<String>,
    last_done: &HashMap<String, usize>,
) {
    let live_hosts: Vec<String> = play_hosts
        .iter()
        .filter(|h| !lost.contains(*h))
        .cloned()
        .collect();
    // `None` sorts below every `Some`, so a live host that has reported nothing yet holds the
    // minimum at `None` and no barrier opens on it.
    let completed_through = live_hosts
        .iter()
        .map(|h| last_done.get(h).copied())
        .min()
        .flatten();
    tx.send_replace(Progress {
        completed_through,
        live_hosts,
    });
}

/// Takes every connection belonging to one host out of the run's map.
fn take_links(links: &mut HashMap<LinkKey, AgentLink>, host: &str) -> Vec<(LinkKey, AgentLink)> {
    let keys: Vec<LinkKey> = links.keys().filter(|k| k.host == host).cloned().collect();
    keys.into_iter()
        .filter_map(|k| links.remove(&k).map(|link| (k, link)))
        .collect()
}

fn clone_task(t: &PlayTask) -> PlayTask {
    PlayTask {
        name: t.name.clone(),
        module: t.module.clone(),
        args: t.args.clone(),
        ignore_errors: t.ignore_errors,
        timeout: t.timeout,
        vars: t.vars.clone(),
        when: t.when.clone(),
        loop_items: t.loop_items.clone(),
        with_items: t.with_items,
        loop_var: t.loop_var.clone(),
        loop_label: t.loop_label.clone(),
        register: t.register.clone(),
        changed_when: t.changed_when.clone(),
        failed_when: t.failed_when.clone(),
        r#become: t.r#become,
        become_user: t.become_user.clone(),
        become_method: t.become_method.clone(),
    }
}

/// Resolves privilege escalation for one task: whether to escalate, to whom, and with what
/// password. `None` means the task runs as the connecting user.
///
/// The order is measured, not assumed. Against `ansible-core 2.19.12`: a host's
/// `ansible_become` variable beats both keywords in both directions (a play `become: false`
/// with `ansible_become=true` ran as root, and a task `become: true` with
/// `ansible_become=false` ran as the invoking user), and between the keywords the task beats
/// the play. `ansible_become_user` follows the same order. The connection defaults, which carry
/// `ansible.cfg`, its environment variables and the command line, speak last.
fn become_for(
    task: &PlayTask,
    play: &PlayPlan,
    vars: &Map<String, Value>,
    defaults: &ConnectionDefaults,
    templar: &Templar,
) -> Result<Option<Escalation>, TemplateError> {
    let on = vars
        .get("ansible_become")
        .and_then(as_bool_value)
        .or(task.r#become)
        .or(play.r#become)
        .unwrap_or(defaults.r#become);
    if !on {
        return Ok(None);
    }
    // The method is refused here, for the task that would actually use it, the way the
    // playbook's own keyword is refused at load time. A task that escalates nowhere is not
    // affected by a method it never runs.
    //
    // A variable beats the defaults, which carry `ansible.cfg`, its environment variables and
    // the command line. The startup pass already refuses what it can see; this catches the same
    // value reaching an escalating task through `group_vars`, `host_vars`, `--extra-vars` or a
    // `set_fact`, and the defaults for a task whose escalation the startup pass could not see.
    match vars.get("ansible_become_method").and_then(Value::as_str) {
        Some(method) if method != crate::playbook::BECOME_METHOD => {
            return Err(TemplateError(format!(
                "ansible_become_method '{method}' is not supported yet"
            )));
        }
        Some(_) => {}
        None if defaults.become_method != crate::playbook::BECOME_METHOD => {
            return Err(TemplateError(format!(
                "become_method '{}' is not supported yet",
                defaults.become_method
            )));
        }
        None => {}
    }
    let user = match vars.get("ansible_become_user").and_then(Value::as_str) {
        Some(user) => user.to_string(),
        None => task
            .become_user
            .clone()
            .or_else(|| play.become_user.clone())
            .unwrap_or_else(|| defaults.become_user.clone()),
    };
    // `become_user: "{{ app_user }}"` is ordinary Ansible, and the rendered name is what the
    // link is keyed by, so it has to be resolved before the connection is opened.
    let user = if Templar::is_template(&user) {
        templar
            .render(&user, vars)?
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| {
                TemplateError(format!("'become_user' must render to a user name: {user}"))
            })?
    } else {
        user
    };
    let password = vars
        .get("ansible_become_password")
        .or_else(|| vars.get("ansible_become_pass"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| defaults.become_password.clone());
    Ok(Some(Escalation { user, password }))
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
/// read once.
fn load_play_vars_files(
    play: &Play,
    hosts: &[Host],
    play_hosts: &[String],
    playbook_dir: &Path,
    state: &RunState,
) -> anyhow::Result<HashMap<String, Vec<Map<String, Value>>>> {
    let mut per_host = HashMap::new();
    if play.vars_files.is_empty() {
        return Ok(per_host);
    }
    let mut loaded: HashMap<PathBuf, Map<String, Value>> = HashMap::new();
    for host in hosts {
        let scope = Scope {
            play_vars: play.vars.clone(),
            vars_files: Vec::new(),
            task_vars: Map::new(),
            play_hosts: play_hosts.to_vec(),
            all_play_hosts: play_hosts.to_vec(),
        };
        let vars = state.templar.resolve_vars(
            &state
                .vars
                .lock()
                .expect("vars lock")
                .for_host(&host.name, &scope),
        );
        let mut files = Vec::new();
        for raw in &play.vars_files {
            let rendered = state
                .templar
                .render(raw, &vars)
                .map_err(|e| anyhow::anyhow!("vars_files '{raw}': {e}"))?;
            let path = rendered
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("vars_files entry must render to a path: {raw}"))?;
            let path = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                playbook_dir.join(path)
            };
            files.push(match loaded.get(&path) {
                Some(file) => file.clone(),
                None => {
                    let file = load_vars_file(&path)?;
                    loaded.insert(path, file.clone());
                    file
                }
            });
        }
        per_host.insert(host.name.clone(), files);
    }
    Ok(per_host)
}

fn task_name(
    task: &PlayTask,
    host: &str,
    plan: &PlayPlan,
    live: &[String],
    state: &RunState,
) -> String {
    if !Templar::is_template(&task.name) {
        return task.name.clone();
    }
    let vars = host_vars(
        host,
        plan,
        &task.vars,
        live,
        state.templar.as_ref(),
        &state.vars,
    );
    state
        .templar
        .render(&task.name, &vars)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| task.name.clone())
}

/// The merged, self-resolved variables of a host for one task. `live` is the play's host list as
/// the coordinator last published it, which is what `ansible_play_hosts` reports.
fn host_vars(
    host: &str,
    plan: &PlayPlan,
    task_vars: &Map<String, Value>,
    live: &[String],
    templar: &Templar,
    store: &Mutex<VarStore>,
) -> Map<String, Value> {
    let scope = Scope {
        play_vars: plan.play_vars.clone(),
        vars_files: plan.vars_files.get(host).cloned().unwrap_or_default(),
        task_vars: task_vars.clone(),
        play_hosts: live.to_vec(),
        all_play_hosts: plan.play_hosts.clone(),
    };
    let raw = store.lock().expect("vars lock").for_host(host, &scope);
    templar.resolve_vars(&raw)
}

/// A task rendered for one host: what to do with it.
enum Prepared {
    /// Every item (or the single non-loop item) had a false `when`: results are ready.
    Skipped(Vec<Item>),
    /// `set_fact` or `debug`: run on the controller.
    Local(Vec<Item>),
    /// Send to the agent, one `Task` per item, over a link running as this task's escalated
    /// user. Escalation belongs to the task rather than to an item: it decides which agent on
    /// the host the whole task talks to, so every item of a loop shares it.
    Remote(Vec<Item>, Option<Escalation>),
}

/// One loop item (or the whole task when there is no loop), rendered.
struct Item {
    /// The loop element, present only for loops.
    element: Option<Value>,
    label: Option<String>,
    args: Map<String, Value>,
    /// Variables in force for this item, for `changed_when`, `failed_when` and local modules.
    vars: Map<String, Value>,
    /// Set when `when` was false: the skip result to report.
    skipped: Option<TaskResult>,
}

fn prepare(
    task: &PlayTask,
    host: &str,
    plan: &PlayPlan,
    live: &[String],
    templar: &Templar,
    store: &Mutex<VarStore>,
    defaults: &ConnectionDefaults,
) -> Result<Prepared, TemplateError> {
    let base = host_vars(host, plan, &task.vars, live, templar, store);
    let elements: Vec<Option<Value>> = match &task.loop_items {
        None => vec![None],
        Some(raw) => {
            let rendered = templar.render_value(raw, &base)?;
            let list = match rendered {
                Value::Array(items) => items,
                other => {
                    return Err(TemplateError(format!(
                        "Invalid data passed to 'loop', it requires a list, got this instead: {other}"
                    )));
                }
            };
            // `with_items` flattens one level; `loop` does not.
            let list = if task.with_items {
                flatten_once(list)
            } else {
                list
            };
            list.into_iter().map(Some).collect()
        }
    };
    let mut items = Vec::new();
    for element in elements {
        let mut vars = base.clone();
        if let Some(el) = &element {
            vars.insert(task.loop_var.clone(), el.clone());
            vars.insert(
                "ansible_loop_var".into(),
                Value::String(task.loop_var.clone()),
            );
            // Variables naming the loop variable could not resolve before it was bound.
            vars = templar.resolve_vars(&vars);
        }
        let label = match (&element, &task.loop_label) {
            (None, _) => None,
            (Some(_), Some(template)) => Some(display(&templar.render(template, &vars)?)),
            (Some(el), None) => Some(display(el)),
        };
        let mut skipped = None;
        for condition in &task.when {
            if !templar.condition(condition, &vars)? {
                let mut r = Map::new();
                r.insert("changed".into(), json!(false));
                r.insert("skipped".into(), json!(true));
                r.insert("skip_reason".into(), json!("Conditional result was False"));
                r.insert("false_condition".into(), json!(condition));
                skipped = Some(TaskResult(r));
                break;
            }
        }
        let args = if skipped.is_some() {
            Map::new()
        } else {
            let rendered = templar.render_value(&Value::Object(task.args.clone()), &vars)?;
            let Value::Object(mut map) = rendered else {
                unreachable!("an object renders to an object")
            };
            map.retain(|_, v| v.as_str() != Some(omit_token()));
            map
        };
        items.push(Item {
            element,
            label,
            args,
            vars,
            skipped,
        });
    }
    if items.iter().all(|i| i.skipped.is_some()) {
        return Ok(Prepared::Skipped(items));
    }
    if is_local(&task.module) {
        return Ok(Prepared::Local(items));
    }
    let escalation = become_for(task, plan, &base, defaults, templar)?;
    Ok(Prepared::Remote(items, escalation))
}

fn flatten_once(list: Vec<Value>) -> Vec<Value> {
    list.into_iter()
        .flat_map(|v| match v {
            Value::Array(inner) => inner,
            other => vec![other],
        })
        .collect()
}

fn is_local(module: &str) -> bool {
    matches!(short_name(module), "set_fact" | "debug")
}

/// Ansible prints a string item as is and anything else as JSON.
fn display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => ansible_json(other),
    }
}

/// `set_fact` and `debug` never leave the controller.
fn run_local(
    task: &PlayTask,
    item: &Item,
    host: &str,
    templar: &Templar,
    store: &Mutex<VarStore>,
    verbosity: u8,
) -> TaskResult {
    let mut r = Map::new();
    match short_name(&task.module) {
        "set_fact" => {
            let mut facts = Map::new();
            for (k, v) in &item.args {
                if k == "cacheable" {
                    continue;
                }
                store
                    .lock()
                    .expect("vars lock")
                    .set_fact(host, k, v.clone());
                facts.insert(k.clone(), v.clone());
            }
            r.insert("ansible_facts".into(), Value::Object(facts));
            r.insert("changed".into(), json!(false));
        }
        "debug" => {
            let wanted: u8 = item
                .args
                .get("verbosity")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u8;
            if wanted > verbosity {
                r.insert("skipped".into(), json!(true));
                r.insert(
                    "skipped_reason".into(),
                    json!("Verbosity threshold not met."),
                );
                r.insert("changed".into(), json!(false));
                return TaskResult(r);
            }
            if let Some(var) = item.args.get("var").and_then(Value::as_str) {
                match templar.evaluate(var, &item.vars) {
                    Ok(v) => r.insert(var.to_string(), v),
                    Err(e) => r.insert(
                        var.to_string(),
                        Value::String(format!("VARIABLE IS NOT DEFINED!: {e}")),
                    ),
                };
            } else {
                r.insert(
                    "msg".into(),
                    item.args
                        .get("msg")
                        .cloned()
                        .unwrap_or_else(|| json!("Hello world!")),
                );
            }
            // `debug` only ever prints `msg` or the named `var`, never `changed`, matching what
            // ansible-playbook's callback actually displays for it.
        }
        other => {
            return TaskResult::failed_with(format!("{other} is not a controller-side module"));
        }
    }
    TaskResult(r)
}

/// One module result, ready to report: `changed_when` and `failed_when` decide its outcome
/// wherever the module ran, controller side as well as on the agent.
fn finish(task: &PlayTask, item: &Item, result: TaskResult, templar: &Templar) -> TaskResult {
    match apply_conditions(task, item, result, templar) {
        Ok(r) => r,
        Err(e) => TaskResult::failed_with(e.0),
    }
}

/// Applies `changed_when` and `failed_when` to one result, with `result` bound to it.
fn apply_conditions(
    task: &PlayTask,
    item: &Item,
    mut result: TaskResult,
    templar: &Templar,
) -> Result<TaskResult, TemplateError> {
    if task.changed_when.is_empty() && task.failed_when.is_empty() {
        return Ok(result);
    }
    let mut vars = item.vars.clone();
    if let Some(reg) = &task.register {
        vars.insert(reg.clone(), Value::Object(result.0.clone()));
    }
    vars.insert("result".into(), Value::Object(result.0.clone()));
    if !task.changed_when.is_empty() {
        let changed = all_hold(&task.changed_when, &vars, templar)?;
        result.0.insert("changed".into(), json!(changed));
    }
    if !task.failed_when.is_empty() {
        let failed = all_hold(&task.failed_when, &vars, templar)?;
        result.0.insert("failed_when_result".into(), json!(failed));
        if failed {
            result.0.insert("failed".into(), json!(true));
        } else {
            result.0.remove("failed");
            // A non-zero rc would still count as failed: the condition has spoken.
            if result.failed() {
                result.0.insert("failed".into(), json!(false));
            }
        }
    }
    Ok(result)
}

fn all_hold(
    conditions: &[String],
    vars: &Map<String, Value>,
    templar: &Templar,
) -> Result<bool, TemplateError> {
    for c in conditions {
        if !templar.condition(c, vars)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// What `register` stores: the single result, or Ansible's loop aggregate.
fn registered_value(task: &PlayTask, results: &[(Option<Value>, TaskResult)]) -> Value {
    if task.loop_items.is_none() {
        return results
            .first()
            .map(|(_, r)| Value::Object(r.0.clone()))
            .unwrap_or(Value::Null);
    }
    let mut list = Vec::new();
    for (element, r) in results {
        let mut m = r.0.clone();
        if let Some(el) = element {
            m.insert(task.loop_var.clone(), el.clone());
            m.insert("ansible_loop_var".into(), json!(task.loop_var));
        }
        list.push(Value::Object(m));
    }
    let mut agg = Map::new();
    agg.insert("results".into(), Value::Array(list));
    agg.insert(
        "changed".into(),
        json!(results.iter().any(|(_, r)| r.changed())),
    );
    if results.iter().any(|(_, r)| r.failed()) {
        agg.insert("failed".into(), json!(true));
    }
    agg.insert(
        "skipped".into(),
        json!(results.iter().all(|(_, r)| r.skipped())),
    );
    agg.insert("msg".into(), json!("All items completed"));
    Value::Object(agg)
}

fn classify(result: &TaskResult, ignore_errors: bool) -> Outcome {
    if result.failed() {
        if ignore_errors {
            Outcome::Ignored
        } else {
            Outcome::Failed
        }
    } else if result.skipped() {
        Outcome::Skipped
    } else if result.changed() {
        Outcome::Changed
    } else {
        Outcome::Ok
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive_host(
    host: Host,
    plan: Arc<PlayPlan>,
    agents: AgentSource,
    options: RunOptions,
    templar: Arc<Templar>,
    store: Arc<Mutex<VarStore>>,
    verbosity: u8,
    existing: Vec<(LinkKey, AgentLink)>,
    forks: Arc<Semaphore>,
    mut progress: watch::Receiver<Progress>,
    tx: mpsc::Sender<Event>,
) {
    let name = host.name.clone();
    let mut stop = options.stop.clone();
    // Set once the stop watch's sender is gone, so a dropped sender is never read as an
    // interrupt and the select below stops polling a branch that would otherwise resolve
    // immediately forever.
    let mut stop_broken = false;
    // One entry per target user on this host: the connecting user's own agent, and one more
    // for each `become_user` the play escalates to. Connections this play never uses itself
    // stay in here untouched and go straight back with `Finished`.
    let mut links: HashMap<LinkKey, AgentLink> = existing.into_iter().collect();
    // Which of them this play has already proved alive: one liveness check per connection per
    // play, and never again.
    let mut checked: HashSet<LinkKey> = HashSet::new();
    // Ansible's `forks`: one permit per host, held from the connection to the results of a
    // batch. It lives in this binding and releases itself when dropped, so no way out of this
    // function can leak it, whether that is a return, an error, a cancellation or a panic.
    let mut permit: Option<OwnedSemaphorePermit> = None;
    // Set when the host leaves the run without finishing: the message the recap shows.
    let mut unreachable: Option<String> = None;
    let mut failed = false;
    let mut pos = 0;
    let n = plan.tasks.len();
    let mut batch_id: u64 = 0;

    'run: while pos < n && !failed && !*stop.borrow() {
        // Collect a batch of remote tasks up to the next boundary; report skips and run local
        // tasks as they come, in order.
        let mut batch: Vec<(usize, Vec<Item>)> = Vec::new();
        // The escalation every task of the batch shares. A batch is one message to one agent,
        // so it cannot span two target users.
        let mut batch_escalation: Option<Escalation> = None;
        let mut deferred_error: Option<(usize, TemplateError)> = None;
        while pos < n {
            let task = &plan.tasks[pos];
            // A task that reads across hosts is a boundary before itself: the batch in hand
            // goes out first, and then this host waits for the others to reach the previous
            // task, the way `linear` does.
            if plan.barriers[pos] && pos > 0 {
                if !batch.is_empty() {
                    break;
                }
                loop {
                    if *stop.borrow() {
                        break 'run;
                    }
                    let p = progress.borrow().clone();
                    // Alone in the play there is nobody left to wait for, which is also how a
                    // wait ends when every other host has died.
                    if p.live_hosts.len() <= 1 || p.completed_through.is_some_and(|c| c + 1 >= pos)
                    {
                        break;
                    }
                    tokio::select! {
                        changed = progress.changed() => {
                            // The coordinator is gone, so no further progress can be published
                            // and waiting on it would never end.
                            if changed.is_err() {
                                break;
                            }
                        }
                        res = stop.changed(), if !stop_broken => {
                            if res.is_err() {
                                stop_broken = true;
                            } else {
                                break 'run;
                            }
                        }
                    }
                }
            }
            let live = progress.borrow().live_hosts.clone();
            match prepare(
                task,
                &name,
                &plan,
                &live,
                &templar,
                &store,
                &options.defaults,
            ) {
                Err(err) => {
                    deferred_error = Some((pos, err));
                    break;
                }
                Ok(Prepared::Skipped(items)) => {
                    if !batch.is_empty() {
                        break;
                    }
                    let labels: Vec<Option<String>> =
                        items.iter().map(|i| i.label.clone()).collect();
                    let results: Vec<(Option<Value>, TaskResult)> = items
                        .into_iter()
                        .map(|i| (i.element, i.skipped.unwrap_or_default()))
                        .collect();
                    report_task(&tx, &name, pos, task, &results, &labels, false).await;
                    if let Some(reg) = &task.register {
                        store.lock().expect("vars lock").set_fact(
                            &name,
                            reg,
                            registered_value(task, &results),
                        );
                    }
                    pos += 1;
                }
                Ok(Prepared::Local(items)) => {
                    if !batch.is_empty() {
                        break;
                    }
                    let mut results = Vec::new();
                    let mut labels = Vec::new();
                    for item in &items {
                        let r = match &item.skipped {
                            Some(s) => s.clone(),
                            None => finish(
                                task,
                                item,
                                run_local(task, item, &name, &templar, &store, verbosity),
                                &templar,
                            ),
                        };
                        results.push((item.element.clone(), r));
                        labels.push(item.label.clone());
                    }
                    if let Some(reg) = &task.register {
                        store.lock().expect("vars lock").set_fact(
                            &name,
                            reg,
                            registered_value(task, &results),
                        );
                    }
                    failed = report_task(
                        &tx,
                        &name,
                        pos,
                        task,
                        &results,
                        &labels,
                        short_name(&task.module) == "debug",
                    )
                    .await;
                    pos += 1;
                    if failed {
                        break 'run;
                    }
                }
                Ok(Prepared::Remote(items, escalation)) => {
                    // A different target user is a different agent on the host, so the batch
                    // ends here and the next one opens its own link. `pos` does not move, so
                    // this task is the first of that batch.
                    if !batch.is_empty() && escalation != batch_escalation {
                        break;
                    }
                    batch_escalation = escalation;
                    let boundary = task.register.is_some()
                        || !task.changed_when.is_empty()
                        || !task.failed_when.is_empty();
                    batch.push((pos, items));
                    pos += 1;
                    if boundary {
                        break;
                    }
                }
            }
        }

        if !batch.is_empty() {
            if permit.is_none() {
                match Arc::clone(&forks).acquire_owned().await {
                    Ok(p) => permit = Some(p),
                    // Nothing in this run closes the semaphore, so this is a bug rather than
                    // a shutdown. Reporting it beats returning as if the host had run.
                    Err(_) => {
                        unreachable = Some("the run's fork limit is gone".to_string());
                        break 'run;
                    }
                }
            }
            let key = LinkKey {
                host: name.clone(),
                become_user: batch_escalation.as_ref().map(|e| e.user.clone()),
            };
            let link = match reuse_or_connect(
                &mut links,
                &mut checked,
                &key,
                batch_escalation.as_ref(),
                &host,
                &agents,
                &options,
            )
            .await
            {
                Ok(l) => l,
                // The host answered and then refused to escalate, so this is the task failing
                // and not the host going away. `ignore_errors` is deliberately not honoured:
                // the batch never ran, and a run that reported success while having quietly
                // skipped every escalated task is the worst outcome available here.
                Err(ConnectError::Become(msg)) => {
                    let index = batch[0].0;
                    let mut task = clone_task(&plan.tasks[index]);
                    task.ignore_errors = false;
                    let results = vec![(None, TaskResult::failed_with(msg))];
                    report_task(&tx, &name, index, &task, &results, &[None], false).await;
                    failed = true;
                    break 'run;
                }
                Err(err) => {
                    unreachable = Some(err.to_string());
                    break 'run;
                }
            };
            batch_id += 1;
            // Flat list for the agent, with a map back to (task, item).
            let mut tasks = Vec::new();
            let mut origin = Vec::new();
            for (bi, (index, items)) in batch.iter().enumerate() {
                let task = &plan.tasks[*index];
                for (ii, item) in items.iter().enumerate() {
                    if item.skipped.is_some() {
                        continue;
                    }
                    tasks.push(Task {
                        module: task.module.clone(),
                        args: item.args.clone(),
                        ignore_errors: task.ignore_errors,
                        timeout: task.timeout,
                    });
                    origin.push((bi, ii));
                }
            }
            if let Err(err) = link
                .send(&ToAgent::RunBatch {
                    id: batch_id,
                    tasks,
                })
                .await
            {
                unreachable = Some(format!("sending batch: {err}"));
                break 'run;
            }
            let mut received: Vec<Vec<Option<TaskResult>>> = batch
                .iter()
                .map(|(_, items)| vec![None; items.len()])
                .collect();
            let ended = loop {
                let msg = tokio::select! {
                    msg = link.recv() => msg,
                    res = stop.changed(), if !stop_broken => {
                        if res.is_err() {
                            stop_broken = true;
                            continue;
                        }
                        link.cancel(batch_id, CANCEL_GRACE).await;
                        break Ok(BatchOutcome::Cancelled { at: 0 });
                    }
                };
                match msg {
                    Ok(Some(FromAgent::TaskResult { index, result, .. })) => {
                        if let Some(&(bi, ii)) = origin.get(index) {
                            received[bi][ii] = Some(result);
                        }
                    }
                    Ok(Some(FromAgent::BatchDone { outcome, .. })) => break Ok(outcome),
                    Ok(Some(FromAgent::Log { message, .. })) => eprintln!("[{name}] {message}"),
                    Ok(Some(FromAgent::Ready { .. })) => {}
                    Ok(None) => break Err("agent stopped before the batch finished".to_string()),
                    Err(err) => break Err(format!("reading from the agent: {err}")),
                }
            };
            // Report every task of the batch in order; tasks the agent never reached (after a
            // failure) are not reported at all, as in Ansible.
            for (bi, (index, items)) in batch.iter().enumerate() {
                let task = &plan.tasks[*index];
                let mut results = Vec::new();
                let mut labels = Vec::new();
                let mut reached = true;
                for (ii, item) in items.iter().enumerate() {
                    let r = match (&item.skipped, received[bi][ii].take()) {
                        (Some(s), _) => s.clone(),
                        (None, Some(r)) => finish(task, item, r, &templar),
                        (None, None) => {
                            reached = false;
                            break;
                        }
                    };
                    results.push((item.element.clone(), r));
                    labels.push(item.label.clone());
                }
                if !reached && results.is_empty() {
                    break;
                }
                if let Some(reg) = &task.register {
                    store.lock().expect("vars lock").set_fact(
                        &name,
                        reg,
                        registered_value(task, &results),
                    );
                }
                if report_task(&tx, &name, *index, task, &results, &labels, false).await {
                    failed = true;
                    break;
                }
            }
            // The results are in and reported, so the next host may start while this one
            // renders its remaining local tasks.
            permit = None;
            match ended {
                Err(msg) => {
                    unreachable = Some(msg);
                    break 'run;
                }
                Ok(BatchOutcome::Cancelled { .. }) => break 'run,
                Ok(_) => {}
            }
        }

        if !failed && let Some((index, err)) = deferred_error {
            let task = &plan.tasks[index];
            let results = vec![(None, TaskResult::failed_with(err.0))];
            failed = report_task(&tx, &name, index, task, &results, &[None], false).await;
            pos = index + 1;
            if failed {
                break 'run;
            }
        }
    }
    drop(permit);
    if let Some(msg) = unreachable {
        // Whatever went wrong, none of these connections is one to hand to the next play.
        for (_, link) in links.drain() {
            link.shutdown().await;
        }
        failed = true;
        let _ = tx
            .send(Event::Unreachable {
                host: name.clone(),
                msg,
            })
            .await;
    }
    // Healthy connections outlive the play: the run closes them once, before the recap.
    let _ = tx
        .send(Event::Finished {
            host: name,
            failed,
            links: links.into_iter().collect(),
        })
        .await;
}

/// The connection for the next batch, under `key`'s target user: the one kept from an earlier
/// play if its agent still answers, a fresh one otherwise. A kept connection gets exactly one
/// liveness check per play, and a failed check exactly one reconnection; a failed reconnection
/// is the host's `UNREACHABLE`.
#[allow(clippy::too_many_arguments)]
async fn reuse_or_connect<'a>(
    links: &'a mut HashMap<LinkKey, AgentLink>,
    checked: &mut HashSet<LinkKey>,
    key: &LinkKey,
    escalation: Option<&Escalation>,
    host: &Host,
    agents: &AgentSource,
    options: &RunOptions,
) -> Result<&'a mut AgentLink, ConnectError> {
    // Why the kept connection was not reused, kept only in case the reconnection below fails
    // too: on its own it is not a failure (a single reconnection is the designed recovery), but
    // discarding it silently would leave a reconnect failure reporting only its own cause.
    let mut stale: Option<String> = None;
    if checked.insert(key.clone())
        && let Some(mut link) = links.remove(key)
    {
        let alive = tokio::time::timeout(options.defaults.connect_timeout, link.handshake()).await;
        match alive {
            Ok(Ok(())) => {
                links.insert(key.clone(), link);
            }
            Ok(Err(err)) => {
                stale = Some(format!("{err:#}"));
                link.shutdown().await;
            }
            Err(_) => {
                stale = Some("no answer from the kept connection".to_string());
                link.shutdown().await;
            }
        }
    }
    if !links.contains_key(key) {
        let link = match connect(host, agents, &options.defaults, escalation).await {
            Ok(link) => link,
            Err(ConnectError::Unreachable(msg)) => {
                return Err(ConnectError::Unreachable(match stale {
                    Some(reason) => format!(
                        "the kept connection failed ({reason}), and reconnecting failed too: {msg}"
                    ),
                    None => msg,
                }));
            }
            // A refused escalation is the host's answer about this user, not a connection that
            // failed, so it travels on untouched: adding the stale note would turn a failed
            // task into something that reads like an unreachable host.
            Err(err) => return Err(err),
        };
        links.insert(key.clone(), link);
    }
    Ok(links.get_mut(key).expect("connected just above"))
}

/// Sends the result lines of one task and its `TaskDone`. Returns whether the host failed.
/// `labels` is parallel to `results`: each item's display label, honouring a custom
/// `loop_control.label` template where the playbook gave one.
async fn report_task(
    tx: &mpsc::Sender<Event>,
    host: &str,
    index: usize,
    task: &PlayTask,
    results: &[(Option<Value>, TaskResult)],
    labels: &[Option<String>],
    dump: bool,
) -> bool {
    let is_loop = task.loop_items.is_some();
    let mut any_failed = false;
    for (i, (_element, r)) in results.iter().enumerate() {
        let outcome = classify(r, task.ignore_errors);
        any_failed |= r.failed();
        let label = labels.get(i).cloned().flatten();
        let _ = tx
            .send(Event::Result {
                host: host.to_string(),
                index,
                label,
                outcome,
                result: r.clone(),
                dump,
                show: true,
                counts: !is_loop,
            })
            .await;
    }
    if is_loop {
        let mut aggregate = match registered_value(task, results) {
            Value::Object(mut m) => {
                m.remove("results");
                TaskResult(m)
            }
            _ => TaskResult::default(),
        };
        let outcome = classify(&aggregate, task.ignore_errors);
        let show = aggregate.failed();
        if show {
            aggregate
                .0
                .insert("msg".into(), json!("One or more items failed"));
        }
        let _ = tx
            .send(Event::Result {
                host: host.to_string(),
                index,
                label: None,
                outcome,
                result: aggregate,
                dump: false,
                show,
                counts: true,
            })
            .await;
    }
    let _ = tx
        .send(Event::TaskDone {
            host: host.to_string(),
            index,
        })
        .await;
    any_failed && !task.ignore_errors
}

/// Opens one link, escalated when `escalation` is given. Every way this can fail is a host the
/// run cannot reach and comes back as `ConnectError::Unreachable`, except a `sudo` that refused,
/// which comes back as `ConnectError::Become` because the host itself answered.
async fn connect(
    host: &Host,
    agents: &AgentSource,
    defaults: &ConnectionDefaults,
    escalation: Option<&Escalation>,
) -> Result<AgentLink, ConnectError> {
    let transport = Transport::for_host(host, defaults)
        .map_err(|e| ConnectError::Unreachable(format!("{e:#}")))?;
    let mut link = transport.connect(agents, escalation).await?;
    let timeout = defaults.connect_timeout;
    tokio::time::timeout(timeout, link.handshake())
        .await
        .map_err(|_| {
            ConnectError::Unreachable(format!(
                "no answer from the agent after {} seconds",
                timeout.as_secs()
            ))
        })?
        .map_err(|e| ConnectError::Unreachable(format!("{e:#}")))?;
    Ok(link)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(module: &str) -> PlayTask {
        PlayTask {
            name: "t".into(),
            module: module.into(),
            args: Map::new(),
            ignore_errors: false,
            timeout: None,
            vars: Map::new(),
            when: Vec::new(),
            loop_items: None,
            with_items: false,
            loop_var: "item".into(),
            loop_label: None,
            register: None,
            changed_when: Vec::new(),
            failed_when: Vec::new(),
            r#become: None,
            become_user: None,
            become_method: None,
        }
    }

    fn plan() -> PlayPlan {
        PlayPlan {
            tasks: Vec::new(),
            barriers: Vec::new(),
            play_vars: Map::new(),
            vars_files: HashMap::new(),
            play_hosts: Vec::new(),
            r#become: None,
            become_user: None,
        }
    }

    fn defaults() -> ConnectionDefaults {
        ConnectionDefaults {
            remote_user: None,
            private_key: None,
            host_key_checking: true,
            remote_tmp: "~/.ansible/tmp".into(),
            connect_timeout: Duration::from_secs(10),
            r#become: false,
            become_user: "root".into(),
            become_method: "sudo".into(),
            become_password: None,
        }
    }

    fn vars(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    /// The order measured against `ansible-core 2.19.12`: `ansible_become` wins over both
    /// keywords, in both directions, and the task keyword wins over the play's.
    #[test]
    fn a_host_variable_beats_both_become_keywords() {
        let templar = Templar::new(std::env::temp_dir());
        let escalate = |task_become, play_become, host: Value| {
            let mut t = task("command");
            t.r#become = task_become;
            let mut p = plan();
            p.r#become = play_become;
            become_for(&t, &p, &vars(host), &defaults(), &templar).unwrap()
        };
        assert!(
            escalate(None, Some(true), json!({"ansible_become": false})).is_none(),
            "the variable turns a play's become off"
        );
        assert!(
            escalate(None, Some(false), json!({"ansible_become": true})).is_some(),
            "and turns it on where the play said no"
        );
        assert!(
            escalate(Some(true), None, json!({"ansible_become": false})).is_none(),
            "a task keyword loses to the variable too"
        );
        assert!(
            escalate(Some(false), Some(true), json!({})).is_none(),
            "with no variable, the task keyword beats the play's"
        );
        assert!(
            escalate(None, Some(true), json!({})).is_some(),
            "and the play keyword stands on its own"
        );
        assert!(
            escalate(None, None, json!({"ansible_become": "yes"})).is_some(),
            "a variable spelled the way Ansible content spells it is still a boolean"
        );
    }

    #[test]
    fn the_target_user_follows_the_same_order_and_defaults_to_root() {
        let templar = Templar::new(std::env::temp_dir());
        let mut t = task("command");
        t.r#become = Some(true);
        let mut p = plan();
        p.become_user = Some("play".into());
        assert_eq!(
            become_for(&t, &p, &Map::new(), &defaults(), &templar)
                .unwrap()
                .unwrap()
                .user,
            "play"
        );
        t.become_user = Some("task".into());
        assert_eq!(
            become_for(&t, &p, &Map::new(), &defaults(), &templar)
                .unwrap()
                .unwrap()
                .user,
            "task"
        );
        let host = vars(json!({"ansible_become_user": "host"}));
        assert_eq!(
            become_for(&t, &p, &host, &defaults(), &templar)
                .unwrap()
                .unwrap()
                .user,
            "host",
            "the variable wins here as well"
        );
        let bare = task("command");
        let mut d = defaults();
        d.r#become = true;
        assert_eq!(
            become_for(&bare, &plan(), &Map::new(), &d, &templar)
                .unwrap()
                .unwrap()
                .user,
            "root",
            "nothing said anywhere means root, as in Ansible"
        );
        let templated = vars(json!({"ansible_become_user": "{{ who }}", "who": "deploy"}));
        assert_eq!(
            become_for(&t, &p, &templated, &defaults(), &templar)
                .unwrap()
                .unwrap()
                .user,
            "deploy",
            "a templated user is resolved before the link is keyed by it"
        );
    }

    /// A method arriving as a variable is refused by its name rather than quietly escalating
    /// with `sudo`: running the task under rules nobody wrote is worse than not running it.
    #[test]
    fn an_unsupported_become_method_variable_fails_the_task() {
        let templar = Templar::new(std::env::temp_dir());
        let mut t = task("command");
        t.r#become = Some(true);
        let err = become_for(
            &t,
            &plan(),
            &vars(json!({"ansible_become_method": "su"})),
            &defaults(),
            &templar,
        )
        .unwrap_err();
        assert!(
            err.0.contains("su") && err.0.contains("not supported"),
            "{}",
            err.0
        );
        assert_eq!(
            become_for(
                &task("command"),
                &plan(),
                &vars(json!({"ansible_become_method": "su"})),
                &defaults(),
                &templar,
            )
            .unwrap(),
            None,
            "a task that escalates nowhere is not refused for a method it never runs"
        );
        let d = ConnectionDefaults {
            become_method: "doas".into(),
            ..defaults()
        };
        let err = become_for(&t, &plan(), &Map::new(), &d, &templar).unwrap_err();
        assert!(
            err.0.contains("doas") && err.0.contains("not supported"),
            "the defaults are refused for an escalating task the startup pass could not see: {}",
            err.0
        );
        assert_eq!(
            become_for(&task("command"), &plan(), &Map::new(), &d, &templar).unwrap(),
            None,
            "and the same defaults leave a task that never escalates alone"
        );
    }

    /// The password never has to be quoted, logged or formatted, so the one place it could still
    /// leak is a `Debug` that a `-vvv` diagnostic reaches.
    #[test]
    fn the_become_password_is_redacted_in_debug_output() {
        let escalation = Escalation {
            user: "root".into(),
            password: Some("s3cret".into()),
        };
        assert!(!format!("{escalation:?}").contains("s3cret"));
        let mut d = defaults();
        d.become_password = Some("s3cret".into());
        assert!(!format!("{d:?}").contains("s3cret"));
    }

    fn result(v: Value) -> TaskResult {
        TaskResult(v.as_object().unwrap().clone())
    }

    #[test]
    fn a_loop_aggregate_follows_ansible_shape() {
        let mut t = task("command");
        t.loop_items = Some(json!([1, 2]));
        let results = vec![
            (Some(json!(1)), result(json!({"changed": true, "rc": 0}))),
            (
                Some(json!(2)),
                result(json!({"changed": false, "rc": 1, "failed": true})),
            ),
        ];
        let agg = registered_value(&t, &results);
        assert_eq!(agg["changed"], json!(true));
        assert_eq!(agg["failed"], json!(true));
        assert_eq!(agg["skipped"], json!(false));
        assert_eq!(agg["msg"], json!("All items completed"));
        assert_eq!(agg["results"][1]["item"], json!(2));
        assert_eq!(agg["results"][1]["ansible_loop_var"], json!("item"));
    }

    #[test]
    fn failed_when_false_rescues_a_non_zero_rc() {
        let templar = Templar::new(std::env::temp_dir());
        let mut t = task("command");
        t.failed_when = vec!["result.rc == 99".into()];
        let item = Item {
            element: None,
            label: None,
            args: Map::new(),
            vars: Map::new(),
            skipped: None,
        };
        let r = apply_conditions(
            &t,
            &item,
            result(json!({"rc": 1, "changed": true})),
            &templar,
        )
        .unwrap();
        assert!(!r.failed(), "{:?}", r.0);
        assert_eq!(r.0["failed_when_result"], json!(false));
        let r = apply_conditions(
            &t,
            &item,
            result(json!({"rc": 99, "changed": true})),
            &templar,
        )
        .unwrap();
        assert!(r.failed());
    }

    #[test]
    fn labels_show_strings_bare_and_the_rest_as_json() {
        assert_eq!(display(&json!("one")), "one");
        assert_eq!(display(&json!({"name": "one"})), r#"{"name": "one"}"#);
        assert_eq!(display(&json!(3)), "3");
    }

    /// Everything a playbook can hide a cross-host read in has to be searched, or a barrier
    /// silently does nothing and the ordering it promised was never there.
    #[test]
    fn a_cross_host_read_is_found_wherever_the_task_spells_it() {
        let mut t = task("command");
        assert!(!reads_across_hosts(&t));
        t.args
            .insert("cmd".into(), json!("echo {{ hostvars['a'].x }}"));
        assert!(reads_across_hosts(&t), "arguments");
        let mut t = task("debug");
        t.when = vec!["inventory_hostname in ansible_play_hosts".into()];
        assert!(reads_across_hosts(&t), "when");
        let mut t = task("debug");
        t.name = "count {{ ansible_play_batch | length }}".into();
        assert!(reads_across_hosts(&t), "name");
        let mut t = task("debug");
        t.vars.insert("peer".into(), json!("{{ hostvars['a'].y }}"));
        assert!(reads_across_hosts(&t), "vars");
        let mut t = task("command");
        t.loop_items = Some(json!("{{ ansible_play_hosts }}"));
        assert!(reads_across_hosts(&t), "loop");
        let mut t = task("command");
        t.changed_when = vec!["hostvars['a'].rc == 0".into()];
        assert!(reads_across_hosts(&t), "changed_when");
        let mut t = task("command");
        t.failed_when = vec!["play_hosts | length > 1".into()];
        assert!(reads_across_hosts(&t), "failed_when");
    }

    /// `ansible_play_hosts_all` never changes once the play starts, so a task mentioning it and
    /// nothing else needs no barrier. The live list and the batch keyword still hold one.
    #[test]
    fn ansible_play_hosts_all_alone_is_not_a_boundary() {
        let mut t = task("debug");
        t.args.insert(
            "msg".into(),
            json!("{{ ansible_play_hosts_all | join(',') }}"),
        );
        assert!(!reads_across_hosts(&t), "the static list on its own");
        let mut t = task("debug");
        t.args.insert(
            "msg".into(),
            json!(
                "{{ ansible_play_hosts | join(',') }} of {{ ansible_play_hosts_all | join(',') }}"
            ),
        );
        assert!(reads_across_hosts(&t), "the live list is still present");
        let mut t = task("debug");
        t.args
            .insert("msg".into(), json!("{{ ansible_play_batch }}"));
        assert!(reads_across_hosts(&t), "play_batch is unaffected");
    }

    /// The barrier opens on the slowest live host, and a host that has left the play stops
    /// holding it: this is what keeps a wait from outliving the host it waits for.
    #[test]
    fn progress_follows_the_slowest_live_host_and_forgets_the_others() {
        let hosts: Vec<String> = vec!["alpha".into(), "beta".into()];
        let (tx, rx) = watch::channel(Progress::default());
        let mut last_done = HashMap::new();
        let mut lost = HashSet::new();

        publish(&tx, &hosts, &lost, &last_done);
        assert_eq!(rx.borrow().completed_through, None, "nobody has reported");
        assert_eq!(rx.borrow().live_hosts, hosts);

        last_done.insert("beta".to_string(), 3);
        publish(&tx, &hosts, &lost, &last_done);
        assert_eq!(
            rx.borrow().completed_through,
            None,
            "alpha has reported nothing, so the barrier stays shut"
        );

        last_done.insert("alpha".to_string(), 1);
        publish(&tx, &hosts, &lost, &last_done);
        assert_eq!(rx.borrow().completed_through, Some(1));

        lost.insert("alpha".to_string());
        publish(&tx, &hosts, &lost, &last_done);
        assert_eq!(
            rx.borrow().completed_through,
            Some(3),
            "a host out of the play no longer holds the barrier"
        );
        assert_eq!(rx.borrow().live_hosts, vec!["beta".to_string()]);

        lost.insert("beta".to_string());
        publish(&tx, &hosts, &lost, &last_done);
        assert!(rx.borrow().live_hosts.is_empty());
        assert_eq!(rx.borrow().completed_through, None);
    }

    #[test]
    fn with_items_flattens_one_level_only() {
        assert_eq!(
            flatten_once(vec![json!([1, [2]]), json!(3)]),
            vec![json!(1), json!([2]), json!(3)]
        );
    }
}
