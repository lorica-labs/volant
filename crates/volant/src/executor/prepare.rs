// SPDX-License-Identifier: GPL-3.0-or-later
//! Rendering one task for one host: variables, escalation, environment and loop items.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};
use tokio::sync::watch;
use volant_protocol::TaskResult;

use crate::action_plugins::Kind;
use crate::compile::{Compiled, IncludeParams, Origin, Step};
use crate::playbook::{Flag, PlayTask, python_repr};
use crate::python::ModulePayload;
use crate::render::ansible_json;
use crate::template::{Templar, TemplateError};
use crate::transport::{ConnectionDefaults, Escalation};
use crate::vars::{HostVars, Scope, VarStore, host_setting, omit_token};

use super::as_bool_value;
use super::coordinator::Progress;

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
    // nowhere is unaffected by a method it never runs. A variable beats the defaults, which
    // carry `ansible.cfg` and the command line: this catches the value reaching an escalating
    // task through `group_vars`, `host_vars`, `--extra-vars` or a `set_fact`, which the startup
    // pass cannot see.
    match host_setting(vars, "ansible_become_method").and_then(Value::as_str) {
        Some(method) if method != crate::playbook::BECOME_METHOD => {
            return Err(TemplateError(format!(
                "ansible_become_method '{method}' is not supported yet"
            )));
        }
        None if defaults.become_method != crate::playbook::BECOME_METHOD => {
            return Err(TemplateError(format!(
                "become_method '{}' is not supported yet",
                defaults.become_method
            )));
        }
        Some(_) | None => {}
    }
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
    Ok(Some(Escalation { user, password }))
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
        let (map, shared) = store.layered_for_host(host, &scope);
        HostVars {
            map,
            hostvars,
            shared,
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
        Option<Escalation>,
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
    ),
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
}

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

