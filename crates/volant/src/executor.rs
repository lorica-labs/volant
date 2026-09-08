// SPDX-License-Identifier: GPL-3.0-or-later
//! Runs one play on its hosts with the `linear` strategy. Each host renders its own tasks,
//! groups consecutive remote tasks into batches, and runs `set_fact` and `debug` locally.
//! Output is shown task by task, once every live host has reported that task.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, watch};
use volant_protocol::modules::short_name;
use volant_protocol::{BatchOutcome, FromAgent, Task, TaskResult, ToAgent};

use crate::agent::{AgentLink, AgentSource};
use crate::inventory::Host;
use crate::playbook::{Play, PlayTask};
use crate::render::{Renderer, ansible_json};
use crate::stats::{Outcome, Stats};
use crate::template::{Templar, TemplateError};
use crate::transport::{ConnectError, ConnectionDefaults, Transport};
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
    /// Flips to `true` once when the user interrupts the run.
    pub stop: watch::Receiver<bool>,
}

/// What outlives a play: the templar, the variable store hosts write into, and the hosts that
/// are out of the run.
pub struct RunState {
    pub templar: Arc<Templar>,
    pub vars: Arc<Mutex<VarStore>>,
    pub failed_hosts: HashSet<String>,
    pub verbosity: u8,
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
    Finished {
        host: String,
        failed: bool,
    },
    Unreachable {
        host: String,
        msg: String,
    },
}

