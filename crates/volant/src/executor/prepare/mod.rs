// SPDX-License-Identifier: GPL-3.0-or-later
//! Rendering one task for one host: variables, escalation, environment and loop items.

use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};
use volant_protocol::TaskResult;
use volant_protocol::modules::short_name;

use crate::compile::Step;
use crate::playbook::python_repr;
use crate::template::{ALLITEMS_KEY, Templar, TemplateError};
use crate::transport::ConnectionDefaults;
use crate::vars::VarStore;

use super::coordinator::Progress;

mod carry;
mod connect;
mod escalation;
mod fields;
mod item;
mod label;
mod late;
mod loops;
mod plan;
pub(in crate::executor) mod undefined;
mod vars;

pub(in crate::executor) use escalation::retry_name;
pub(in crate::executor) use item::strict_boolean;
pub(in crate::executor) use item::{
    FINALIZING, Item, NOTIFY_FAILED, Prepared, failed_result, failure_text, pause_of,
    undefined_name,
};
pub(in crate::executor) use label::{label_failed, label_raised_result, raised_by_label};
pub(in crate::executor) use late::{hide_after_failure, resolve, strategy_reads_as_true};
pub(in crate::executor) use plan::{Gate, PlayPlan, Strategy};
pub(in crate::executor) use vars::host_vars;
use vars::{Named, delegate_host_vars, merged_vars, resolved};

