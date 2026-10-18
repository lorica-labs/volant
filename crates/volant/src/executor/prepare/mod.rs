// SPDX-License-Identifier: GPL-3.0-or-later
//! Rendering one task for one host: variables, escalation, environment and loop items.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;

use crate::compile::Step;
use crate::playbook::{Flag, python_repr};
use crate::template::{Templar, TemplateError};
use crate::transport::ConnectionDefaults;
use crate::vars::{HostVars, VarStore};

use super::coordinator::Progress;

mod connect;
mod escalation;
mod item;
mod loops;
mod plan;
mod vars;

pub(in crate::executor) use escalation::retry_name;
pub(in crate::executor) use item::{
    FINALIZING, Item, NOTIFY_FAILED, Prepared, failure_text, pause_of,
};
pub(in crate::executor) use plan::{PlayPlan, Strategy};
pub(in crate::executor) use vars::host_vars;

use connect::{connect_for, delegate_for};
use escalation::{become_for, remove_omit};
use item::{check_gather_subset, environment_for, flag_for, ignore_errors_for, notify_for};
use loops::{LoopOptions, loop_list, loop_options, with_lookup_items};

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