/// Everything a host driver needs about the play, shared read-only.
struct PlayPlan {
    tasks: Vec<PlayTask>,
    play_vars: Map<String, Value>,
    /// The `vars_files` maps of each host, in the order the play lists the files.
    vars_files: HashMap<String, Vec<Map<String, Value>>>,
    play_hosts: Vec<String>,
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
        play_vars: play.vars.clone(),
        vars_files,
        play_hosts: play_hosts.clone(),
    });

    let (tx, mut rx) = mpsc::channel::<Event>(64);
    let mut workers = Vec::new();
    for host in &hosts {
        let tx = tx.clone();
        let (host, plan, agents, options) = (
            host.clone(),
            Arc::clone(&plan),
            agents.clone(),
            options.clone(),
        );
        let (templar, vars) = (Arc::clone(&state.templar), Arc::clone(&state.vars));
        let verbosity = state.verbosity;
        let name = host.name.clone();
        workers.push((
            name,
            tokio::spawn(async move {
                drive_host(host, plan, agents, options, templar, vars, verbosity, tx).await
            }),
        ));
    }
    drop(tx);

    let mut pending: HashMap<(String, usize), Vec<Event>> = HashMap::new();
    let mut done: HashSet<(String, usize)> = HashSet::new();
    let mut gone: HashSet<String> = HashSet::new();
    for (index, task) in plan.tasks.iter().enumerate() {
        let mut header_shown = false;
        for host in &play_hosts {
            loop {
                let key = (host.clone(), index);
                if done.contains(&key) {
                    if !header_shown {
                        out.task(&task_name(task, host, &plan, state));
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
                        let key = if let Event::Result { host, index, .. } = &event {
                            (host.clone(), *index)
                        } else {
                            unreachable!()
                        };
                        pending.entry(key).or_default().push(event);
                    }
                    Some(Event::TaskDone { host, index }) => {
                        done.insert((host, index));
                    }
                    Some(Event::Finished { host, failed }) => {
                        if failed {
                            state.failed_hosts.insert(host.clone());
                        }
                        gone.insert(host);
                    }
                    Some(Event::Unreachable { host, msg }) => {
                        if !header_shown {
                            out.task(&task_name(task, &host, &plan, state));
                            header_shown = true;
                        }
                        stats.unreachable(&host);
                        out.unreachable(&host, &msg);
                        state.failed_hosts.insert(host.clone());
                        gone.insert(host);
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
            }
            Event::Finished { host, failed: true } => {
                state.failed_hosts.insert(host);
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

fn task_name(task: &PlayTask, host: &str, plan: &PlayPlan, state: &RunState) -> String {
    if !Templar::is_template(&task.name) {
        return task.name.clone();
    }
    let vars = host_vars(host, plan, &task.vars, state.templar.as_ref(), &state.vars);
    state
        .templar
        .render(&task.name, &vars)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| task.name.clone())
}

/// The merged, self-resolved variables of a host for one task.
fn host_vars(
    host: &str,
    plan: &PlayPlan,
    task_vars: &Map<String, Value>,
    templar: &Templar,
    store: &Mutex<VarStore>,
) -> Map<String, Value> {
    let scope = Scope {
        play_vars: plan.play_vars.clone(),
        vars_files: plan.vars_files.get(host).cloned().unwrap_or_default(),
        task_vars: task_vars.clone(),
        play_hosts: plan.play_hosts.clone(),
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
    /// Send to the agent, one `Task` per item.
    Remote(Vec<Item>),
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
    templar: &Templar,
    store: &Mutex<VarStore>,
) -> Result<Prepared, TemplateError> {
    let base = host_vars(host, plan, &task.vars, templar, store);
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
    Ok(if is_local(&task.module) {
        Prepared::Local(items)
    } else {
        Prepared::Remote(items)
    })
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
    tx: mpsc::Sender<Event>,
) {
    let name = host.name.clone();
    let mut stop = options.stop.clone();
    // Set once the stop watch's sender is gone, so a dropped sender is never read as an
    // interrupt and the select below stops polling a branch that would otherwise resolve
    // immediately forever.
    let mut stop_broken = false;
    let mut link: Option<AgentLink> = None;
    let mut failed = false;
    let mut pos = 0;
    let n = plan.tasks.len();
    let mut batch_id: u64 = 0;

    'run: while pos < n && !failed && !*stop.borrow() {
        // Collect a batch of remote tasks up to the next boundary; report skips and run local
        // tasks as they come, in order.
        let mut batch: Vec<(usize, Vec<Item>)> = Vec::new();
        let mut deferred_error: Option<(usize, TemplateError)> = None;
        while pos < n {
            let task = &plan.tasks[pos];
            match prepare(task, &name, &plan, &templar, &store) {
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
                Ok(Prepared::Remote(items)) => {
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
            let link = match &mut link {
                Some(l) => l,
                None => match connect(&host, &agents, &options.defaults).await {
                    Ok(l) => link.insert(l),
                    Err(err) => {
                        let _ = tx
                            .send(Event::Unreachable {
                                host: name.clone(),
                                msg: err.to_string(),
                            })
                            .await;
                        return;
                    }
                },
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
                let _ = tx
                    .send(Event::Unreachable {
                        host: name.clone(),
                        msg: format!("sending batch: {err}"),
                    })
                    .await;
                return;
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
            match ended {
                Err(msg) => {
                    let _ = tx
                        .send(Event::Unreachable {
                            host: name.clone(),
                            msg,
                        })
                        .await;
                    return;
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
    if let Some(link) = link {
        link.shutdown().await;
    }
    let _ = tx.send(Event::Finished { host: name, failed }).await;
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

/// Every way this can fail is a host the run cannot reach, so it all comes back as one
/// `ConnectError` the driver renders as `UNREACHABLE`.
async fn connect(
    host: &Host,
    agents: &AgentSource,
    defaults: &ConnectionDefaults,
) -> Result<AgentLink, ConnectError> {
    let transport = Transport::for_host(host, defaults)
        .map_err(|e| ConnectError::Unreachable(format!("{e:#}")))?;
    let mut link = transport.connect(agents).await?;
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
        }
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

    #[test]
    fn with_items_flattens_one_level_only() {
        assert_eq!(
            flatten_once(vec![json!([1, [2]]), json!(3)]),
            vec![json!(1), json!([2]), json!(3)]
        );
    }
}
