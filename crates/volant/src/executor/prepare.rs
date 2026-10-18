// SPDX-License-Identifier: GPL-3.0-or-later
//! Rendering one task for one host: variables, escalation, environment and loop items.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};
use tokio::sync::{Semaphore, watch};
use volant_protocol::TaskResult;

use crate::action_plugins::Kind;
use crate::compile::{Compiled, IncludeParams, Origin, Step};
use crate::playbook::{Flag, PlayTask, python_repr};
use crate::python::ModulePayload;
use crate::render::ansible_json;
use crate::template::{Templar, TemplateError};
use crate::transport::{BecomeMethod, ConnectionDefaults, ConnectionKeywords, Escalation};
use crate::vars::{HostVars, Scope, VarStore, host_setting, omit_token};

use super::as_bool_value;
use super::coordinator::Progress;

/// The head of a task whose arguments a plugin failed to render, then the action in quotes:
/// `Finalization of task args for 'ansible.builtin.debug' failed: Error while resolving value
/// for 'msg': The lookup plugin 'x' failed: …`, measured on ansible-core 2.19.12.
pub(super) const FINALIZING: &str = "Finalization of task args for ";

/// Everything a host driver needs about the play, shared read-only.
pub(super) struct PlayPlan {
    /// The play flattened into numbered steps, with the block spans that say where each one
    /// sits. Every host walks the same list, which is what lets the coordinator name a step to
    /// all of them at once.
    ///
    /// It is a watch rather than a value because of the one thing in a play that is not known
    /// when it is compiled: at a flush point the coordinator inserts the handler steps behind
    /// the flush and publishes the new list here. Every index a driver holds is still the index
    /// it was holding, because no driver reads the list again between reporting a flush point
    /// and the splice behind it - see [`Compiled::splice`] and `wait_for_splice`.
    pub(super) plan: watch::Receiver<Arc<Compiled>>,
    /// Whether a host that failed still runs the handlers it notified: the play's own keyword,
    /// or the run's `--force-handlers` when the play says nothing.
    pub(super) force_handlers: bool,
    /// Whether the hosts meet in front of every step: a play with `max_fail_percentage` decides
    /// after each task whether the batch goes on, and a host that ran ahead would run a task the
    /// reference never starts.
    pub(super) lockstep: bool,
    /// How the hosts of the play move through it relative to one another.
    pub(super) strategy: Strategy,
    /// One gate per step carrying a `throttle`, made by the first host to reach it. A step's
    /// index names it for as long as the play runs: a splice only moves the steps behind a point
    /// no host has passed, so no gate is made for an index before the list has settled there.
    pub(super) throttles: Mutex<HashMap<usize, Arc<Semaphore>>>,
    /// `host_pinned`: the hosts of the batch in the order they take a fork, and how many of them
    /// have had their turn. A host queues for the fork only when the one before it has, so the
    /// forks go out in the batch's order and not in the order the scheduler starts the drivers.
    pub(super) queue: Vec<String>,
    pub(super) turn: watch::Sender<usize>,
    pub(super) play_vars: Map<String, Value>,
    /// The `vars_files` maps of each host, in the order the play lists the files.
    pub(super) vars_files: HashMap<String, Vec<Map<String, Value>>>,
    /// Every host the play resolved, whatever batch it belongs to and whether it has failed or
    /// not: `ansible_play_hosts_all`. Fixed for the whole play, batches included.
    pub(super) all_play_hosts: Vec<String>,
    pub(super) r#become: Option<bool>,
    pub(super) become_user: Option<String>,
    /// The module payloads this run sends, built once before the first connection. Shared rather
    /// than copied: it is the base64 of a 631 KB zip and every host of every batch reads it.
    pub(super) python: Option<Arc<crate::python::Union>>,
    /// `--step`: what the operator answered to each task of the batch.
    pub(super) step: Option<super::debugger::Stepper>,
}

/// The play's `strategy`. `Debug` walks as `Linear` does and opens the debugger prompt on a
/// failed task; `Free` lets every host walk its steps without waiting for the others; and
/// `HostPinned` is `Free` with a host kept by one fork from its first step to its last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Strategy {
    Linear,
    Free,
    HostPinned,
    Debug,
}

impl Strategy {
    /// The strategy a play names. The pre-flight has refused every other name by now.
    pub(super) fn named(name: Option<&str>) -> Self {
        match name {
            Some("free") => Self::Free,
            Some("host_pinned") => Self::HostPinned,
            Some("debug") => Self::Debug,
            _ => Self::Linear,
        }
    }

    /// Whether the hosts walk the play without meeting at every step.
    pub(super) fn free(self) -> bool {
        matches!(self, Self::Free | Self::HostPinned)
    }

    pub(super) fn pinned(self) -> bool {
        self == Self::HostPinned
    }
}

impl PlayPlan {
    /// The step list as it stands. An `Arc` clone, so the watch is never held across an `await`.
    pub(super) fn steps(&self) -> Arc<Compiled> {
        self.plan.borrow().clone()
    }
}

/// Drops every value the `omit` variable rendered to, at any depth. Measured against
/// ansible-core 2.19.12, where `omit` is a sentinel the templating engine removes from any
/// container it survives in, sequences included: a mapping loses the key and a list loses the
/// element. Only the value goes, never the container around it, so `{k: omit}` comes back as an
/// empty mapping and `[omit]` as an empty list.
fn remove_omit(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, v| v.as_str() != Some(omit_token()));
            map.values_mut().for_each(remove_omit);
        }
        Value::Array(items) => {
            items.retain(|v| v.as_str() != Some(omit_token()));
            items.iter_mut().for_each(remove_omit);
        }
        _ => {}
    }
}