use carry::Carry;
use connect::{connect_for, delegate_for, unplugged};
use escalation::become_for;
use fields::{Fields, render_fields};
use item::{item_failure, notify_for};
use label::label_for;
pub(in crate::executor) use loops::undefined_causes;
use loops::{LoopOptions, loop_list, loop_options, named_undefined, with_lookup_items};

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
    // A looped `set_fact` hands what it sets to the items after it, so it keeps the variables
    // before they resolve each other: see `carry`.
    let carries = task.loop_items.is_some() && short_name(&task.module) == "set_fact";
    let (base, mut carry) = if carries {
        let merged = merged_vars(
            host,
            Named::ByTheRun,
            plan,
            &task.vars,
            step.role,
            step.include_params.as_deref(),
            live,
            store,
            &step.origin,
            &playbook_dir,
        );
        (resolved(merged.clone(), templar), Some(Carry::new(merged)))
    } else {
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
        (base, None)
    };
    // Every item of a loop starts from these variables. They are shared rather than copied
    // into each item, so that the memory of a loop does not grow with the size of what the host
    // holds, a registered result for one.
    let mut base = if task.loop_items.is_some() {
        base.shared_by_items()
    } else {
        base
    };
    // Whether the list this loop walks came from a managed host. A loop over a literal list the
    // playbook wrote binds author content; one over `{{ r.stdout_lines }}` binds data, and the
    // items of such a loop are never rendered again.
    let mut items_from_host = false;
    // The groups and keys of `module_defaults`, and the lookups of a `with_` loop, that an include
    // brought in while the play runs have not been asked about yet. A loop over a lookup nothing
    // answers to is refused before its terms are read.
    if !task.module_defaults.is_empty() || task.loop_with.is_some() {
        plan.steps()
            .resolved
            .ensure([task], templar.bridge().map(|bridge| &**bridge))
            .map_err(TemplateError)?;
    }
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
            loop_error = Some(match &task.loop_items {
                Some(raw) => named_undefined(raw, e),
                None => e,
            });
            vec![None]
        }
        Some(Err(e)) => return Err(e),
    };
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
    // The list of all the items is one value for the loop: an item reads it through
    // `ansible_loop.allitems`, see `ALLITEMS_KEY`.
    if let Some(all) = &list_value {
        Arc::make_mut(&mut base.base).insert(ALLITEMS_KEY.into(), all.clone());
    }
    let mut notify_failures = 0;
    let mut first_notify_error: Option<TemplateError> = None;
    for (index, element) in elements.iter().cloned().enumerate() {
        let mut vars = carry
            .as_ref()
            .map_or_else(|| base.clone(), |c| c.vars(&base));
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
            crate::profile::count(crate::profile::Counter::RenderVars, 1);
            let (resolved, untrusted) = templar.resolve_vars_tainted(&vars);
            vars.map = resolved;
            vars.untrusted = untrusted;
        }
        let (label, label_error, label_raised) =
            raises(label_for(task, element.as_ref(), &vars, templar))?;
        // A field of a loop item that fails to render fails that item alone, when the loop
        // reaches it. Measured on ansible-core 2.19.12: `failed: [h] (item=2)` between two items
        // that ran, then `One or more items failed`; an item behind a `break_when` that ended
        // the loop shows nothing. A task with no item fails as a whole.
        let fields = match render_fields(
            task,
            plan,
            templar,
            &vars,
            &fields::Noting {
                prepare: crate::profile::next_prepare(),
                host,
                later_item: index > 0,
                item: index,
                loop_names: [
                    task.loop_var.as_str(),
                    task.index_var.as_deref().unwrap_or_default(),
                    "ansible_loop",
                ],
                defer: index > 0 && !task.break_when.is_empty() && carry.is_none(),
            },
            &mut loop_error,
            warnings,
        ) {
            Ok(fields) => fields,
            Err(err) if element.is_some() => failed_fields(task, &err),
            Err(err) => return Err(err),
        };
        // As `environment`: a keyword the reference renders before the module runs, so an
        // undefined name fails the task even when nothing changes, and a skipped item renders none.
        // In a loop a `notify` that fails to render fails that item only, and the other items
        // run: measured, `failed: [h] (item=2)` between two `changed` items.
        let (notify, skipped) = if fields.skipped.is_some() {
            (Vec::new(), fields.skipped)
        } else {
            match notify_for(task, &vars, templar) {
                Ok(names) => (names, None),
                Err(err) if task.loop_items.is_some() => {
                    notify_failures += 1;
                    first_notify_error.get_or_insert_with(|| err.clone());
                    (
                        Vec::new(),
                        Some(TaskResult::failed_with(failure_text(&err))),
                    )
                }
                Err(err) => return Err(err),
            }
        };
        let mut item = Item {
            element,
            label,
            args: fields.args,
            args_untrusted: fields.args_untrusted,
            vars,
            environment: fields.environment,
            skipped,
            ignore_errors: fields.ignore_errors,
            check_mode: fields.check_mode,
            diff: fields.diff,
            set_fact_names: std::sync::OnceLock::new(),
            notify,
            label_error,
            label_raised,
            deferred: fields.deferred,
        };
        if let Some(carry) = &mut carry {
            carry.take(&item, templar);
            // The reference renders `loop_control.label` after the item ran, so it reads the
            // facts the item just set.
            if task.loop_label.is_some() {
                let mut after = item.vars.clone();
                carry.lay_over(&mut after);
                (item.label, item.label_error, item.label_raised) =
                    raises(label_for(task, item.element.as_ref(), &after, templar))?;
            }
        }
        settle_label(task, &mut item);
        let ends = item.label_raised.is_some();
        items.push(item);
        // The task ends at this item: nothing behind it is rendered or runs.
        if ends {
            break;
        }
    }
    // Every item failing to render its `notify` is the task's own failure.
    if let Some(err) = first_notify_error.filter(|_| notify_failures == items.len()) {
        return Err(err);
    }
    // Only when every item was left out: an item that failed to render fails the task.
    if items
        .iter()
        .all(|i| i.skipped.as_ref().is_some_and(|r| !r.failed()))
    {
        return Ok(Prepared::Skipped(items));
    }
    let delegate = delegate_for(task, &base, templar)?;
    // A word that names no action fails on the controller before any link is opened.
    if is_local(&task.module)
        || crate::action_plugins::controller_side(&task.module)
        || crate::playbook::names_no_action(&task.module)
    {
        late::check_become(task, plan, &base, templar)?;
        if delegate.is_none() {
            unplugged(task, host, (&base, templar, defaults), &mut items, warnings);
        }
        return Ok(Prepared::Local(items, delegate.map(|(name, _)| name)));
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
    let delegate = delegate.map(|(name, data)| {
        let vars = delegate_host_vars(
            &name,
            Named::ByDelegate { data },
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
    // The loader's warning about a module whose collection nobody installed, said once the task
    // runs, before `argspec::refusal` fails it.
    if items.iter().any(|item| item.skipped.is_none())
        && let Some(Some(collection)) = crate::preflight::missing_at_run(task)
    {
        warnings.push(crate::preflight::load_warning(&task.module, &collection));
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

/// What an item whose fields failed to render runs with: nothing, and its failure.
fn failed_fields(task: &crate::playbook::PlayTask, err: &TemplateError) -> Fields {
    let mut failed = Fields::failed(item_failure(task, err));
    // Measured on ansible-core 2.19.12: an item whose `ignore_errors` failed to render counts
    // as ignored, so the loop prints `...ignoring` unless another item failed for good.
    if err
        .0
        .starts_with("Error processing keyword 'ignore_errors'")
    {
        failed.ignore_errors = Some(true);
    }
    failed
}

/// An item that runs nothing reports its label's failure over its result now.
fn settle_label(task: &crate::playbook::PlayTask, item: &mut Item) {
    if let Some(result) = item.skipped.as_mut()
        && let Some(message) = item.label_error.take()
    {
        label_failed(&message, result);
    }
    if let Some(result) = item.skipped.as_mut()
        && let Some(message) = &item.label_raised
    {
        *result = label_raised_result(task, message);
    }
}

/// The arguments and keywords of an item [`prepare`] left for the loop to reach, rendered now,
/// and the warnings the render raised. Measured on ansible-core 2.19.12: an item behind a
/// `break_when` that ended the loop renders none of them, so a lookup in its `msg` never runs.
pub(in crate::executor) fn render_deferred(
    task: &crate::playbook::PlayTask,
    plan: &PlayPlan,
    templar: &Templar,
    host: &str,
    item: &mut Item,
    index: usize,
) -> Vec<String> {
    let mut warnings = Vec::new();
    if !std::mem::take(&mut item.deferred) {
        return warnings;
    }
    let at = fields::Noting {
        prepare: crate::profile::next_prepare(),
        host,
        later_item: true,
        item: index,
        loop_names: [
            task.loop_var.as_str(),
            task.index_var.as_deref().unwrap_or_default(),
            "ansible_loop",
        ],
        defer: false,
    };
    let fields = fields::render_rest(task, plan, templar, &item.vars, &at, &mut warnings)
        .unwrap_or_else(|err| failed_fields(task, &err));
    if fields.skipped.is_some() {
        item.notify.clear();
    }
    item.skipped = fields.skipped;
    item.args = fields.args;
    item.args_untrusted = fields.args_untrusted;
    item.environment = fields.environment;
    item.ignore_errors = fields.ignore_errors;
    item.check_mode = fields.check_mode;
    item.diff = fields.diff;
    settle_label(task, item);
    warnings
}

/// An item's label, the message it fails with, and the error that ends the task after it.
type Labelled = (Option<String>, Option<String>, Option<String>);

/// A label's render as `prepare` keeps it: its error, which an item reports after it ran, is
/// carried rather than raised.
fn raises(
    rendered: Result<(Option<String>, Option<String>), TemplateError>,
) -> Result<Labelled, TemplateError> {
    match rendered {
        Ok((label, error)) => Ok((label, error, None)),
        Err(err) => Ok((None, None, Some(err.0))),
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