#[expect(
    clippy::too_many_arguments,
    reason = "the driver's whole context, plus the warnings the render raises on its way"
)]
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
    let mut items = Vec::new();
    for element in elements {
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
            let raw = if volant_protocol::modules::short_name(&task.module) == "assert" {
                let mut raw = task.args.clone();
                raw.remove("that");
                std::borrow::Cow::Owned(raw)
            } else {
                std::borrow::Cow::Borrowed(&task.args)
            };
            let (map, untrusted) = templar.render_map_tainted(&raw, &vars)?;
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
        // A skipped item runs nothing, so a layer it could not render is not its problem: the
        // reference does not evaluate `environment` for a task a `when` left out.
        let environment = if skipped.is_some() {
            BTreeMap::new()
        } else {
            environment_for(task, &vars, templar, warnings)?
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
        });
    }
    if items.iter().all(|i| i.skipped.is_some()) {
        return Ok(Prepared::Skipped(items));
    }
    let delegate = delegate_for(task, &base, templar)?;
    if is_local(&task.module) {
        return Ok(Prepared::Local(items, delegate));
    }
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
    let escalation = become_for(task, plan, &base, defaults, templar)?;
    if let Some(kind) = crate::action_plugins::kind(&task.module) {
        return Ok(Prepared::Remote(
            items,
            escalation,
            delegate,
            None,
            Some(kind),
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
    Ok(Prepared::Remote(items, escalation, delegate, payload, None))
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
    let keyword = |detail: String| {
        TemplateError(format!(
            "Error processing keyword 'ignore_errors': {detail}"
        ))
    };
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
    let found = templar.evaluate(&format!("lookup({lookup:?}, *{TERMS})"), &scope)?;
    // `wantlist=True` for the two lookups this runs: `lookup()` answers no result with an empty
    // string and one result bare, and neither of them can find an empty name.
    let list = match found {
        Value::Array(list) => list,
        Value::String(s) if s.is_empty() => Vec::new(),
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

/// Ansible prints a string item as is and anything else as JSON.
pub(super) fn display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => ansible_json(other),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{hvars, task};
    use super::*;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// Each shape below was run through `ansible-core 2.19.12` as a `debug: msg=` argument
    /// first, and the expectations are what it printed: a key nested three levels down goes,
    /// a bare list element goes too, and every emptied container stays.
    #[test]
    fn omit_leaves_containers_behind_but_never_its_own_value() {
        let omit = json!(omit_token());
        let mut args = json!({
            "toplevel_keep": 9,
            "toplevel_drop": omit,
            "nested": {"keep": 1, "drop": omit, "deeper": {"dropme": omit, "stay": 2}},
            "alist": [omit, 1, {"inlist": omit, "other": 3}],
            "nested_list": [[1, omit], 3],
            "only_omit_list": [omit],
            "empty_after": {"k": omit},
        });
        remove_omit(&mut args);
        assert_eq!(
            args,
            json!({
                "toplevel_keep": 9,
                "nested": {"keep": 1, "deeper": {"stay": 2}},
                "alist": [1, {"other": 3}],
                "nested_list": [[1], 3],
                "only_omit_list": [],
                "empty_after": {},
            })
        );
    }

    /// A task naming a module the run built a payload for is prepared with that payload's
    /// module half; a native task is prepared with none.
    ///
    /// What would make this red: the payload attached by anything other than the module's own
    /// short name - a task would then travel with another module's facts, and the agent would
    /// import one module out of the blob while running another's arguments. Or attached to every
    /// task, which sends a payload with `command`.
    #[test]
    fn a_task_is_prepared_with_the_payload_built_for_its_module() {
        let mut facts = BTreeMap::new();
        facts.insert(
            "lineinfile".to_string(),
            crate::python::ModuleFacts {
                module_fqn: "ansible.modules.lineinfile".into(),
                profile: "legacy".into(),
                rlimit_nofile: 0,
                extensions: Map::new(),
                core: true,
            },
        );
        // A collection's module beside one of the same short name: each task gets its own.
        for (key, fqn) in [
            ("sysctl", "ansible.modules.sysctl"),
            (
                "ansible.posix.sysctl",
                "ansible_collections.ansible.posix.plugins.modules.sysctl",
            ),
        ] {
            facts.insert(
                key.to_string(),
                crate::python::ModuleFacts {
                    module_fqn: fqn.into(),
                    profile: "legacy".into(),
                    rlimit_nofile: 0,
                    extensions: Map::new(),
                    core: true,
                },
            );
        }
        let union = Arc::new(crate::python::Union {
            hash: "ab".into(),
            zip_b64: "UEsDBA==".into(),
            modules: facts,
            refused: BTreeMap::new(),
            natives: crate::python::Natives::default(),
        });
        let payload_of = |module: &str| {
            let mut plan = plan();
            plan.python = Some(Arc::clone(&union));
            let inventory = crate::inventory::Inventory::parse_ini(
                "h1
",
            )
            .expect("an inventory");
            let store = Mutex::new(
                VarStore::new(&inventory, None, Path::new("."), Map::new()).expect("a var store"),
            );
            let mut step = Step {
                kind: crate::compile::StepKind::Task,
                task: task(module),
                block: None,
                section: crate::compile::Section::Body,
                role: None,
                origin: Arc::new(Origin::default()),
                include_params: None,
                hosts: None,
            };
            step.task.args.insert("path".into(), json!("/tmp/x"));
            let mut warnings = Vec::new();
            let prepared = prepare(
                &step,
                "h1",
                &plan,
                &Progress::default(),
                &Templar::new(PathBuf::from(".")),
                &store,
                &defaults(),
                &mut warnings,
            )
            .expect("the task renders");
            match prepared {
                Prepared::Remote(_, _, _, payload, kind) => (payload, kind),
                _ => panic!("{module} is a remote task"),
            }
        };
        let (built, kind) = payload_of("lineinfile");
        let built = built.expect("the run built one for it");
        assert_eq!(built.blob, "ab");
        assert_eq!(built.facts.module_fqn, "ansible.modules.lineinfile");
        assert_eq!(kind, None);
        assert_eq!(
            payload_of("ansible.posix.sysctl")
                .0
                .expect("the run built one for it")
                .facts
                .module_fqn,
            "ansible_collections.ansible.posix.plugins.modules.sysctl"
        );
        assert!(
            payload_of("command").0.is_none(),
            "a module this release runs itself travels without a payload"
        );
        // A plugin's sub-tasks carry the payloads, so the task itself carries none: it says
        // which plugin runs it, and that is all.
        assert_eq!(
            payload_of("ansible.builtin.package"),
            (None, Some(Kind::Package))
        );
    }

    /// The task goes out with `force_python` wherever an agent's native module must not answer
    /// for it: natives switched off for the run, a module that is not ansible-core's own
    /// (measured, a `library/stat.py` is built as `ansible.legacy.stat`; `core` is the second
    /// barrier), and ansible-core's `setup`, whatever name the task wrote, unless `--facts
    /// native` asked for the native collector.
    ///
    /// What would make this red: the flag never set, or set on the payload and dropped on the way
    /// to the wire.
    #[test]
    fn a_task_is_sent_to_python_where_a_native_must_not_answer() {
        use crate::python::{Facts, ModuleFacts, Natives};
        let facts = |name: &str, core: bool| ModuleFacts {
            module_fqn: format!("ansible.modules.{name}"),
            profile: "legacy".into(),
            rlimit_nofile: 0,
            extensions: Map::new(),
            core,
        };
        let modules = BTreeMap::from([
            ("stat".to_string(), facts("stat", true)),
            ("setup".to_string(), facts("setup", true)),
            // The module a `library/lineinfile.py` builds.
            ("lineinfile".to_string(), facts("lineinfile", false)),
            // A collection's name `runtime.yml` redirects to `ansible.builtin.setup`: the helper
            // builds ansible-core's own file, under its own name.
            ("my.coll.facts".to_string(), facts("setup", true)),
        ]);
        let forced = |natives: Natives, module: &str| -> bool {
            let mut plan = plan();
            plan.python = Some(Arc::new(crate::python::Union {
                hash: "ab".into(),
                zip_b64: "UEsDBA==".into(),
                modules: modules.clone(),
                refused: BTreeMap::new(),
                natives,
            }));
            let step = step_of(task(module), Origin::default());
            let prepared = prepare(
                &step,
                "h1",
                &plan,
                &Progress::default(),
                &Templar::new(PathBuf::from(".")),
                &store_at(Path::new(".")),
                &defaults(),
                &mut Vec::new(),
            )
            .expect("the task renders");
            let Prepared::Remote(items, _, _, Some(payload), _) = prepared else {
                panic!("{module} is a python task");
            };
            super::super::run::protocol_task(
                &step.task,
                &items[0],
                Some((&payload, "/usr/bin/python3")),
            )
            .force_python
        };
        let on = Natives {
            enabled: true,
            facts: Facts::Auto,
        };
        let off = Natives {
            enabled: false,
            ..on
        };
        assert_eq!(
            Natives::default(),
            off,
            "a policy nobody set is natives off"
        );
        assert!(!forced(on, "stat"));
        assert!(!forced(on, "ansible.builtin.stat"));
        assert!(forced(off, "stat"), "native_modules = false");
        assert!(
            forced(on, "lineinfile"),
            "a module that is not ansible-core's own"
        );
        assert!(forced(on, "ansible.builtin.setup"), "--facts auto");
        assert!(forced(on, "my.coll.facts"), "setup under another name");
        let python = Natives {
            facts: Facts::Python,
            ..on
        };
        assert!(forced(python, "setup"), "--facts python");
        let native = Natives {
            facts: Facts::Native,
            ..on
        };
        assert!(!forced(native, "ansible.builtin.setup"), "--facts native");
        assert!(forced(
            Natives {
                enabled: false,
                ..native
            },
            "setup"
        ));
    }

    fn store_at(playbook_dir: &Path) -> Mutex<VarStore> {
        let inventory = crate::inventory::Inventory::parse_ini("h1\n").expect("an inventory");
        Mutex::new(VarStore::new(&inventory, None, playbook_dir, Map::new()).expect("a var store"))
    }

    fn step_of(task: PlayTask, origin: Origin) -> Step {
        Step {
            kind: crate::compile::StepKind::Task,
            task,
            block: None,
            section: crate::compile::Section::Body,
            role: None,
            origin: Arc::new(origin),
            include_params: None,
            hosts: None,
        }
    }

    fn prepared(step: &Step, store: &Mutex<VarStore>) -> Result<Vec<Item>, TemplateError> {
        let prepared = prepare(
            step,
            "h1",
            &plan(),
            &Progress::default(),
            &Templar::new(PathBuf::from(".")),
            store,
            &defaults(),
            &mut Vec::new(),
        )?;
        Ok(match prepared {
            Prepared::Skipped(items)
            | Prepared::Local(items, _)
            | Prepared::Remote(items, _, _, _, _) => items,
        })
    }

    fn failing(ignore_errors: &str) -> Step {
        let mut t = task("command");
        t.args.insert("_raw_params".into(), json!("false"));
        t.ignore_errors = Some(Flag::Template(ignore_errors.into()));
        step_of(t, Origin::default())
    }

    /// `assert`'s `that` is left to the conditional, which reads it as the task wrote it: a
    /// render error in it is the conditional's to report, as the reference's `finalize_task_arg`
    /// for `assert` lets it be. The other arguments still render.
    ///
    /// What would make this red: `that` rendered with the other arguments, which fails the task
    /// with the render's message before the assert reads the text.
    #[test]
    fn prepare_leaves_an_assert_s_that_to_the_conditional() {
        let store = store_at(Path::new("."));
        let mut t = task("assert");
        t.args.insert("that".into(), json!("{{ foo.bar == 1 }}"));
        t.args.insert("fail_msg".into(), json!("{{ 1 + 1 }}"));
        let items = prepared(&step_of(t, Origin::default()), &store)
            .expect("the render error is left to the conditional");
        assert!(!items[0].args.contains_key("that"), "{:?}", items[0].args);
        assert_eq!(items[0].args["fail_msg"], json!(2));
    }

    /// Each item's verdict, as `prepare` rendered it.
    fn verdicts(step: &Step, store: &Mutex<VarStore>) -> Vec<Option<bool>> {
        prepared(step, store)
            .unwrap()
            .iter()
            .map(|i| i.ignore_errors)
            .collect()
    }

    /// `ignore_errors` written as a template is rendered for each item that runs, and read the
    /// way ansible-core's `boolean(strict=True)` reads a keyword. Measured on ansible-core
    /// 2.19.12: `ignore_errors: "{{ ansible_check_mode }}"` on a failing `command` fails the task
    /// outside `--check`, and a render that is not a boolean fails it with `Error processing
    /// keyword 'ignore_errors': The value ... could not be converted to 'bool'.`
    ///
    /// What would make this red: the render in `prepare` removed, which leaves every template
    /// reading as not ignoring, `"{{ true }}"` included; a reader narrower than the reference's,
    /// which refuses `-e lenient=1`; or a render that is not a boolean read as `false` rather
    /// than refused.
    #[test]
    fn a_templated_ignore_errors_is_rendered_when_the_task_runs() {
        let store = store_at(Path::new("."));
        for (template, want) in [
            ("{{ ansible_check_mode }}", false),
            ("{{ true }}", true),
            ("{{ 1 }}", true),
            ("{{ 0.0 }}", false),
            ("{{ ' Y ' }}", true),
            ("{{ 'Off' }}", false),
            ("{{ 't' }}", true),
        ] {
            assert_eq!(
                verdicts(&failing(template), &store),
                [Some(want)],
                "{template}"
            );
        }
        for (template, shown) in [("{{ 'maybe' }}", "'maybe'"), ("{{ 2 }}", "2")] {
            let Err(err) = prepared(&failing(template), &store) else {
                panic!("{template} is not a boolean");
            };
            assert!(
                err.0.contains(&format!(
                    "Error processing keyword 'ignore_errors': The value {shown} could not be converted to 'bool'."
                )),
                "{}",
                err.0
            );
        }

        // A task a `when` left out never renders it, so a template it could not render is not
        // its problem; a task that wrote no template carries no verdict of its own.
        let mut step = failing("{{ nosuch }}");
        step.task.when = vec!["false".into()];
        assert!(prepared(&step, &store).is_ok());
        let step = step_of(task("command"), Origin::default());
        assert_eq!(verdicts(&step, &store), [None]);
    }

    /// Each loop item renders its own verdict against its own variables, so items can disagree,
    /// and an outer `item` an include handed down does not decide for the inner loop's items. An
    /// item a `when` left out renders nothing.
    ///
    /// What would make this red: the verdict rendered once against the task's variables, which
    /// fails on `item` as undefined, or reads the outer one for every item.
    #[test]
    fn each_loop_item_renders_its_own_ignore_errors() {
        let store = store_at(Path::new("."));
        let mut step = failing("{{ item }}");
        step.task.loop_items = Some(json!([true, false, "yes"]));
        assert_eq!(
            verdicts(&step, &store),
            [Some(true), Some(false), Some(true)]
        );

        let mut values = Map::new();
        values.insert("item".into(), json!(false));
        step.include_params = Some(Arc::new(IncludeParams {
            values,
            untrusted: BTreeSet::new(),
        }));
        step.task.loop_items = Some(json!([true]));
        assert_eq!(verdicts(&step, &store), [Some(true)]);

        let mut step = failing("{{ true }}");
        step.task.loop_items = Some(json!([1, 2]));
        step.task.when = vec!["item == 1".into()];
        assert_eq!(verdicts(&step, &store), [Some(true), None]);
    }

    /// A step an include brought in goes through the same `prepare`, so its templated
    /// `ignore_errors` is rendered too, and against what the include handed down.
    ///
    /// What would make this red: the render tied to steps compiled with the play rather than
    /// to every step `prepare` is given.
    #[test]
    fn a_templated_ignore_errors_in_an_included_file_is_rendered_too() {
        let store = store_at(Path::new("."));
        let mut step = failing("{{ lenient }}");
        step.origin = Arc::new(Origin {
            depth: 1,
            ..Origin::default()
        });
        let mut values = Map::new();
        values.insert("lenient".into(), json!(true));
        step.include_params = Some(Arc::new(IncludeParams {
            values,
            untrusted: BTreeSet::new(),
        }));
        assert_eq!(verdicts(&step, &store), [Some(true)]);
    }

    /// An undefined variable in `loop:` or `with_*` waits for the task's `when`, the way
    /// ansible-core 2.19.12's `TaskExecutor` keeps it (`_loop_eval_error`) and raises it only
    /// once the conditional, evaluated without an `item`, says the task runs. A false `when`
    /// skips the task with the ordinary skip result; a true one, none at all, or one that
    /// cannot be evaluated fails it with the loop's own error. Any other loop error is not kept.
    ///
    /// What would make this red: the loop rendered with `?` before any `when` is read, which
    /// fails the skipped cases; the kept error dropped once the `when` holds, which runs the task
    /// with no items; or the conditional's own error raised in place of the loop's.
    #[test]
    fn an_undefined_loop_waits_for_the_when() {
        let store = store_at(Path::new("."));
        let loops = [
            (json!("{{ nope }}"), false, None),
            (json!("{{ nope }}"), true, None),
            (json!(["{{ nope }}"]), false, Some("fileglob")),
            (json!("{{ nope }}"), false, Some("first_found")),
        ];
        for (raw, with_items, with) in loops {
            let mut t = task("debug");
            t.loop_items = Some(raw.clone());
            t.with_items = with_items;
            t.loop_with = with.map(str::to_string);
            t.when = vec!["true".into(), "nope is defined".into()];
            let items = prepared(&step_of(t.clone(), Origin::default()), &store)
                .unwrap_or_else(|e| panic!("{raw} is skipped: {}", e.0));
            assert_eq!(items.len(), 1, "{raw}: the task, not an item");
            assert_eq!(items[0].element, None);
            let skipped = items[0].skipped.as_ref().expect("skipped").0.clone();
            assert_eq!(
                Value::Object(skipped),
                json!({
                    "changed": false,
                    "skipped": true,
                    "skip_reason": "Conditional result was False",
                    "false_condition": "nope is defined",
                }),
                "{raw}"
            );

            for when in [vec![], vec!["true".to_string()], vec!["(".to_string()]] {
                t.when = when.clone();
                let Err(err) = prepared(&step_of(t.clone(), Origin::default()), &store) else {
                    panic!("{raw} under {when:?} fails");
                };
                assert!(err.is_undefined(), "{raw} under {when:?}: {}", err.0);
            }
        }

        // A loop that renders is untouched, and an error other than an undefined variable
        // fails the task whatever its `when` says.
        let mut t = task("debug");
        t.loop_items = Some(json!([1, 2]));
        t.when = vec!["false".into()];
        let items = prepared(&step_of(t.clone(), Origin::default()), &store).unwrap();
        assert_eq!(items.len(), 2);
        t.loop_items = Some(json!("{{ ( }}"));
        let Err(err) = prepared(&step_of(t, Origin::default()), &store) else {
            panic!("a syntax error in the loop fails the task");
        };
        assert!(!err.is_undefined(), "{}", err.0);
    }

    /// `role_path` and `ansible_search_path` in a role's task and in a playbook's. Measured on
    /// ansible-core 2.19.12 (sanitised): `role_path=~/.../roles/probe | search=['~/.../roles/probe',
    /// '~/.../roles/probe/tasks', '~/...']`, and outside a role the path is the playbook's
    /// directory alone with `role_path` undefined.
    ///
    /// What would make this red: the playbook's task listing its directory twice; `role_path`
    /// defined outside a role; or either inserted as data, which leaves a host fact of the same
    /// name in charge of where the lookups look.
    #[test]
    fn a_role_s_task_carries_its_role_path_and_search_path() {
        let play = PathBuf::from("/srv/play");
        let role = play.join("roles/probe");
        let store = store_at(&play);
        store.lock().unwrap().set_untrusted_fact(
            "h1",
            "ansible_search_path",
            json!(["/from/a/host"]),
        );
        let in_role = step_of(
            task("command"),
            Origin {
                file_dir: role.join("tasks"),
                role_dir: Some(role.clone()),
                depth: 0,
                inherited: None,
            },
        );
        let vars = &prepared(&in_role, &store).unwrap()[0].vars;
        assert_eq!(vars.map["role_path"], json!("/srv/play/roles/probe"));
        assert_eq!(
            vars.map["ansible_search_path"],
            json!([
                "/srv/play/roles/probe",
                "/srv/play/roles/probe/tasks",
                "/srv/play"
            ])
        );
        assert!(!vars.untrusted.contains("ansible_search_path"));
        assert!(!vars.untrusted.contains("role_path"));

        let in_play = step_of(
            task("command"),
            Origin {
                file_dir: play.clone(),
                role_dir: None,
                depth: 0,
                inherited: None,
            },
        );
        let vars = &prepared(&in_play, &store).unwrap()[0].vars;
        assert_eq!(vars.map["ansible_search_path"], json!(["/srv/play"]));
        assert!(!vars.map.contains_key("role_path"));
    }

    /// A task's own `vars:` built from a lookup that needs the role's search path to find its
    /// file. Measured against ansible-core 2.19.12 (via `template/lookups.rs`): once
    /// `ansible_search_path` names the role's `templates/`, `vars: {via_lookup: "{{
    /// lookup('template', 't.j2') }}"}` resolves to the lookup's result, and the name stays
    /// data - a later, one-pass read of it (what `debug: var:` compiles to, `Templar::evaluate`)
    /// must show that result, never the raw call.
    ///
    /// What would make this red: the search path inserted after this task's own `vars:` are
    /// resolved, which leaves the role's `templates/t.j2` unreachable on that pass (the lookup
    /// falls back to the playbook directory alone, where the file is not) and the raw,
    /// unevaluated call stored where a later, one-pass read can no longer template it.
    #[test]
    fn a_tasks_own_vars_resolve_with_the_role_s_search_path_already_in_place() {
        let play =
            std::env::temp_dir().join(format!("volant-prepare-searchpath-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&play);
        let role = play.join("roles/probe");
        std::fs::create_dir_all(role.join("templates")).expect("role dir");
        std::fs::write(role.join("templates/t.j2"), "{{ '{{ 1 + 1 }}' }}").expect("template file");

        let store = store_at(&play);
        let mut t = task("command");
        t.vars.insert(
            "via_lookup".into(),
            json!("{{ lookup('template', 't.j2') }}"),
        );
        let step = step_of(
            t,
            Origin {
                file_dir: role.join("tasks"),
                role_dir: Some(role.clone()),
                depth: 0,
                inherited: None,
            },
        );
        let templar = Templar::new(play.clone());
        let prepared = prepare(
            &step,
            "h1",
            &plan(),
            &Progress::default(),
            &templar,
            &store,
            &defaults(),
            &mut Vec::new(),
        )
        .expect("the task renders");
        let items = match prepared {
            Prepared::Skipped(items)
            | Prepared::Local(items, _)
            | Prepared::Remote(items, _, _, _, _) => items,
        };
        let vars = &items[0].vars;
        assert_eq!(vars.map["via_lookup"], json!("{{ 1 + 1 }}"));
        assert!(
            vars.untrusted.contains("via_lookup"),
            "{:?}",
            vars.untrusted
        );

        let _ = std::fs::remove_dir_all(&play);
    }

    fn plan() -> PlayPlan {
        PlayPlan {
            plan: watch::channel(Arc::new(Compiled::default())).1,
            force_handlers: false,
            play_vars: Map::new(),
            vars_files: HashMap::new(),
            all_play_hosts: Vec::new(),
            r#become: None,
            become_user: None,
            python: None,
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
            become_for(&t, &p, &hvars(host), &defaults(), &templar).unwrap()
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
            become_for(&t, &p, &HostVars::default(), &defaults(), &templar)
                .unwrap()
                .unwrap()
                .user,
            "play"
        );
        t.become_user = Some("task".into());
        assert_eq!(
            become_for(&t, &p, &HostVars::default(), &defaults(), &templar)
                .unwrap()
                .unwrap()
                .user,
            "task"
        );
        let host = hvars(json!({"ansible_become_user": "host"}));
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
            become_for(&bare, &plan(), &HostVars::default(), &d, &templar)
                .unwrap()
                .unwrap()
                .user,
            "root",
            "nothing said anywhere means root, as in Ansible"
        );
        let templated = hvars(json!({"ansible_become_user": "{{ who }}", "who": "deploy"}));
        assert_eq!(
            become_for(&t, &p, &templated, &defaults(), &templar)
                .unwrap()
                .unwrap()
                .user,
            "deploy",
            "a templated user is resolved before the link is keyed by it"
        );
    }

    /// `become_user: "{{ item }}"` escalates per item in the reference, measured on the
    /// development machine: two items, `root` then the invoking account, and each ran as its
    /// own. Here it fails the task, because escalation is settled once per batch and one batch
    /// is one message to one agent under one user. What this pins is the message: it names the
    /// keyword and says why, where it used to say only that some option held an undefined
    /// variable.
    #[test]
    fn a_become_user_that_changes_per_loop_item_says_why_it_cannot() {
        let templar = Templar::new(std::env::temp_dir());
        let mut t = task("command");
        t.r#become = Some(true);
        t.become_user = Some("{{ item }}".into());
        t.loop_items = Some(json!(["root", "deploy"]));
        let err = become_for(&t, &plan(), &HostVars::default(), &defaults(), &templar).unwrap_err();
        assert!(
            err.0.contains("become_user") && err.0.contains("per loop item"),
            "{}",
            err.0
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
            &hvars(json!({"ansible_become_method": "su"})),
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
                &hvars(json!({"ansible_become_method": "su"})),
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
        let err = become_for(&t, &plan(), &HostVars::default(), &d, &templar).unwrap_err();
        assert!(
            err.0.contains("doas") && err.0.contains("not supported"),
            "the defaults are refused for an escalating task the startup pass could not see: {}",
            err.0
        );
        assert_eq!(
            become_for(
                &task("command"),
                &plan(),
                &HostVars::default(),
                &d,
                &templar
            )
            .unwrap(),
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

    /// A delegate is the inventory's own host when it has one, the local host for the three
    /// implicit spellings, and an ssh target of that name otherwise -- and its variables come
    /// back merged the way any host's do, so its connection obeys the same precedence.
    ///
    /// Measured on ansible-core 2.19.12: `delegate_to: localhost` with no `localhost` in the
    /// inventory runs locally, and `delegate_to: nosuch` is connected to rather than refused -
    /// it reports ssh's own resolution failure as `UNREACHABLE`.
    ///
    /// What would make this red: an unknown name refused, which refuses a playbook the
    /// reference runs; or `localhost` turned into an ssh target, which tries to connect to the
    /// machine the controller is already on.
    #[test]
    fn a_delegate_is_the_inventory_s_host_or_an_implicit_one() {
        let inventory = crate::inventory::Inventory::parse_ini(
            "h3 ansible_host=10.0.0.3
",
        )
        .expect("an inventory");
        let store = Mutex::new(
            VarStore::new(&inventory, None, Path::new("."), Map::new()).expect("a var store"),
        );
        let templar = Templar::new(std::env::temp_dir());
        let live = Progress::default();
        let plan = plan();
        let of = |name: &str| {
            host_vars(
                name,
                &plan,
                &Map::new(),
                None,
                None,
                &live,
                &templar,
                &store,
                &Origin::default(),
                Path::new("."),
            )
            .map
        };

        assert_eq!(of("h3").get("ansible_host"), Some(&json!("10.0.0.3")));

        for name in ["localhost", "127.0.0.1", "::1"] {
            assert_eq!(
                of(name).get("ansible_connection"),
                Some(&json!("local")),
                "{name}"
            );
        }

        let stranger = of("nosuch");
        assert_eq!(stranger.get("ansible_connection"), None);
        assert_eq!(
            stranger.get("ansible_host"),
            None,
            "an unknown name is an ssh target of that name, not a refusal"
        );
    }

    /// A directory of fixture files, removed when the test ends however it ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("volant-with-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("the scratch directory");
            // Canonical, so a path the lookup prints compares equal on a machine whose temporary
            // directory is a symlink.
            Scratch(dir.canonicalize().expect("the scratch directory resolves"))
        }

        fn write(&self, rel: &str) -> String {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("the parent");
            std::fs::write(&path, "x").expect("the fixture file");
            path.display().to_string()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The one task of `yaml`, read by the loader, as a step written in `origin`.
    fn loaded(yaml: &str, origin: Origin) -> Step {
        let pb = crate::playbook::parse(yaml, "x.yml").expect("the playbook loads");
        let Some(crate::playbook::TaskOrBlock::Task(t)) = pb.plays[0].tasks.first() else {
            panic!("one task");
        };
        step_of(t.clone(), origin)
    }

    /// What each item binds the loop variable to, and whether it binds it as data.
    fn bound(step: &Step, store: &Mutex<VarStore>) -> Result<Vec<(Value, bool)>, TemplateError> {
        Ok(prepared(step, store)?
            .iter()
            .map(|item| {
                let var = &step.task.loop_var;
                (
                    item.vars.map[var].clone(),
                    item.vars.untrusted.contains(var),
                )
            })
            .collect())
    }

    /// The `airgap` role's `Distribute K3s binary` task, as `site.yml` lists it statically:
    /// `copy` over `with_first_found` with one mapping term naming two absolute candidates and
    /// `skip: true`. The first candidate that exists is the one item; with neither there, `skip`
    /// makes it a loop over nothing, which reports the task skipped; without `skip` it fails in
    /// the plugin's words, `No file was found when using first_found.`
    ///
    /// What would make this red: the terms handed to the lookup as one string rather than as its
    /// terms, the lookup's bare-string answer walked as it stands (no item, or a failure on a
    /// loop that is not a list), `skip` read as a failure, or the order of the candidates lost.
    #[test]
    fn with_first_found_walks_the_file_it_found_and_nothing_under_skip() {
        let dir = Scratch::new("first-found");
        let task = |skip: &str| {
            format!(
                "- hosts: all\n  tasks:\n    - name: Distribute K3s binary\n      copy:\n        src: \"{{{{ item }}}}\"\n        dest: /usr/local/bin/k3s\n      with_first_found:\n        - files:\n            - \"{{{{ airgap_dir }}}}/k3s-{{{{ k3s_arch }}}}\"\n            - \"{{{{ airgap_dir }}}}/k3s\"\n{skip}      vars:\n        airgap_dir: {}\n        k3s_arch: arm64\n",
                dir.0.display()
            )
        };
        let store = store_at(&dir.0);
        let step = loaded(&task("          skip: true\n"), Origin::default());
        assert_eq!(step.task.loop_with.as_deref(), Some("first_found"));

        assert_eq!(bound(&step, &store).unwrap(), []);
        let plain = dir.write("k3s");
        assert_eq!(bound(&step, &store).unwrap(), [(json!(plain), false)]);
        let arch = dir.write("k3s-arm64");
        assert_eq!(bound(&step, &store).unwrap(), [(json!(arch), false)]);

        std::fs::remove_file(&arch).expect("the file goes");
        std::fs::remove_file(&plain).expect("the file goes");
        let strict = loaded(&task(""), Origin::default());
        let err = bound(&strict, &store).expect_err("nothing found and no skip");
        assert!(
            err.0.contains("No file was found when using first_found."),
            "{}",
            err.0
        );
    }

    /// The `airgap` role's two static `with_fileglob` tasks: a pattern under `airgap_dir` walks
    /// every file it matches, sorted, and one match is still a loop of one, not the path's bare
    /// string. A term that renders undefined fails here, where `first_found` would drop it.
    ///
    /// What would make this red: the lookup's single-match answer, a bare string, walked as it
    /// stands; no match read as one empty item; or the undefined term dropped for every lookup
    /// rather than for `first_found` alone.
    #[test]
    fn with_fileglob_walks_every_match_and_one_match_is_a_list_of_one() {
        let dir = Scratch::new("fileglob");
        let task = |pattern: &str| {
            format!(
                "- hosts: all\n  tasks:\n    - name: Distribute K3s images\n      copy:\n        src: \"{{{{ item }}}}\"\n        dest: /var/lib/rancher/k3s/agent/images/\n      with_fileglob:\n        - \"{pattern}\"\n      vars:\n        airgap_dir: {}\n",
                dir.0.display()
            )
        };
        let store = store_at(&dir.0);
        let step = loaded(&task("{{ airgap_dir }}/*.tar.gz"), Origin::default());
        assert_eq!(step.task.loop_with.as_deref(), Some("fileglob"));
        assert_eq!(bound(&step, &store).unwrap(), []);

        let one = dir.write("images-2.tar.gz");
        assert_eq!(bound(&step, &store).unwrap(), [(json!(one), false)]);
        let first = dir.write("images-1.tar.gz");
        dir.write("images.txt");
        assert_eq!(
            bound(&step, &store).unwrap(),
            [(json!(first), false), (json!(one), false)]
        );

        let undefined = loaded(&task("{{ nosuch }}/*.rpm"), Origin::default());
        let err = bound(&undefined, &store).expect_err("an undefined term fails fileglob");
        assert!(err.is_undefined(), "{}", err.0);
    }

    /// The `raspberrypi` role's `include_tasks` over `with_first_found`, which `site.yml` lists
    /// statically and runs on every host. On a host that is not a Pi, `detected_distribution` was
    /// never set, so the terms that name it render undefined and are dropped - the plugin's own
    /// rule when it is invoked as `with_` - and the search goes on to the ones that render. The
    /// file is found beside the task, in the role's `tasks/`, and the task's `when` then skips
    /// each item.
    ///
    /// A term built from a fact is the host's choice of file, so the items are bound as data
    /// then, and as the playbook's own when only author terms were read.
    ///
    /// What would make this red: an undefined term failing the task on every host that is not a
    /// Pi, the `when` evaluated before the loop, or an item named by a fact bound as author
    /// content.
    #[test]
    fn an_include_over_with_first_found_skips_undefined_terms_and_keeps_the_host_s_choice_as_data()
    {
        let dir = Scratch::new("raspberrypi");
        let role = dir.0.join("roles/raspberrypi");
        let default = dir.write("roles/raspberrypi/tasks/setup/default.yml");
        let ubuntu = dir.write("roles/raspberrypi/tasks/setup/Ubuntu.yml");
        let yaml = "- hosts: all\n  tasks:\n    - name: Execute OS related tasks on the Raspberry Pi - {{ action_ }}\n      include_tasks: \"{{ item }}\"\n      with_first_found:\n        - \"{{ action_ }}/{{ detected_distribution }}-{{ detected_distribution_major_version }}.yml\"\n        - \"{{ action_ }}/{{ detected_distribution }}.yml\"\n        - \"{{ action_ }}/{{ ansible_distribution }}-{{ ansible_distribution_major_version }}.yml\"\n        - \"{{ action_ }}/{{ ansible_distribution }}.yml\"\n        - \"{{ action_ }}/default.yml\"\n      vars:\n        state: present\n        action_: >-\n          {% if state == 'present' %}setup{% else %}teardown{% endif %}\n      when:\n        - raspberry_pi | default(false)\n";
        let step = loaded(
            yaml,
            Origin {
                file_dir: role.join("tasks"),
                role_dir: Some(role.clone()),
                depth: 0,
                inherited: None,
            },
        );
        let store = store_at(&dir.0);
        let items = prepared(&step, &store).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].vars.map["item"], json!(default));
        assert!(!items[0].vars.untrusted.contains("item"));
        assert!(items[0].skipped.is_some(), "the task's `when` skips it");

        store
            .lock()
            .unwrap()
            .set_untrusted_fact("h1", "ansible_distribution", json!("Ubuntu"));
        store.lock().unwrap().set_untrusted_fact(
            "h1",
            "ansible_distribution_major_version",
            json!("24"),
        );
        assert_eq!(bound(&step, &store).unwrap(), [(json!(ubuntu), true)]);
    }

    /// `with_first_found` searches the subdirectory the task's action names in each entry before
    /// the entry itself, as ansible-core 2.19.12's `first_found.py` does when it runs for a
    /// `with_` loop: `templates/` for a `template`, `vars/` for an `include_vars`, `files/`
    /// otherwise. A plain `lookup('first_found')` does not, which `template/lookups.rs` measured.
    ///
    /// What would make this red: the subdirectory left out, which finds neither file; the wrong
    /// one chosen for the action; or `skip: true` failing when `vars/` holds nothing.
    #[test]
    fn with_first_found_searches_the_directory_its_action_names() {
        let dir = Scratch::new("first-found-subdir");
        let role = dir.0.join("roles/probe");
        let in_role = || Origin {
            file_dir: role.join("tasks"),
            role_dir: Some(role.clone()),
            depth: 0,
            inherited: None,
        };
        let store = store_at(&dir.0);
        let template = dir.write("roles/probe/templates/t.j2");
        let step = loaded(
            "- hosts: all\n  tasks:\n    - template:\n        src: \"{{ item }}\"\n        dest: /tmp/t\n      with_first_found:\n        - t.j2\n",
            in_role(),
        );
        assert_eq!(bound(&step, &store).unwrap(), [(json!(template), false)]);

        let vars = loaded(
            "- hosts: all\n  tasks:\n    - include_vars: \"{{ item }}\"\n      with_first_found:\n        - files:\n            - a.yml\n          skip: true\n",
            in_role(),
        );
        assert_eq!(bound(&vars, &store).unwrap(), []);
        let found = dir.write("roles/probe/vars/a.yml");
        assert_eq!(bound(&vars, &store).unwrap(), [(json!(found), false)]);
    }

    /// A file whose name is a template, matched by a glob under the playbook's own directory, is
    /// refused by name before anything renders it. Its path would otherwise be bound as author
    /// content and rendered again by `src: "{{ item }}"`, running the command in its name on the
    /// controller. The reference never templates a lookup's result and copies the file as it is.
    /// Both ways of looping over the lookup go through the same refusal.
    ///
    /// What would make this red: the refusal gone, which creates the marker file the name's
    /// `pipe` touches; or it placed in the `with_` path alone, which leaves `loop:` open.
    #[test]
    fn a_found_path_that_looks_like_a_template_is_refused_and_never_rendered() {
        let dir = Scratch::new("found-template");
        let marker = PathBuf::from(format!("{}.marker", dir.0.display()));
        let _ = std::fs::remove_file(&marker);
        let named = dir.write("{{ lookup('pipe', 'touch ' ~ airgap_dir ~ '.marker') }}.tar.gz");
        let store = store_at(&dir.0);
        for looping in [
            "with_fileglob:\n        - \"{{ airgap_dir }}/*.tar.gz\"\n",
            "loop: \"{{ [lookup('fileglob', airgap_dir ~ '/*.tar.gz')] }}\"\n",
        ] {
            let step = loaded(
                &format!(
                    "- hosts: all\n  tasks:\n    - copy:\n        src: \"{{{{ item }}}}\"\n        dest: /tmp/images/\n      {looping}      vars:\n        airgap_dir: {}\n",
                    dir.0.display()
                ),
                Origin::default(),
            );
            let Err(err) = prepared(&step, &store) else {
                panic!("{looping}: the path is refused");
            };
            assert!(
                err.0.contains(&named) && err.0.contains("template marker"),
                "{looping}: {}",
                err.0
            );
            assert!(!marker.exists(), "{looping}: the name's command ran");
        }
        let _ = std::fs::remove_file(&marker);
    }
}