/// Resolves privilege escalation for one task: whether to escalate, to whom, through which
/// program, and with what password. `None` means the task runs as the connecting user.
/// `become_method`, `become_exe` and `become_flags` take the order the reference gives every
/// setting it maps to a variable: the host's variable, then the task's keyword over its block's
/// over its play's, then the defaults.
///
/// The order is measured, not assumed. Against `ansible-core 2.19.12`: a host's
/// `ansible_become` variable beats both keywords in both directions (a play `become: false`
/// with `ansible_become=true` ran as root, and a task `become: true` with
/// `ansible_become=false` ran as the invoking user), and between the keywords the task beats
/// the play. `ansible_become_user` follows the same order. The connection defaults, which carry
/// `ansible.cfg`, its environment variables and the command line, speak last.
#[expect(clippy::too_many_lines)]
fn become_for(
    task: &PlayTask,
    play: &PlayPlan,
    vars: &HostVars,
    defaults: &ConnectionDefaults,
    templar: &Templar,
) -> Result<Option<Escalation>, TemplateError> {
    let on = host_setting(vars, "ansible_become")
        .and_then(as_bool_value)
        .or(task.r#become)
        .or(play.r#become)
        .unwrap_or(defaults.r#become);
    if !on {
        return Ok(None);
    }
    // The method is refused for the task that would actually use it, so a task escalating
    // nowhere is unaffected by a method it never runs. A variable beats the keyword, which beats
    // the defaults that carry `ansible.cfg` and the command line: this catches the value
    // reaching an escalating task through `group_vars`, `host_vars`, `--extra-vars` or a
    // `set_fact`, which the startup pass cannot see.
    let (method, source) = match host_setting(vars, "ansible_become_method").and_then(Value::as_str)
    {
        Some(method) => (method, "ansible_become_method"),
        None => (
            task.connect
                .become_method
                .as_deref()
                .unwrap_or(&defaults.become_method),
            "become_method",
        ),
    };
    let method = BecomeMethod::named(method)
        .ok_or_else(|| TemplateError(format!("{source} '{method}' is not supported yet")))?;
    // `become_exe` and `become_flags` in the same order, each a template or not, then split into
    // the words the escalation program is given.
    let program = |var: &str, keyword: &Option<String>, default: &Option<String>| {
        let raw = match host_setting(vars, var).and_then(Value::as_str) {
            Some(text) => text.to_string(),
            None => match keyword.as_ref().or(default.as_ref()) {
                Some(text) => text.clone(),
                None => return Ok(None),
            },
        };
        let key = var.trim_start_matches("ansible_");
        let text = if Templar::is_template(&raw) {
            match templar
                .render(&raw, vars)
                .map_err(|err| TemplateError(format!("rendering '{key}' {raw}: {}", err.0)))?
            {
                Value::String(text) => text,
                _ => return Err(TemplateError(format!("'{key}' must render to text: {raw}"))),
            }
        } else {
            raw
        };
        shlex::split(&text).map(Some).ok_or_else(|| {
            TemplateError(format!(
                "'{key}' has unbalanced quotes and cannot be split into words: {text}"
            ))
        })
    };
    let exe = program(
        "ansible_become_exe",
        &task.connect.become_exe,
        &defaults.become_exe,
    )?
    .unwrap_or_default();
    let flags = program(
        "ansible_become_flags",
        &task.connect.become_flags,
        &defaults.become_flags,
    )?;
    let user = match host_setting(vars, "ansible_become_user").and_then(Value::as_str) {
        Some(user) => user.to_string(),
        None => task
            .become_user
            .clone()
            .or_else(|| play.become_user.clone())
            .unwrap_or_else(|| defaults.become_user.clone()),
    };
    // `become_user: "{{ app_user }}"` is ordinary Ansible, and the rendered name keys the link,
    // so it is resolved before the connection is opened. It renders against the task's variables
    // and not one loop item's: the reference escalates per item, and this engine diverges there
    // because one batch is one message to one agent under one user. The hint below names the
    // keyword, and names the loop variable only on a task that actually loops.
    let user = if Templar::is_template(&user) {
        templar
            .render(&user, vars)
            .map_err(|err| {
                let hint = match (task.loop_items.is_some(), user.contains(&task.loop_var)) {
                    (true, true) => {
                        ". A 'become_user' that changes per loop item is not supported yet: one \
                         batch escalates to one user"
                            .to_string()
                    }
                    (false, true) => format!(
                        ". '{}' is only defined while a task loops, and this task has no 'loop'",
                        task.loop_var
                    ),
                    _ => String::new(),
                };
                TemplateError(format!("rendering 'become_user' {user}: {}{hint}", err.0))
            })?
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| {
                TemplateError(format!("'become_user' must render to a user name: {user}"))
            })?
    } else {
        user
    };
    let password = host_setting(vars, "ansible_become_password")
        .or_else(|| host_setting(vars, "ansible_become_pass"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| defaults.become_password.clone());
    Ok(Some(Escalation {
        method,
        user,
        password,
        exe,
        flags,
    }))
}

/// The task's own name for the `FAILED - RETRYING` line: templated against `vars`, and never
/// role-prefixed the way `task_name`'s banner is - measured, the retry line of a task inside a
/// role carries the task's own name rather than the `role : name` the banner shows.
pub(super) fn retry_name(task: &PlayTask, vars: &HostVars, templar: &Templar) -> String {
    if Templar::is_template(&task.name) {
        templar
            .render(&task.name, vars)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| task.name.clone())
    } else {
        task.name.clone()
    }
}

/// The merged, self-resolved variables of a host for one task. `live` is the progress the
/// coordinator last published: its batch list feeds `ansible_play_batch` and its play-wide list
/// `ansible_play_hosts`, the two being the same list until `serial` parts them.
///
/// `role` decides which of the two role layers the step sits on: its own role's, which carry the
/// whole play's exported values with that role's own laid over them, or the play-wide export for
/// a step that belongs to no role. Role parameters are the one layer that does not leave the
/// role - measured, a parameter beats a `set_fact` inside the role and is not defined at all in
/// the play's own tasks afterwards.
#[expect(
    clippy::too_many_arguments,
    reason = "the driver's whole context, read for one host's variables"
)]
pub(super) fn host_vars(
    host: &str,
    plan: &PlayPlan,
    task_vars: &Map<String, Value>,
    role: Option<usize>,
    include_params: Option<&IncludeParams>,
    live: &Progress,
    templar: &Templar,
    store: &Mutex<VarStore>,
    origin: &Origin,
    playbook_dir: &Path,
) -> HostVars {
    let compiled = plan.steps();
    let role = role
        .and_then(|i| compiled.roles.get(i))
        .unwrap_or(&compiled.exported);
    // What an include handed down joins the role parameters rather than the task variables:
    // measured, it beats a `set_fact` of the same name, which a task's own `vars:` does not.
    let mut role_params = role.params.clone();
    for (key, value) in include_params.iter().flat_map(|p| &p.values) {
        role_params.insert(key.clone(), value.clone());
    }
    let scope = Scope {
        play_vars: plan.play_vars.clone(),
        vars_files: plan.vars_files.get(host).cloned().unwrap_or_default(),
        task_vars: task_vars.clone(),
        role_defaults: role.defaults.clone(),
        role_vars: role.vars.clone(),
        role_params,
        play_hosts: live.play_hosts_left.clone(),
        batch_hosts: live.live_hosts.clone(),
        all_play_hosts: plan.all_play_hosts.clone(),
    };
    let mut vars = {
        let mut store = store.lock().expect("vars lock");
        // The shared views first: a variable of this host's own can name `hostvars` or `groups`,
        // and `resolve_vars` below has to be able to answer it.
        let hostvars = store.hostvars_shared(host);
        let mut untrusted = store.untrusted_of(host, &scope);
        // What an include handed down is already rendered, so its provenance cannot be read off
        // the store: it travelled with the values.
        untrusted.extend(
            include_params
                .iter()
                .flat_map(|p| p.untrusted.iter().cloned()),
        );
        let untrusted_hosts = store.untrusted_hosts();
        let layers = store.layered_for_host(host, &scope);
        HostVars {
            map: layers.map,
            hostvars,
            shared: layers.shared,
            facts: layers.facts,
            untrusted,
            untrusted_hosts,
        }
    };
    // Before anything of the task's own `vars:` resolves, never after: a lookup one of them
    // calls (`first_found`, `template`) reads these two names to find its file, and a pass that
    // resolves the task's variables without them first is a pass that lookup cannot complete on.
    insert_search_path(&mut vars, origin, playbook_dir);
    // The merged map carries this host's facts, so the names that came from a managed host
    // travel into the resolution below and are left there exactly as they arrived. The set comes
    // back wider than it went in: a `vars:` of the play, of a block or of the task itself built
    // from a registered value is data too, and this resolution is the only place that can tell.
    let (map, untrusted) = templar.resolve_vars_tainted(&vars);
    vars.map = map;
    vars.untrusted = untrusted;
    vars
}

