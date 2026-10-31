// SPDX-License-Identifier: GPL-3.0-or-later
//! The keywords a playbook wrote as templates, rendered for the host that runs the task.
//!
//! Measured on ansible-core 2.19.12: `no_log`, `become`, `run_once`, `any_errors_fatal`,
//! `ignore_unreachable`, `delegate_facts`, `timeout`, `async` and `poll` are rendered when the
//! task runs, once its `when` has held, and a value the keyword cannot take fails that task with
//! `Task failed: Error processing keyword '<name>': The value <value> could not be converted to
//! '<type>'.` A keyword written as a literal is read at load and never comes here.

use std::sync::Mutex;

use serde_json::Value;

use crate::compile::Step;
use crate::executor::coordinator::Progress;
use crate::playbook::{Deferred, PlayTask, python_repr};
use crate::template::{Templar, TemplateError};
use crate::vars::{HostVars, VarStore};

use super::item::{boolean_keyword, undefined_name};
use super::plan::PlayPlan;
use super::vars::host_vars;

/// The longest `timeout` the reference accepts, in seconds.
const TIMEOUT_CEILING: i64 = 100_000_000;

/// What rendering a step's templated keywords for one host came to.
#[derive(Default)]
pub(in crate::executor) struct Resolution {
    /// The step with each keyword that rendered put into its typed field. `None` when the step
    /// has nothing to render, in which case the step as it stands is the one to run.
    pub(in crate::executor) step: Option<Box<Step>>,
    /// Lines to print before the task, as the reference does.
    pub(in crate::executor) warnings: Vec<String>,
    /// The failure of the first keyword that did not render to something it can take. The step
    /// above still carries what could be read, since the task's own line is still printed.
    pub(in crate::executor) error: Option<TemplateError>,
}

impl Resolution {
    fn fail(&mut self, err: TemplateError) {
        self.error.get_or_insert(task_failed(err));
    }
}

/// A boolean keyword that may be a template: the literal when there is one, the rendering of the
/// template otherwise, and `None` when the keyword is written nowhere.
pub(super) fn keyword_bool(
    key: &str,
    fixed: Option<bool>,
    template: Option<&str>,
    vars: &HostVars,
    templar: &Templar,
) -> Result<Option<bool>, TemplateError> {
    match template {
        Some(raw) if super::item::omitted(raw, vars, templar) => Ok(None),
        Some(raw) => boolean_keyword(key, raw, vars, templar)
            .map(Some)
            .map_err(task_failed),
        None => Ok(fixed),
    }
}

/// A task that runs on the controller escalates nowhere, and still has its `become` read: the
/// reference validates every keyword of a task, whichever side runs it.
pub(super) fn check_become(
    task: &PlayTask,
    plan: &PlayPlan,
    vars: &HostVars,
    templar: &Templar,
) -> Result<(), TemplateError> {
    let own = keyword_bool(
        "become",
        task.r#become,
        task.deferred.r#become.as_deref(),
        vars,
        templar,
    )?;
    if own.is_none() {
        keyword_bool(
            "become",
            plan.r#become.0,
            plan.r#become.1.as_deref(),
            vars,
            templar,
        )?;
    }
    Ok(())
}

/// The step of a task whose preparation failed, with a templated `no_log` read first: a failed
/// task hides its result as a task that ran does. The keyword is rendered with the variables the
/// host has, and a value that cannot be read, or is no boolean, hides the result. `None` when the
/// task wrote no such template.
pub(in crate::executor) fn hide_after_failure(
    step: &Step,
    host: &str,
    plan: &PlayPlan,
    live: &Progress,
    templar: &Templar,
    store: &Mutex<VarStore>,
) -> Option<Box<Step>> {
    let raw = step.task.deferred.no_log.as_deref()?;
    let playbook_dir = store
        .lock()
        .expect("vars lock")
        .playbook_dir()
        .to_path_buf();
    let vars = host_vars(
        host,
        plan,
        &step.task.vars,
        step.role,
        step.include_params.as_deref(),
        live,
        templar,
        store,
        &step.origin,
        &playbook_dir,
    );
    let hidden = boolean_keyword("no_log", raw, &vars, templar).unwrap_or(true);
    let mut copy = step.clone();
    copy.task.no_log = Some(hidden);
    Some(Box::new(copy))
}

/// The reference's prefix on the message of a task that dies before it runs.
fn task_failed(err: TemplateError) -> TemplateError {
    TemplateError(format!("Task failed: {}", err.0))
}

