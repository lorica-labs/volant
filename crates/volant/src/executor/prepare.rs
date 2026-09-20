// SPDX-License-Identifier: GPL-3.0-or-later
//! Rendering one task for one host: variables, escalation, environment and loop items.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};
use tokio::sync::watch;
use volant_protocol::TaskResult;

use crate::compile::{Compiled, IncludeParams, Step};
use crate::inventory::Host;
use crate::playbook::PlayTask;
use crate::render::ansible_json;
use crate::template::{Templar, TemplateError, Vars};
use crate::transport::{ConnectionDefaults, Escalation};
use crate::vars::{HostVars, Scope, VarStore, omit_token};

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
    /// Every host the inventory resolved, by name, for `delegate_to` to look one up in. The
    /// whole inventory rather than the play's own hosts: measured on ansible-core 2.19.12, a
    /// play over h1 and h2 delegates to h3 without h3 being in the play at all, and h3 stays
    /// out of the recap.
    pub(super) inventory: Arc<HashMap<String, Host>>,
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
    let on = vars
        .get("ansible_become")
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
    match vars.get("ansible_become_method").and_then(Value::as_str) {
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
    let user = match vars.get("ansible_become_user").and_then(Value::as_str) {
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
    let password = vars
        .get("ansible_become_password")
        .or_else(|| vars.get("ansible_become_pass"))
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
    let (raw, hostvars, shared, untrusted, untrusted_hosts) = {
        let mut store = store.lock().expect("vars lock");
        // The shared views first: a variable of this host's own can name `hostvars` or `groups`,
        // and `resolve_vars` below has to be able to answer it.
        let hostvars = store.hostvars_shared(host);
        let shared = store.shared_values(&scope);
        let mut untrusted = store.untrusted_of(host);
        // What an include handed down is already rendered, so its provenance cannot be read off
        // the store: it travelled with the values.
        untrusted.extend(
            include_params
                .iter()
                .flat_map(|p| p.untrusted.iter().cloned()),
        );
        let untrusted_hosts = store.untrusted_hosts();
        (
            store.for_host(host, &scope),
            hostvars,
            shared,
            untrusted,
            untrusted_hosts,
        )
    };
    // The merged map carries this host's facts, so the names that came from a managed host
    // travel into the resolution below and are left there exactly as they arrived. The set comes
    // back wider than it went in: a `vars:` of the play, of a block or of the task itself built
    // from a registered value is data too, and this resolution is the only place that can tell.
    let (map, untrusted) = templar.resolve_vars_tainted(Vars {
        map: &raw,
        hostvars: Some(&hostvars),
        shared: Some(&shared),
        untrusted: Some(&untrusted),
        untrusted_hosts: Some(&untrusted_hosts),
    });
    HostVars {
        map,
        hostvars,
        shared,
        untrusted,
        untrusted_hosts,
    }
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
    /// connection is never opened for it.
    Remote(Vec<Item>, Option<Escalation>, Option<Host>),
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
}

/// One task's `environment`, layer by layer: the play's, then each block's, then the task's,
/// each rendered against this item's variables and merged over the ones before it.
///
/// Measured on ansible-core 2.19.12: a layer that does not render to a mapping warns on stderr
/// and is skipped while the task still runs, and the layers around it stay in force. Values are
/// what Python's `str()` makes of them - `42` is `"42"`, `true` is `"True"`, `null` is `"None"`.
fn environment_for(
    task: &PlayTask,
    vars: &HostVars,
    templar: &Templar,
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
            // Straight to stderr rather than through the renderer: `prepare` runs inside a host
            // driver, which has no renderer of its own, and every `[WARNING]` this engine prints
            // goes to stderr anyway.
            eprintln!(
                "[WARNING]: could not parse environment value, skipping: {}",
                ansible_json(&rendered)
            );
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

pub(super) fn prepare(
    step: &Step,
    host: &str,
    plan: &PlayPlan,
    live: &Progress,
    templar: &Templar,
    store: &Mutex<VarStore>,
    defaults: &ConnectionDefaults,
) -> Result<Prepared, TemplateError> {
    let task = &step.task;
    let base = host_vars(
        host,
        plan,
        &task.vars,
        step.role,
        step.include_params.as_deref(),
        live,
        templar,
        store,
    );
    // Whether the list this loop walks came from a managed host. A loop over a literal list the
    // playbook wrote binds author content; one over `{{ r.stdout_lines }}` binds data, and the
    // items of such a loop are never rendered again.
    let mut items_from_host = false;
    let elements: Vec<Option<Value>> = match &task.loop_items {
        None => vec![None],
        Some(raw) => {
            let (rendered, tainted) = templar.render_value_tainted(raw, &base)?;
            items_from_host = tainted;
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
        // Rendered argument by argument rather than as one value, because a few arguments are
        // read back as engine input rather than as data - the name a `debug: var:` compiles -
        // and the render is the only place that can say which of them came from a host.
        let (args, args_untrusted) = if skipped.is_some() {
            (Map::new(), BTreeSet::new())
        } else {
            let (map, untrusted) = templar.render_map_tainted(&task.args, &vars)?;
            let mut rendered = Value::Object(map);
            remove_omit(&mut rendered);
            let Value::Object(map) = rendered else {
                unreachable!("an object renders to an object")
            };
            (map, untrusted)
        };
        // A skipped item runs nothing, so a layer it could not render is not its problem: the
        // reference does not evaluate `environment` for a task a `when` left out.
        let environment = if skipped.is_some() {
            BTreeMap::new()
        } else {
            environment_for(task, &vars, templar)?
        };
        items.push(Item {
            element,
            label,
            args,
            args_untrusted,
            vars,
            environment,
            skipped,
        });
    }
    if items.iter().all(|i| i.skipped.is_some()) {
        return Ok(Prepared::Skipped(items));
    }
    let delegate = delegate_for(task, &base, templar)?.map(|name| delegate_host(&name, plan));
    if is_local(&task.module) {
        return Ok(Prepared::Local(items, delegate.map(|d| d.name)));
    }
    // From the delegating host's own variables, measured on ansible-core 2.19.12:
    // `ansible_become_user` under a `delegate_to` still reads the value the **delegating**
    // host carries, while `ansible_host` and `ansible_connection` read the delegate's. So the
    // link goes to the delegate and escalates to the user the task's own host asked for.
    let escalation = become_for(task, plan, &base, defaults, templar)?;
    Ok(Prepared::Remote(items, escalation, delegate))
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

/// The host a `delegate_to` names: the inventory's own entry when it has one, an implicit host
/// otherwise.
///
/// Measured on ansible-core 2.19.12: `delegate_to: localhost` with no `localhost` in the
/// inventory runs locally (`changed: [h1 -> localhost]`), `127.0.0.1` does the same, and a name
/// the inventory has never heard of is **connected to** rather than refused - `delegate_to:
/// nosuch` reports `fatal: [h1 -> nosuch]: UNREACHABLE!` with ssh's own resolution error and
/// exits 4. So an unknown name is an ssh target of that name, not a load-time error.
fn delegate_host(name: &str, plan: &PlayPlan) -> Host {
    if let Some(host) = plan.inventory.get(name) {
        return host.clone();
    }
    let mut vars = BTreeMap::new();
    if matches!(name, "localhost" | "127.0.0.1" | "::1") {
        vars.insert("ansible_connection".to_string(), json!("local"));
    }
    Host {
        name: name.to_string(),
        vars,
    }
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

    fn plan() -> PlayPlan {
        PlayPlan {
            plan: watch::channel(Arc::new(Compiled::default())).1,
            force_handlers: false,
            play_vars: Map::new(),
            vars_files: HashMap::new(),
            all_play_hosts: Vec::new(),
            inventory: Arc::new(HashMap::new()),
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
    /// implicit spellings, and an ssh target of that name otherwise.
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
        let mut plan = plan();
        let mut known = Host {
            name: "h3".into(),
            vars: BTreeMap::new(),
        };
        known.vars.insert("ansible_host".into(), json!("10.0.0.3"));
        plan.inventory = Arc::new(HashMap::from([("h3".to_string(), known)]));

        let found = delegate_host("h3", &plan);
        assert_eq!(found.vars.get("ansible_host"), Some(&json!("10.0.0.3")));

        for name in ["localhost", "127.0.0.1", "::1"] {
            let implicit = delegate_host(name, &plan);
            assert_eq!(implicit.name, name);
            assert_eq!(
                implicit.vars.get("ansible_connection"),
                Some(&json!("local")),
                "{name}"
            );
        }

        let stranger = delegate_host("nosuch", &plan);
        assert_eq!(stranger.name, "nosuch");
        assert!(
            stranger.vars.is_empty(),
            "an unknown name is an ssh target of that name, not a refusal"
        );
    }
}
