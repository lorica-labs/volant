// SPDX-License-Identifier: GPL-3.0-or-later
//! An item's `loop_control.label`: rendered, and the failures it leaves on a result.

use serde_json::Value;
use volant_protocol::TaskResult;
use volant_protocol::modules::short_name;

use crate::playbook::PlayTask;
use crate::template::{Templar, TemplateError};
use crate::vars::HostVars;

use super::display;
use super::item::undefined_name;

/// The key a result carries when its item's label raised: the task's failure, standing for the
/// whole loop. Taken off with the other `_volant_` keys.
pub(in crate::executor) const LABEL_RAISED_KEY: &str = "_volant_label_raised";

/// The task failure a label that raised leaves. Measured on ansible-core 2.19.12: the items before
/// it print their lines, then `fatal: [h]: FAILED! => {"msg": ...}` with the error at the label's
/// value, and the task registers `exception`, `failed` and `msg` alone.
pub(in crate::executor) fn label_raised_result(task: &PlayTask, message: &str) -> TaskResult {
    let mut r = TaskResult::failed_with(message);
    r.0.insert("exception".into(), "(traceback unavailable)".into());
    r.0.insert(crate::render::ERROR_KEY.into(), message.into());
    if let Some(at) = task.value_origins.get("loop_control.label") {
        r.0.insert(
            crate::render::AT_KEY.into(),
            crate::render::cause("", Some(at), None),
        );
    }
    r.0.insert(LABEL_RAISED_KEY.into(), Value::Bool(true));
    r
}

/// The result a label that raised left at the end of `results`, if one did.
pub(in crate::executor) fn raised_by_label(
    results: &[(Option<Value>, TaskResult)],
) -> Option<&TaskResult> {
    results
        .last()
        .map(|(_, r)| r)
        .filter(|r| r.0.contains_key(LABEL_RAISED_KEY))
}

/// The `(item=...)` label of one item and, when its `loop_control.label` fails on an undefined
/// variable, the message the item fails with instead. Measured on ansible-core 2.19.12: `failed:
/// [h] (item=2) => {..., "msg": "Failed to template loop_control.label: 'nope' is undefined"}`,
/// after the module ran, and the items after it run. The line then shows the item itself, which a
/// `debug` result no longer holds once it is cleaned for display: `(item=None)` there.
///
/// Any other error, a lookup that raises for one, ends the reference's whole task once the item
/// ran: the caller keeps it as the item's `label_raised`.
pub(super) fn label_for(
    task: &PlayTask,
    element: Option<&Value>,
    vars: &HostVars,
    templar: &Templar,
) -> Result<(Option<String>, Option<String>), TemplateError> {
    Ok(match (element, &task.loop_label) {
        (None, _) => (None, None),
        (Some(el), None) => (Some(display(el)), None),
        // Rendered once the item ran, by `label_after`.
        (Some(el), Some(template)) if reads_register(task, template, templar) => {
            (Some(display(el)), None)
        }
        (Some(el), Some(template)) => match templar.render(template, vars) {
            Ok(label) => (Some(display(&label)), None),
            Err(e) if e.is_undefined() => (
                Some(if short_name(&task.module) == "debug" {
                    "None".to_string()
                } else {
                    display(el)
                }),
                Some(format!(
                    "Failed to template loop_control.label: {}",
                    undefined_name(template, e.0)
                )),
            ),
            Err(e) => return Err(e),
        },
    })
}

/// Whether `template`, a `loop_control.label`, reads the name the task registers.
fn reads_register(task: &PlayTask, template: &str, templar: &Templar) -> bool {
    task.register.as_ref().is_some_and(|name| {
        templar
            .env()
            .template_from_str(template)
            .is_ok_and(|t| t.undeclared_variables(false).contains(name))
    })
}

/// The label of an item that ran, as `item.label` holds it unless the label reads the name the
/// task registers. Then it is rendered now, with that name bound to the item's own result, and a
/// label that fails fails the result. Measured on ansible-core 2.19.12: `label: "{{ r.stdout }}"`
/// with `register: r` shows each item's own output, and a skipped item, which has none, registers
/// `Failed to template loop_control.label: ... has no attribute 'stdout'`.
pub(in crate::executor) fn label_after(
    task: &PlayTask,
    item: &super::Item,
    result: &mut TaskResult,
    templar: &Templar,
) -> Option<String> {
    let (Some(element), Some(template), Some(name)) =
        (&item.element, &task.loop_label, &task.register)
    else {
        return item.label.clone();
    };
    if !reads_register(task, template, templar) {
        return item.label.clone();
    }
    let mut vars = item.vars.clone();
    vars.insert(name.clone(), Value::Object(result.0.clone()));
    match templar.render(template, &vars) {
        Ok(label) => Some(display(&label)),
        Err(e) => {
            let message = format!(
                "Failed to template loop_control.label: {}",
                undefined_name(template, e.0)
            );
            label_failed(&message, result);
            Some(display(element))
        }
    }
}

/// `result` failed by a label that did not render, as the reference's `res.update` fails it.
pub(in crate::executor) fn label_failed(message: &str, result: &mut TaskResult) {
    result.0.insert("failed".into(), Value::Bool(true));
    result
        .0
        .insert("msg".into(), Value::String(message.to_string()));
    result
        .0
        .insert(crate::render::NO_BLOCK_KEY.into(), Value::Bool(true));
}

/// An item's label, the message it fails with, and the error that ends the task after it.
pub(super) type Labelled = (Option<String>, Option<String>, Option<String>);

/// A label's render as `prepare` keeps it: its error, which an item reports after it ran, is
/// carried rather than raised.
pub(super) fn raises(
    rendered: Result<(Option<String>, Option<String>), TemplateError>,
) -> Result<Labelled, TemplateError> {
    match rendered {
        Ok((label, error)) => Ok((label, error, None)),
        Err(err) => Ok((None, None, Some(err.0))),
    }
}