/// Renders the templated keywords of `step` for one host. `items` holds the variables of each loop
/// item that runs, or of the task itself when it has no loop; the caller has already evaluated the
/// task's `when` and holds no skipped item.
///
/// The keywords belong to the task here, and the reference renders them for each item: the first
/// item's value stands for the others, except for `no_log`, which hides the whole task when any
/// item's value says so, so that no item can show what another was meant to hide.
pub(in crate::executor) fn resolve(
    step: &Step,
    items: &[&HostVars],
    templar: &Templar,
) -> Resolution {
    let waiting = &step.task.deferred;
    let Some(&vars) = items.first() else {
        return Resolution::default();
    };
    if !has_work(waiting) {
        return Resolution::default();
    }
    let mut task = step.task.clone();
    let mut out = Resolution::default();
    if let Some(raw) = &waiting.any_errors_fatal {
        // Validated and not kept: the strategy reads the keyword as written, see
        // [`PlayTask::fails_all`].
        if let Err(err) = boolean_keyword("any_errors_fatal", raw, vars, templar) {
            out.fail(err);
        }
    }
    if let Some(raw) = &waiting.async_val {
        match whole("async_val", raw, vars, templar) {
            Ok(n) => match u64::try_from(n) {
                Ok(n) => task.async_val = n,
                Err(_) => out.fail(negative("async_val", n)),
            },
            Err(err) => out.fail(err),
        }
    }
    if let Some(raw) = &waiting.delegate_facts {
        match boolean_keyword("delegate_facts", raw, vars, templar) {
            Ok(b) => task.delegate_facts = Some(b),
            Err(err) => out.fail(err),
        }
    }
    if let Some(raw) = &waiting.ignore_unreachable {
        match boolean_keyword("ignore_unreachable", raw, vars, templar) {
            Ok(b) => task.ignore_unreachable = Some(b),
            Err(err) => out.fail(err),
        }
    }
    if let Some(raw) = &waiting.no_log {
        let mut hidden = false;
        for vars in items {
            match boolean_keyword("no_log", raw, vars, templar) {
                Ok(b) => hidden |= b,
                // Measured: a value that is not a boolean masks the task's output and warns, and
                // the task still fails on the keyword.
                Err(err) => {
                    hidden = true;
                    out.warnings.push(format!(
                        "Invalid no_log value for task, output will be masked: {}",
                        err.0
                    ));
                    out.fail(err);
                    break;
                }
            }
        }
        task.no_log = Some(hidden);
    }
    if let Some(raw) = &waiting.poll {
        match whole("poll", raw, vars, templar) {
            Ok(n) => match u64::try_from(n) {
                Ok(n) => task.poll = Some(n),
                Err(_) => out.fail(negative("poll", n)),
            },
            Err(err) => out.fail(err),
        }
    }
    if let Some(raw) = &waiting.run_once {
        // The value was decided before any host got here, by the coordinator, from the same text.
        // What a host checks is that the text is a boolean for it.
        if let Err(err) = boolean_keyword("run_once", raw, vars, templar) {
            out.fail(err);
        }
    }
    if let Some(raw) = &waiting.timeout {
        match whole("timeout", raw, vars, templar) {
            Ok(n) if (0..=TIMEOUT_CEILING).contains(&n) => task.timeout = Some(n as u64),
            Ok(n) => out.fail(TemplateError(format!(
                "Timeout {n} is invalid, it must be between 0 and {TIMEOUT_CEILING}."
            ))),
            Err(err) => out.fail(err),
        }
    }
    out.step = Some(Box::new(Step {
        task,
        ..step.clone()
    }));
    out
}

/// ansible-core 2.19.12 runs a task with a negative `async` or `poll`; the typed fields here are
/// unsigned, as for a literal, which the loader refuses, so a negative value is refused by name.
fn negative(key: &str, n: i64) -> TemplateError {
    TemplateError(format!(
        "Error processing keyword '{key}': the value {n} is negative, which is not supported yet."
    ))
}

/// Whether a layer holds a keyword for [`resolve`]. `become`, `gather_facts` and `force_handlers`
/// are rendered where they are read, with the play's own and the task's.
fn has_work(waiting: &Deferred) -> bool {
    [
        &waiting.any_errors_fatal,
        &waiting.async_val,
        &waiting.delegate_facts,
        &waiting.ignore_unreachable,
        &waiting.no_log,
        &waiting.poll,
        &waiting.run_once,
        &waiting.timeout,
    ]
    .iter()
    .any(|slot| slot.is_some())
}

/// A keyword the reference declares `isa='int'`, rendered and converted as it does: a whole
/// number, however spelled (`3`, `'3'`, `3.0`, `true`), and nothing it would have to truncate.
fn whole(key: &str, raw: &str, vars: &HostVars, templar: &Templar) -> Result<i64, TemplateError> {
    let rendered = templar.render(raw, vars).map_err(|e| {
        TemplateError(format!(
            "Error processing keyword '{key}': {}",
            undefined_name(raw, e.0)
        ))
    })?;
    as_whole(&rendered).ok_or_else(|| {
        TemplateError(format!(
            "Error processing keyword '{key}': The value {} could not be converted to 'int'.",
            python_repr(&rendered)
        ))
    })
}

fn as_whole(value: &Value) -> Option<i64> {
    let number = match value {
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    (number.is_finite() && number.fract() == 0.0 && number.abs() < 9.0e15).then_some(number as i64)
}

/// The `bool` a rendered `run_once` is read as by the strategy: what Python makes of it as a
/// condition, so a text that is not a boolean still elects a runner. See `PlayTask::fails_all`
/// for the same reading of `any_errors_fatal`.
pub(in crate::executor) fn strategy_reads_as_true(value: &Value) -> bool {
    crate::template::truthy(value)
}

/// Whether `item` ignores a host it cannot reach: the task's `ignore_unreachable`, rendered for
/// the item when it was written as a template. Measured on ansible-core 2.19.12:
/// `ignore_unreachable: "{{ item }}"` is read per item.
pub(in crate::executor) fn item_ignores_unreachable(
    task: &PlayTask,
    item: &super::Item,
    templar: &Templar,
) -> bool {
    match &task.deferred.ignore_unreachable {
        Some(raw) => {
            boolean_keyword("ignore_unreachable", raw, &item.vars, templar).unwrap_or(false)
        }
        None => task.ignores_unreachable(),
    }
}