/// A task rendered for one host: what to do with it.
pub(super) enum Prepared {
    /// Every item (or the single non-loop item) had a false `when`: results are ready. No
    /// delegate travels with it: measured on ansible-core 2.19.12, a task a `when` left out
    /// prints `skipping: [h1]` with no arrow, because it never reached the delegate.
    Skipped(Vec<Item>),
    /// `set_fact` or `debug`: run on the controller. A `delegate_to` changes nothing about
    /// where it runs - measured, a delegated `debug` still runs here - so the delegate travels
    /// only as the name the line shows.
    Local(Vec<Item>, Option<String>),
    /// Send to the agent, one `Task` per item, over a link running as this task's escalated
    /// user. Escalation belongs to the task rather than to an item: it decides which agent on
    /// the host the whole task talks to, so every item of a loop shares it.
    ///
    /// The host is the delegate when the task has one, and that is the host the link is opened
    /// to, keyed by, and reused from: measured on ansible-core 2.19.12, a task delegated away
    /// from a host nothing can reach runs perfectly well, so the delegating host's own
    /// connection is never opened for it. The delegate travels with its own effective
    /// variables, which are what its connection is built from; the task's own host needs none
    /// here, since every item already carries them.
    Remote(
        Vec<Item>,
        Option<Box<Escalation>>,
        Option<(String, HostVars)>,
        /// Present when this task is a Python module: the module's half of the payload every
        /// item of it runs with. One per task rather than one per item, because the module is
        /// the task's, not the item's - a loop varies the arguments, never the module.
        ///
        /// The module's half only. The interpreter is the host's, and this is built before a
        /// link to that host exists, so what would go in that field here is a guess; the wire
        /// payload is assembled where the chosen interpreter is in hand. A payload carrying an
        /// interpreter nobody chose is a module running under something other than what was
        /// asked for, so it is made unrepresentable rather than filled in later.
        ///
        /// `None` for every task this release runs today: a module that needs a payload is
        /// refused before the first connection, so nothing reaches here carrying one until the
        /// driver has a union blob to name.
        ///
        /// Boxed because it is three strings and a map against the two pointers of the variant
        /// beside it, and every `Prepared` a run builds - one per task and per host, payload or
        /// not - would otherwise be that size. Clippy's `large_enum_variant` is denied here and
        /// says so; unboxing it is not a simplification.
        Option<Box<ModulePayload>>,
        /// Present when an action plugin this release runs backs the task. Such a task carries
        /// no payload of its own: the sub-tasks the plugin asks for each carry theirs.
        Option<Kind>,
        /// The task's `remote_user`, `port` and `connection`, rendered: they decide the link
        /// together with the host's own variables, and a task that differs ends the batch.
        Box<ConnectionKeywords>,
    ),
}

/// What `loop_control`'s `extended`, `extended_allitems` and `pause` come to once rendered.
pub(super) struct LoopOptions {
    pub(super) extended: bool,
    pub(super) allitems: bool,
    pub(super) pause: Option<std::time::Duration>,
}

/// The three `loop_control` fields the reference templates per task, rendered against `vars` and
/// converted. Measured on ansible-core 2.19.12: a value that does not convert fails the task
/// with `Error processing keyword 'pause': The value 'nope' could not be converted to 'float'.`,
/// and an undefined name with `Error processing keyword 'pause': 'x' is undefined`; a negative
/// `pause` is accepted and waits for nothing.
pub(super) fn loop_options(
    task: &PlayTask,
    vars: &HostVars,
    templar: &Templar,
) -> Result<LoopOptions, TemplateError> {
    let rendered = |key: &str, raw: &Option<Value>| -> Result<Option<Value>, TemplateError> {
        raw.as_ref()
            .map(|raw| {
                templar
                    .render_value(raw, vars)
                    .map_err(|e| TemplateError(format!("{KEYWORD_ERROR}'{key}': {}", e.0)))
            })
            .transpose()
    };
    let unconverted = |key: &str, value: &Value, to: &str| {
        let shown = match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        TemplateError(format!(
            "{KEYWORD_ERROR}'{key}': The value '{shown}' could not be converted to '{to}'."
        ))
    };
    let flag = |key: &str, raw: &Option<Value>, default: bool| -> Result<bool, TemplateError> {
        match rendered(key, raw)? {
            None => Ok(default),
            Some(Value::Number(n)) if n.as_i64() == Some(0) => Ok(false),
            Some(Value::Number(n)) if n.as_i64() == Some(1) => Ok(true),
            Some(value) => as_bool_value(&value).ok_or_else(|| unconverted(key, &value, "bool")),
        }
    };
    let pause = match rendered("pause", &task.loop_pause)? {
        None => None,
        Some(value) => {
            let seconds = match &value {
                Value::Number(n) => n.as_f64(),
                Value::Bool(b) => Some(f64::from(u8::from(*b))),
                Value::String(s) => s.trim().parse::<f64>().ok(),
                _ => None,
            }
            .ok_or_else(|| unconverted("pause", &value, "float"))?;
            if seconds > 0.0 {
                Some(
                    std::time::Duration::try_from_secs_f64(seconds)
                        .map_err(|_| unconverted("pause", &value, "float"))?,
                )
            } else {
                None
            }
        }
    };
    Ok(LoopOptions {
        extended: flag("extended", &task.loop_extended, false)?,
        allitems: flag("extended_allitems", &task.loop_allitems, true)?,
        pause,
    })
}

/// The start of the message a `loop_control` field that fails to render or convert reports.
pub(super) const KEYWORD_ERROR: &str = "Error processing keyword ";

