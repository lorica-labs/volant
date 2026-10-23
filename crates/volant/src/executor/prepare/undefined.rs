// SPDX-License-Identifier: GPL-3.0-or-later
//! How a task that dies on a name with no value is worded, and the chain and place its error
//! carries: its arguments, its `when` and its `loop_control` fields.

use serde_json::Value;

use crate::playbook::PlayTask;
use crate::template::TemplateError;
use volant_protocol::TaskResult;

/// What the reference puts before the sentence of a condition it cannot evaluate.
pub(in crate::executor) const CONDITIONAL: &str = "Error while evaluating conditional: ";

/// What a `when` that fails reports: the error a loop kept, which is the loop's to raise, else
/// `err`, worded as the reference words a condition that read a missing name.
pub(super) fn when_error(err: TemplateError, kept: Option<&TemplateError>) -> TemplateError {
    match kept {
        Some(kept) => kept.clone(),
        None if err.is_undefined() => TemplateError(format!("{CONDITIONAL}{}", err.0)),
        None => err,
    }
}

/// The chain of a task whose `when` could not be evaluated: the condition's sentence at where the
/// `when` was written.
pub(in crate::executor) fn conditional_causes(task: &PlayTask, message: &str) -> Option<Value> {
    let sentence = message.strip_prefix("Task failed: ").unwrap_or(message);
    sentence.starts_with(CONDITIONAL).then(|| {
        serde_json::json!([crate::render::cause(
            sentence,
            task.value_origins.get("when"),
            None
        )])
    })
}

/// Makes `result` the error of a `loop_control` keyword that read a missing name. Measured on
/// ansible-core 2.19.12: the error stands alone, with no `Task failed: `, at the value.
pub(in crate::executor) fn mark_keyword(
    task: &PlayTask,
    err: &TemplateError,
    result: &mut TaskResult,
) {
    if let Some(at) = keyword_at(task, err) {
        result
            .0
            .insert(crate::render::ERROR_KEY.into(), err.0.clone().into());
        result.0.insert(crate::render::AT_KEY.into(), at);
    }
}

/// Where a `loop_control` keyword's value was written, when the failure is that value reading a
/// missing name.
fn keyword_at(task: &PlayTask, err: &TemplateError) -> Option<Value> {
    let rest = err.0.strip_prefix(super::item::KEYWORD_ERROR)?;
    let (key, why) = rest.strip_prefix('\'')?.split_once("': ")?;
    let origin = task.value_origins.get(&format!("loop_control.{key}"))?;
    TemplateError(why.to_string())
        .is_undefined()
        .then(|| crate::render::cause("", Some(origin), None))
}

/// The chain of a task whose arguments a plugin failed to render, as the reference prints it:
/// `Finalization of task args for '<action>' failed.` at the action's key, then the value it was
/// resolving at that value.
pub(in crate::executor) fn finalization_causes(task: &PlayTask, message: &str) -> Option<Value> {
    let rest = message.strip_prefix(super::item::FINALIZING)?;
    let at = rest.find(&format!(": {}", crate::template::RESOLVING))?;
    let head = format!("{}{}.", super::item::FINALIZING, &rest[..at]);
    let resolving = &rest[at + 2..];
    let key = resolving[crate::template::RESOLVING.len()..]
        .strip_prefix('\'')?
        .split('\'')
        .next()?;
    let origin = |name: &str| task.value_origins.get(name);
    let resolved = crate::render::cause(resolving, origin(&format!("args.{key}")), None);
    // A task with no name of its own starts where its action does, and the reference puts the
    // two links in one: the block opens with the finalization's sentence.
    // A syntax error is the head of the block itself, not a link behind it: measured on
    // ansible-core 2.19.12, `No filter named` prints one `caused by`, the value.
    if resolving.contains(crate::template::NO_FILTER)
        || (task.origin.is_some() && task.origin.as_ref() == origin("action"))
    {
        return Some(Value::Array(vec![
            crate::render::lead(&format!("Task failed: {head}")),
            resolved,
        ]));
    }
    Some(Value::Array(vec![
        crate::render::cause(&head, origin("action"), None),
        resolved,
    ]))
}