/// The text a task that dies before it runs reports: the reference's `Task failed: ` prefix,
/// except on a keyword error, which it states as it is.
pub(super) fn failure_text(err: &TemplateError) -> String {
    if err.0.starts_with(KEYWORD_ERROR) || err.0.starts_with(NOTIFY_FAILED) {
        err.0.clone()
    } else {
        format!("Task failed: {}", err.0)
    }
}

/// The pause the loop `items` belong to waits between two of them, if it has one.
pub(super) fn pause_of(
    task: &PlayTask,
    items: &[Item],
    templar: &Templar,
) -> Option<std::time::Duration> {
    let first = items.first()?;
    loop_options(task, &first.vars, templar).ok()?.pause
}

/// One loop item (or the whole task when there is no loop), rendered.
pub(super) struct Item {
    /// The loop element, present only for loops.
    pub(super) element: Option<Value>,
    pub(super) label: Option<String>,
    pub(super) args: Map<String, Value>,
    /// The arguments whose render read a value that came from a managed host. Only the ones the
    /// engine reads back as source text consult it; everything else treats an argument as the
    /// data it is and ships it to the agent.
    pub(super) args_untrusted: BTreeSet<String>,
    /// Variables in force for this item, for `changed_when`, `failed_when` and local modules.
    pub(super) vars: HostVars,
    /// The variables the module runs with, this item's layers merged and rendered. Per item
    /// rather than per task because a layer's values are templates and may name the loop
    /// variable.
    pub(super) environment: BTreeMap<String, String>,
    /// Set when `when` was false: the skip result to report.
    pub(super) skipped: Option<TaskResult>,
    /// The task's `ignore_errors` as rendered for this item, when it was written as a template
    /// and the item was not skipped. The readers fall back to the task's keyword without it.
    pub(super) ignore_errors: Option<bool>,
    /// The task's `check_mode` and `diff` as rendered for this item: a keyword written as a
    /// template is rendered per item, and an item a `when` left out renders neither.
    pub(super) check_mode: Option<bool>,
    pub(super) diff: Option<bool>,
    /// A `set_fact`'s keys as rendered, once: see `run::set_fact_names`. Each entry is the key as
    /// written, the name it rendered to, and whether that name holds a managed host's value.
    pub(super) set_fact_names: std::sync::OnceLock<Result<Vec<FactName>, TaskResult>>,
    /// The handler names the task's `notify` rendered to for this item, empty for an item a `when`
    /// left out.
    pub(super) notify: Vec<String>,
}

/// One rendered `set_fact` key: the key as written, the name it rendered to, and whether that
/// name holds a managed host's value.
pub(super) type FactName = (String, String, bool);

/// One task's `environment`, layer by layer: the play's, then each block's, then the task's,
/// each rendered against this item's variables and merged over the ones before it.
///
/// Measured on ansible-core 2.19.12: a layer that does not render to a mapping warns on stderr
/// and is skipped while the task still runs, and the layers around it stay in force. Values are
/// what Python's `str()` makes of them - `42` is `"42"`, `true` is `"True"`, `null` is `"None"`.
///
/// The warning goes into `warnings` rather than onto the terminal: `prepare` runs inside a host
/// driver, and every line a task puts on the terminal belongs on the coordinator's queue, where
/// the `no_log` policy can see it.
fn environment_for(
    task: &PlayTask,
    vars: &HostVars,
    templar: &Templar,
    warnings: &mut Vec<String>,
) -> Result<BTreeMap<String, String>, TemplateError> {
    let mut out = BTreeMap::new();
    for layer in &task.environment {
        // The reference's own prefix for a keyword that cannot be rendered, measured: an
        // undefined variable in an `environment:` fails the task with
        // `Task failed: Error processing keyword 'environment': ...`.
        let rendered = templar.render_value(layer, vars).map_err(|e| {
            TemplateError(format!("Error processing keyword 'environment': {}", e.0))
        })?;
        let Value::Object(map) = rendered else {
            // The raw stack of layers, not the rendered one. Measured on ansible-core 2.19.12
            // with a `no_log` task whose `environment` is `"{{ secret }}"`: the warning reads
            // `could not parse environment value, skipping: ['{{ secret }}']` - brackets
            // included, because the whole list is reported, and unrendered, so the secret never
            // reaches the line. Reporting the rendered value instead put it there.
            warnings.push(format!(
                "could not parse environment value, skipping: {}",
                python_repr(&Value::Array(task.environment.clone()))
            ));
            continue;
        };
        for (k, v) in map {
            out.insert(k, environment_value(&v));
        }
    }
    Ok(out)
}

/// The handler names one item's `notify` renders to. Measured on ansible-core 2.19.12: a template
/// that fails fails the task with `Error processing keyword 'notify': ...` before the module
/// runs; a rendered list notifies each of its names; a number or boolean is looked up as the text
/// `str()` makes of it, so `{{ 5 }}` finds no handler called `5`. A mapping makes the reference
/// crash, and is refused here.
/// The reference names the variable a keyword template read and found missing, bare. The engine
/// cannot say which one, so a template that is nothing but `{{ name }}` is worded here and any
/// other keeps the engine's sentence.
fn undefined_name(template: &str, detail: String) -> String {
    let inner = template
        .trim()
        .strip_prefix("{{")
        .and_then(|t| t.strip_suffix("}}"))
        .map(str::trim);
    match inner {
        Some(name)
            if detail.starts_with("The task includes an option with an undefined variable.")
                && !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
        {
            format!("'{name}' is undefined")
        }
        _ => detail,
    }
}

/// How the message of a `notify` that fails to render starts: the reference's `Task failed: ` and
/// the keyword, which [`failure_text`] leaves alone.
pub(super) const NOTIFY_FAILED: &str = "Task failed: Error processing keyword 'notify': ";

fn notify_for(
    task: &PlayTask,
    vars: &HostVars,
    templar: &Templar,
) -> Result<Vec<String>, TemplateError> {
    let keyword = |detail: String| TemplateError(format!("{NOTIFY_FAILED}{detail}"));
    let mut names = Vec::new();
    for entry in &task.notify {
        let rendered = if Templar::is_template(entry) {
            templar
                .render(entry, vars)
                .map_err(|e| keyword(undefined_name(entry, e.0)))?
        } else {
            Value::String(entry.clone())
        };
        let list = match rendered {
            Value::Array(list) => list,
            one => vec![one],
        };
        for name in list {
            names.push(match name {
                Value::String(s) => s,
                Value::Bool(_) | Value::Number(_) | Value::Null => display(&name),
                other => {
                    return Err(keyword(format!(
                        "{} is not a handler name",
                        python_repr(&other)
                    )));
                }
            });
        }
    }
    Ok(names)
}

/// One environment value as the process sees it, which is what `str()` makes of the Python
/// object the reference hands to `os.environ`. Measured: `{B: true, N: null, S: plain}` reaches
/// the shell as `True`, `None` and `plain`, and `{N: 42}` as `42`.
///
/// A list or a mapping is not measured and is written as JSON here rather than as Python's own
/// repr; nothing in ansible's documentation suggests writing one.
fn environment_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Null => "None".to_string(),
        other => ansible_json(other),
    }
}

/// The play's `gather_subset` as this host renders it: items must be strings, and a template
/// that renders to a string with a comma is refused, since the module would split it where the
/// reference lists it whole. A failure here is the host's own task failure.
fn check_gather_subset(raw: Option<&Value>, rendered: Option<&Value>) -> Result<(), TemplateError> {
    let Some(rendered) = rendered else {
        return Ok(());
    };
    let items = match rendered {
        Value::Array(items) => items.as_slice(),
        one => std::slice::from_ref(one),
    };
    if let Some(bad) = items.iter().find(|item| !item.is_string()) {
        return Err(TemplateError(format!(
            "Keyword 'gather_subset' items must be of type 'str', not '{}'.",
            super::python_type(bad)
        )));
    }
    if let (Some(Value::String(text)), Value::String(out)) = (raw, rendered)
        && Templar::is_template(text)
        && out.contains(',')
    {
        return Err(TemplateError(
            "a gather_subset template that renders to a string with a comma in it is not supported yet"
                .to_string(),
        ));
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "the driver's whole context, plus the warnings the render raises on its way"
)]
#[expect(clippy::too_many_lines)]
pub(super) fn prepare(
    step: &Step,
    host: &str,
    plan: &PlayPlan,
    live: &Progress,
    templar: &Templar,
    store: &Mutex<VarStore>,
    defaults: &ConnectionDefaults,
    warnings: &mut Vec<String>,
) -> Result<Prepared, TemplateError> {
    let task = &step.task;
    let playbook_dir = store
        .lock()
        .expect("vars lock")
        .playbook_dir()
        .to_path_buf();
    let base = host_vars(
        host,
        plan,
        &task.vars,
        step.role,
        step.include_params.as_deref(),
        live,
        templar,
        store,
        &step.origin,
        &playbook_dir,
    );
    // Whether the list this loop walks came from a managed host. A loop over a literal list the
    // playbook wrote binds author content; one over `{{ r.stdout_lines }}` binds data, and the
    // items of such a loop are never rendered again.
    let mut items_from_host = false;
    // An undefined variable in the loop is kept rather than raised, the way the reference's
    // `TaskExecutor.run` keeps `_loop_eval_error`: the task's `when`, read without an item,
    // decides first, and the error is raised only if the task would run. Any other loop error
    // is raised at once, as there.
    let mut loop_error = None;
    let listed = match &task.loop_items {
        None => None,
        Some(raw) if let Some(lookup) = &task.loop_with => {
            Some(with_lookup_items(templar, lookup, &task.module, raw, &base))
        }
        Some(raw) => Some(loop_list(templar, task, raw, &base)),
    };
    let elements: Vec<Option<Value>> = match listed {
        None => vec![None],
        Some(Ok((list, tainted))) => {
            items_from_host = tainted;
            list.into_iter().map(Some).collect()
        }
        Some(Err(e)) if e.is_undefined() => {
            loop_error = Some(e);
            vec![None]
        }
        Some(Err(e)) => return Err(e),
    };
    // The groups and keys of `module_defaults` an include brought in while the play runs have
    // not been asked about yet.
    if !task.module_defaults.is_empty() {
        plan.steps()
            .resolved
            .ensure([task], templar.bridge().map(|bridge| &**bridge))
            .map_err(TemplateError)?;
    }
    let mut items = Vec::new();
    // What `ansible_loop` reads: every item, kept only when the loop asks for `extended`.
    let options = if task.loop_items.is_some() {
        loop_options(task, &base, templar)?
    } else {
        LoopOptions {
            extended: false,
            allitems: true,
            pause: None,
        }
    };
    let list: Vec<&Value> = if options.extended {
        elements.iter().flatten().collect()
    } else {
        Vec::new()
    };
    let list_value = (options.extended && options.allitems)
        .then(|| Value::Array(list.iter().map(|v| (*v).clone()).collect()));
    for (index, element) in elements.iter().cloned().enumerate() {
        let mut vars = base.clone();
        if let Some(el) = &element {
            if items_from_host {
                vars.insert_untrusted(task.loop_var.clone(), el.clone());
            } else {
                vars.insert(task.loop_var.clone(), el.clone());
            }
            vars.insert(
                "ansible_loop_var".into(),
                Value::String(task.loop_var.clone()),
            );
            if let Some(name) = &task.index_var {
                vars.insert(name.clone(), Value::from(index));
            }
            if options.extended {
                let last = list.len() - 1;
                let mut ansible_loop = Map::new();
                ansible_loop.insert("index".into(), Value::from(index + 1));
                ansible_loop.insert("index0".into(), Value::from(index));
                ansible_loop.insert("first".into(), Value::Bool(index == 0));
                ansible_loop.insert("last".into(), Value::Bool(index == last));
                ansible_loop.insert("length".into(), Value::from(list.len()));
                ansible_loop.insert("revindex".into(), Value::from(list.len() - index));
                ansible_loop.insert("revindex0".into(), Value::from(last - index));
                if let Some(all) = &list_value {
                    ansible_loop.insert("allitems".into(), all.clone());
                }
                if index < last {
                    ansible_loop.insert("nextitem".into(), list[index + 1].clone());
                }
                if index > 0 {
                    ansible_loop.insert("previtem".into(), list[index - 1].clone());
                }
                // Its items are the loop's, so they are as much a managed host's as the
                // loop variable is.
                if items_from_host {
                    vars.insert_untrusted("ansible_loop".into(), Value::Object(ansible_loop));
                } else {
                    vars.insert("ansible_loop".into(), Value::Object(ansible_loop));
                }
            }
            // Variables naming the loop variable could not resolve before it was bound. One of
            // them may be built from the item, so this pass widens the set the same way.
            let (resolved, untrusted) = templar.resolve_vars_tainted(&vars);
            vars.map = resolved;
            vars.untrusted = untrusted;
        }
        let label = match (&element, &task.loop_label) {
            (None, _) => None,
            (Some(_), Some(template)) => Some(display(&templar.render(template, &vars)?)),
            (Some(el), None) => Some(display(el)),
        };
        let mut skipped = None;
        for condition in &task.when {
            // A conditional that cannot be evaluated yields to a kept loop error, as there.
            let holds = templar
                .condition(condition, &vars)
                .map_err(|e| loop_error.clone().unwrap_or(e))?;
            if !holds {
                let mut r = Map::new();
                r.insert("changed".into(), json!(false));
                r.insert("skipped".into(), json!(true));
                r.insert("skip_reason".into(), json!("Conditional result was False"));
                r.insert("false_condition".into(), json!(condition));
                skipped = Some(TaskResult(r));
                break;
            }
        }
        if skipped.is_none()
            && let Some(e) = loop_error.take()
        {
            return Err(e);
        }
        // Rendered argument by argument rather than as one value, because a few arguments are
        // read back as engine input rather than as data - the name a `debug: var:` compiles -
        // and the render is the only place that can say which of them came from a host.
        let (args, args_untrusted) = if skipped.is_some() {
            (Map::new(), BTreeSet::new())
        } else {
            // `assert` reads `that` off the task as written and evaluates it the way `when` is
            // evaluated, so it is not rendered here: a render error in it would fail the task in
            // the render's words before the conditional could report it, which the reference's
            // `finalize_task_arg` for `assert` does not do.
            //
            let raw = if volant_protocol::modules::short_name(&task.module) == "assert" {
                let mut raw = task.args.clone();
                raw.remove("that");
                std::borrow::Cow::Owned(raw)
            } else {
                std::borrow::Cow::Borrowed(&task.args)
            };
            let finalizing = |err: TemplateError| {
                if err.0.starts_with(crate::template::RESOLVING) {
                    TemplateError(format!(
                        "{FINALIZING}'{}' failed: {}",
                        super::run::action_fqcn(&task.module),
                        err.0
                    ))
                } else {
                    err
                }
            };
            let (mut map, mut untrusted) = templar
                .render_map_tainted(&raw, &vars)
                .map_err(finalizing)?;
            if task.name == "Gathering Facts" && task.module == "ansible.legacy.setup" {
                check_gather_subset(raw.get("gather_subset"), map.get("gather_subset"))?;
            }
            // The task's `module_defaults` lie under its own arguments, each rendered once on
            // its own: a default is a template like an argument, and may be a whole template
            // that renders to a mapping. A task's argument wins, `omit` included.
            if !task.module_defaults.is_empty() {
                let mut from_default = BTreeSet::new();
                let mut defaults = crate::collection::defaults_for(
                    &task.module,
                    &task.module_defaults,
                    &plan.steps().resolved.groups(),
                    &mut |key, value| {
                        render_default(templar, &vars, key, value, &mut from_default)
                            .map_err(|err| finalizing(err).0)
                    },
                )
                .map_err(TemplateError)?;
                from_default.retain(|name| !map.contains_key(name));
                untrusted.extend(from_default);
                defaults.extend(std::mem::take(&mut map));
                map = defaults;
            }
            let mut rendered = Value::Object(map);
            remove_omit(&mut rendered);
            let Value::Object(map) = rendered else {
                unreachable!("an object renders to an object")
            };
            (map, untrusted)
        };
        // Per item, against the item's own variables, and never for an item a `when` left out:
        // the reference evaluates the conditional before it renders the task's keywords.
        let ignore_errors = match (&task.ignore_errors, &skipped) {
            (Some(Flag::Template(raw)), None) => Some(ignore_errors_for(raw, &vars, templar)?),
            _ => None,
        };
        let check_mode = flag_for(
            "check_mode",
            task.check_mode.as_ref(),
            &vars,
            templar,
            skipped.is_some(),
        )?;
        let diff = flag_for(
            "diff",
            task.diff.as_ref(),
            &vars,
            templar,
            skipped.is_some(),
        )?;
        // A skipped item runs nothing, so a layer it could not render is not its problem: the
        // reference does not evaluate `environment` for a task a `when` left out.
        let environment = if skipped.is_some() {
            BTreeMap::new()
        } else {
            environment_for(task, &vars, templar, warnings)?
        };
        // As `environment`: a keyword the reference renders before the module runs, so an
        // undefined name fails the task even when nothing changes, and a skipped item renders none.
        let notify = if skipped.is_some() {
            Vec::new()
        } else {
            notify_for(task, &vars, templar)?
        };
        items.push(Item {
            element,
            label,
            args,
            args_untrusted,
            vars,
            environment,
            skipped,
            ignore_errors,
            check_mode,
            diff,
            set_fact_names: std::sync::OnceLock::new(),
            notify,
        });
    }
    if items.iter().all(|i| i.skipped.is_some()) {
        return Ok(Prepared::Skipped(items));
    }
    let delegate = delegate_for(task, &base, templar)?;
    if is_local(&task.module) || crate::action_plugins::controller_side(&task.module) {
        return Ok(Prepared::Local(items, delegate));
    }
    let connect = Box::new(connect_for(task, &base, templar)?);
    // A delegate's variables come back merged under the full precedence, like any host's: its
    // connection is decided by `-e ansible_connection=local`, by a `group_vars` file and by the
    // task's own `vars:` exactly as the delegating host's is. Measured on ansible-core 2.19.12:
    // a `delegate_to` naming a host the inventory has never heard of is connected to rather than
    // refused - `delegate_to: nosuch` reports `fatal: [h1 -> nosuch]: UNREACHABLE!` with ssh's
    // own resolution error and exits 4 - so an unknown name is an ssh target of that name, not a
    // load-time error. The implicit spellings get their local connection from
    // `VarStore::for_host`, which is where every host's view is built and where the delegating
    // host's own implicit `localhost` gets it too.
    let delegate = delegate.map(|name| {
        let vars = host_vars(
            &name,
            plan,
            &task.vars,
            step.role,
            step.include_params.as_deref(),
            live,
            templar,
            store,
            &step.origin,
            &playbook_dir,
        );
        (name, vars)
    });
    // From the delegating host's own variables, measured on ansible-core 2.19.12:
    // `ansible_become_user` under a `delegate_to` still reads the value the **delegating**
    // host carries, while `ansible_host` and `ansible_connection` read the delegate's. So the
    // link goes to the delegate and escalates to the user the task's own host asked for.
    let escalation = become_for(task, plan, &base, defaults, templar)?.map(Box::new);
    if let Some(kind) = crate::action_plugins::kind(&task.module) {
        return Ok(Prepared::Remote(
            items,
            escalation,
            delegate,
            None,
            Some(kind),
            connect,
        ));
    }
    // The module's half of the payload, when this module is one the run built one for. The
    // interpreter is not here: it is the host's, and no link to it exists yet.
    //
    // A Python module the union does not hold is one nothing could have named when the union was
    // built - a dynamic include resolves its file while the play runs - and it travels with no
    // payload, which the batch refuses by name rather than sending.
    let payload = plan
        .python
        .as_ref()
        .and_then(|union| union.payload(&task.module))
        .map(Box::new);
    Ok(Prepared::Remote(
        items, escalation, delegate, payload, None, connect,
    ))
}

/// One `module_defaults` value as the mapping it stands for: a mapping with each of its values
/// rendered, or a whole template that renders to one. The names that read something a managed
/// host wrote go into `untrusted`, so the arguments they become are never rendered again.
fn render_default(
    templar: &Templar,
    vars: &HostVars,
    key: &str,
    value: &Value,
    untrusted: &mut BTreeSet<String>,
) -> Result<Map<String, Value>, TemplateError> {
    let not_a_mapping = || TemplateError(format!("module_defaults for '{key}' must be a mapping"));
    match value {
        Value::Null => Ok(Map::new()),
        Value::Object(map) => {
            let (map, tainted) = templar.render_map_tainted(map, vars)?;
            untrusted.extend(tainted);
            Ok(map)
        }
        other => {
            let mut wrapped = Map::new();
            wrapped.insert("value".into(), other.clone());
            let (mut rendered, tainted) = templar.render_map_tainted(&wrapped, vars)?;
            match rendered.remove("value") {
                Some(Value::Object(map)) => {
                    if tainted.contains("value") {
                        untrusted.extend(map.keys().cloned());
                    }
                    Ok(map)
                }
                Some(Value::Null) => Ok(Map::new()),
                _ => Err(not_a_mapping()),
            }
        }
    }
}

/// The task's `remote_user`, `port` and `connection`, rendered once for the task against its
/// variables, for the reason `delegate_for` renders `delegate_to` that way. An empty result is
/// no keyword, which is what `"{{ maybe | default('') }}"` renders to when nobody asked.
fn connect_for(
    task: &PlayTask,
    vars: &HostVars,
    templar: &Templar,
) -> Result<ConnectionKeywords, TemplateError> {
    let render = |key: &str, raw: &Option<String>| -> Result<Option<String>, TemplateError> {
        let Some(raw) = raw else {
            return Ok(None);
        };
        let text = if Templar::is_template(raw) {
            match templar
                .render(raw, vars)
                .map_err(|err| TemplateError(format!("rendering '{key}' {raw}: {}", err.0)))?
            {
                Value::String(text) => text,
                // `port: "{{ ssh_port }}"` with a number behind it.
                Value::Number(number) => number.to_string(),
                _ => return Err(TemplateError(format!("'{key}' must render to text: {raw}"))),
            }
        } else {
            raw.clone()
        };
        Ok(Some(text.trim().to_string()).filter(|text| !text.is_empty()))
    };
    let connect = &task.connect;
    let port = render("port", &connect.port)?
        .map(|text| {
            text.parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| {
                    TemplateError(format!("'port' must be a port number, found '{text}'"))
                })
        })
        .transpose()?;
    // A value a template read from a host can end up here, so what reaches `ssh` is held to the
    // shape of a name: nothing that starts like an option, splits into words or ends a line.
    let user = render("remote_user", &connect.remote_user)?;
    if let Some(user) = &user
        && (user.starts_with('-')
            || user
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '"'))
    {
        return Err(TemplateError(format!(
            "'remote_user' must be a user name, found '{}'",
            user.escape_debug()
        )));
    }
    let connection = render("connection", &connect.connection)?;
    if let Some(name) = &connection
        && !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'))
    {
        return Err(TemplateError(format!(
            "'connection' must be a plugin name, found '{}'",
            name.escape_debug()
        )));
    }
    Ok(ConnectionKeywords {
        user,
        port,
        connection,
    })
}

/// `delegate_to`, rendered once for the task. An empty name is no delegation, which is what
/// `delegate_to: "{{ maybe | default('') }}"` renders to when nobody asked for one.
///
/// Rendered against the task's variables and **not** against one loop item's, the way
/// `become_user` is and for the same reason: one batch is one message to one agent over one
/// link, and a task whose items each name a different delegate would have to split across links
/// and interleave the answers. The reference does delegate per item; that divergence is named
/// here rather than left to look like the operator's own typo.
fn delegate_for(
    task: &PlayTask,
    vars: &HostVars,
    templar: &Templar,
) -> Result<Option<String>, TemplateError> {
    let Some(raw) = &task.delegate_to else {
        return Ok(None);
    };
    let name = if Templar::is_template(raw) {
        templar
            .render(raw, vars)
            .map_err(|err| {
                let hint = if task.loop_items.is_some() && raw.contains(&task.loop_var) {
                    ". A 'delegate_to' that changes per loop item is not supported yet: one batch runs over one link"
                        .to_string()
                } else {
                    String::new()
                };
                TemplateError(format!("rendering 'delegate_to' {raw}: {}{hint}", err.0))
            })?
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| {
                TemplateError(format!("'delegate_to' must render to a host name: {raw}"))
            })?
    } else {
        raw.clone()
    };
    Ok(Some(name).filter(|n| !n.is_empty()))
}

/// `role_path` and `ansible_search_path`, which a role's tasks and the `first_found` and
/// `template` lookups read to find files. Measured on ansible-core 2.19.12, in a role's task:
/// `role_path` is the role's directory and `ansible_search_path` is `[<role>, <role>/tasks,
/// <playbook>]`. Outside a role the path is `[<playbook>]` and `role_path` is not defined, so
/// reading it fails on the undefined name, as in the reference.
///
/// The middle entry is the directory of the file the task was written in, and a directory already
/// listed is not listed again, which is how a playbook's own task ends up with one entry.
/// Inserted as author content, and into the raw map a task's own `vars:` resolve against rather
/// than into the resolved `HostVars` afterwards: a `vars:` built from a lookup that needs one of
/// these two names (`first_found`, `template`) has to find it on the same pass that resolves the
/// task's variables, or it is left as the unrendered call, with no later pass to pick it back up.
/// A host fact of the same name does not outlive them, so `untrusted` drops both names too.
fn insert_search_path(vars: &mut HostVars, origin: &Origin, playbook_dir: &Path) {
    let mut search: Vec<&Path> = Vec::new();
    for dir in origin
        .role_dir
        .as_deref()
        .into_iter()
        .chain([origin.file_dir.as_path(), playbook_dir])
    {
        if !search.contains(&dir) {
            search.push(dir);
        }
    }
    let search = search
        .iter()
        .map(|dir| Value::String(dir.display().to_string()))
        .collect();
    // `HostVars::insert`: the engine's own value beats a gathered fact of the same name, and it
    // is trusted whatever the name held before.
    vars.insert("ansible_search_path".into(), Value::Array(search));
    if let Some(role) = &origin.role_dir {
        vars.insert(
            "role_path".into(),
            Value::String(role.display().to_string()),
        );
    }
}

/// `ignore_errors` written as a template, rendered for one item and read the way the reference
/// reads a boolean keyword ([`strict_boolean`]). Measured on ansible-core 2.19.12, the reference
/// renders it when the task runs, and a value that is not a boolean fails the task with the
/// sentence the loader refuses a literal with.
fn ignore_errors_for(raw: &str, vars: &HostVars, templar: &Templar) -> Result<bool, TemplateError> {
    boolean_keyword("ignore_errors", raw, vars, templar)
}

/// A task's `check_mode` or `diff` for one item: a literal as written, a template rendered
/// against the item's variables, and nothing for an item a `when` left out.
fn flag_for(
    key: &str,
    flag: Option<&Flag>,
    vars: &HostVars,
    templar: &Templar,
    skipped: bool,
) -> Result<Option<bool>, TemplateError> {
    match flag {
        Some(Flag::Fixed(b)) => Ok(Some(*b)),
        Some(Flag::Template(raw)) if !skipped => boolean_keyword(key, raw, vars, templar).map(Some),
        _ => Ok(None),
    }
}

/// A boolean keyword written as a template, rendered and read as ansible-core reads it.
fn boolean_keyword(
    key: &str,
    raw: &str,
    vars: &HostVars,
    templar: &Templar,
) -> Result<bool, TemplateError> {
    let keyword =
        |detail: String| TemplateError(format!("Error processing keyword '{key}': {detail}"));
    let rendered = templar.render(raw, vars).map_err(|e| keyword(e.0))?;
    strict_boolean(&rendered).ok_or_else(|| {
        keyword(format!(
            "The value {} could not be converted to 'bool'.",
            python_repr(&rendered)
        ))
    })
}

/// ansible-core's `boolean(value, strict=True)`, ported from
/// `ansible/module_utils/parsing/convert_bool.py` (2.19.12), which a boolean keyword is read
/// with: a real boolean; `1`, `1.0`, `0`, `0.0`; or one of `y yes on 1 true t` and
/// `n no off 0 false f`, trimmed and in any case. Anything else is not a boolean.
fn strict_boolean(value: &Value) -> Option<bool> {
    let word = match value {
        Value::Bool(b) => return Some(*b),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.trim().to_lowercase(),
        _ => return None,
    };
    match word.as_str() {
        "y" | "yes" | "on" | "1" | "1.0" | "true" | "t" => Some(true),
        "n" | "no" | "off" | "0" | "0.0" | "-0.0" | "false" | "f" => Some(false),
        _ => None,
    }
}

/// The list a `loop:` or `with_items:` walks, and whether it came from a managed host.
fn loop_list(
    templar: &Templar,
    task: &PlayTask,
    raw: &Value,
    vars: &HostVars,
) -> Result<(Vec<Value>, bool), TemplateError> {
    let (rendered, tainted) = templar.render_value_tainted(raw, vars)?;
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
    Ok((list, tainted))
}

/// The items of a `with_<lookup>` loop: the terms, rendered, handed to the lookup as its
/// arguments, and what it returns taken as a list. Read from ansible-core 2.19.12's
/// `TaskExecutor._get_loop_items`: a string term is resolved and anything that is not a list
/// becomes a list of one, and the lookup runs with `wantlist=True`.
///
/// `with_first_found` alone drops a term inside the list that renders undefined: its plugin does
/// that when it is invoked as `with_` (`_recurse_terms(terms, omit_undefined=True)`). This is what
/// lets the `raspberrypi` role's task, which names `detected_distribution` before any host has
/// set it, reach its `when` on a host that is not a Pi.
///
/// The second half of the answer is whether the terms read a managed host. A path the lookup
/// found is controller content, but one built from a fact is not the playbook's own choice, so
/// the items of such a loop are bound as data, like those of a `loop:` over a registered list.
fn with_lookup_items(
    templar: &Templar,
    lookup: &str,
    module: &str,
    raw: &Value,
    vars: &HostVars,
) -> Result<(Vec<Value>, bool), TemplateError> {
    let mut tainted = false;
    // A term written as one string is resolved before the lookup sees it (`resolve_to_container`
    // in `_get_loop_items`), so an undefined variable there is an error even for `first_found`.
    let omit_undefined = lookup == "first_found" && !raw.is_string();
    let terms = render_terms(templar, raw, vars, omit_undefined, &mut tainted)?;
    let terms = match terms {
        Some(Value::Array(terms)) => terms,
        Some(term) => vec![term],
        None => Vec::new(),
    };
    // Bound in a copy only this call reads, under a name no playbook writes: the terms reach the
    // lookup as values, never as text to compile.
    const TERMS: &str = "__volant_with_terms";
    let mut scope = vars.clone();
    scope.insert(TERMS.into(), Value::Array(terms));
    // What `first_found.py` does when it runs for a `with_` loop: it searches a subdirectory of
    // each entry first, chosen by the task's action as written - `templates` when the name holds
    // `template`, `vars` when it holds `var`, `files` otherwise - and a plain `lookup()` never does.
    // `template/lookups.rs` reads the same name.
    if lookup == "first_found" {
        let subdir = ["template", "var", "file"]
            .into_iter()
            .find(|word| module.contains(word))
            .unwrap_or("file");
        scope.insert(
            "volant::first_found_subdir".into(),
            Value::String(format!("{subdir}s")),
        );
    }
    let (found, from_lookup) = templar.evaluate_tainted(
        &format!("lookup({lookup:?}, *{TERMS}, wantlist=True)"),
        &scope,
    )?;
    // What a lookup read at run time is data, as a host's answer is: its items are never rendered.
    tainted |= from_lookup;
    let list = match found {
        Value::Array(list) => list,
        one => vec![one],
    };
    Ok((list, tainted))
}

/// Every string of `raw` rendered on its own, so an undefined one can be left out rather than
/// fail the whole list when `omit_undefined` says so. `None` is a value left out.
fn render_terms(
    templar: &Templar,
    raw: &Value,
    vars: &HostVars,
    omit_undefined: bool,
    tainted: &mut bool,
) -> Result<Option<Value>, TemplateError> {
    Ok(Some(match raw {
        Value::String(_) => match templar.render_value_tainted(raw, vars) {
            Ok((value, from_host)) => {
                *tainted |= from_host;
                value
            }
            Err(e) if omit_undefined && e.is_undefined() => return Ok(None),
            Err(e) => return Err(e),
        },
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                out.extend(render_terms(templar, item, vars, omit_undefined, tainted)?);
            }
            Value::Array(out)
        }
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                if let Some(value) = render_terms(templar, value, vars, omit_undefined, tainted)? {
                    out.insert(key.clone(), value);
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }))
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
    volant_protocol::modules::local(module).is_some()
}

/// The `(item=...)` label: Python's `str` of the item, which is a string as it is and anything
/// else as its `repr`. Measured on ansible-core 2.19.12: `(item={'r': 1, 'd': 'nan'})` in the
/// playbook's key order, `(item=[1, 'a', True, None])`.
pub(super) fn display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => python_repr(other),
    }
}

#[cfg(test)]
mod tests;
