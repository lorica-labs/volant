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
        // A lookup's failure is the reference's `Task failed: <plugin sentence>`, with the
        // sentence again at the `when`; a `with_` loop's lookup has no task around it.
        None if super::item::is_loop_lookup_failure(&err) => {
            TemplateError(format!("Task failed: {}", err.0))
        }
        None => err,
    }
}

/// The chain of a task whose `when` could not be evaluated, the condition's sentence at where the
/// `when` was written, or whose keyword did not render ([`keyword_causes`]).
pub(in crate::executor) fn conditional_causes(task: &PlayTask, message: &str) -> Option<Value> {
    when_causes(task, message).or_else(|| keyword_causes(task, message))
}

fn when_causes(task: &PlayTask, message: &str) -> Option<Value> {
    let sentence = message.strip_prefix("Task failed: ").unwrap_or(message);
    // A lookup's sentence without `Task failed: ` is a `with_` loop's, shown with no task around it.
    let lookup = message.starts_with("Task failed: The lookup plugin '");
    if sentence.starts_with(CONDITIONAL) || lookup {
        return Some(serde_json::json!([crate::render::cause(
            sentence,
            task.value_origins.get("when"),
            None
        )]));
    }
    // A plugin's own failure stands as the condition's error, with the causes it has behind it.
    // (a `with_` loop's lookup has no task around it, so it is not this chain)
    if (message.starts_with("Task failed: ") || !sentence.starts_with("The lookup plugin '"))
        && sentence.starts_with("The ")
        && crate::template::is_plugin_failure(sentence)
    {
        let (sentence, behind) = split_plugin_failure(sentence);
        let mut chain = vec![crate::render::cause(
            &sentence,
            task.value_origins.get("when"),
            None,
        )];
        chain.extend(behind);
        return Some(Value::Array(chain));
    }
    None
}

/// The chain of a task keyword that failed to render: measured on ansible-core 2.19.12, one cause,
/// the keyword's sentence at where its value was written (`check_mode`, `diff`, `ignore_errors`,
/// `notify`). A keyword whose value has no recorded place has no chain.
fn keyword_causes(task: &PlayTask, message: &str) -> Option<Value> {
    let sentence = message.strip_prefix("Task failed: ").unwrap_or(message);
    let key = sentence
        .strip_prefix(super::item::KEYWORD_ERROR)?
        .strip_prefix('\'')?
        .split('\'')
        .next()?;
    let origin = task.value_origins.get(key)?;
    Some(serde_json::json!([crate::render::cause(
        sentence,
        Some(origin),
        None
    )]))
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
    // A vaulted value no secret opens is a third link, at the value: the argument's own link
    // ends at `Error rendering template.`.
    let (resolving, vaulted) = match resolving.split_once(": Error rendering template: ") {
        Some((before, reason)) if reason.starts_with(crate::vault::UNDECRYPTABLE) => (
            format!("{before}: Error rendering template."),
            Some(crate::render::cause(
                reason,
                crate::vault::failure_origin(reason).as_ref(),
                None,
            )),
        ),
        // An include a file of `lookup('template')` asked for: the argument's link ends at its
        // key, and the refusal is a link at that file.
        Some((before, reason))
            if crate::template::asked_in(&format!("Error rendering template: {reason}"))
                .is_some() =>
        {
            let said = format!("Error rendering template: {reason}");
            let file = crate::template::asked_in(&said).map(|f| crate::playbook::Origin {
                path: f.display().to_string().into(),
                line: 0,
                col: 0,
            });
            (
                format!("{before}."),
                Some(crate::render::cause(&said, file.as_ref(), None)),
            )
        }
        _ => (resolving.to_string(), None),
    };
    // A plugin that failed on an exception of its own is a block of its sentence, then one bare
    // block per cause.
    let (resolving, behind) = split_plugin_failure(&resolving);
    let resolved = crate::render::cause(&resolving, origin(&format!("args.{key}")), None);
    // A task with no name of its own starts where its action does, and the reference puts the
    // two links in one: the block opens with the finalization's sentence.
    // A syntax error is the head of the block itself, not a link behind it: measured on
    // ansible-core 2.19.12, `No filter named` prints one `caused by`, the value.
    let mut chain = if resolving.contains(crate::template::NO_FILTER)
        || (task.origin.is_some() && task.origin.as_ref() == origin("action"))
    {
        vec![
            crate::render::lead(&format!("Task failed: {head}")),
            resolved,
        ]
    } else {
        vec![
            crate::render::cause(&head, origin("action"), None),
            resolved,
        ]
    };
    chain.extend(vaulted);
    chain.extend(behind);
    Some(Value::Array(chain))
}

/// `text`, a message that ends with the failure of a bridged plugin, as the sentence of its own
/// block, and the bare blocks of its causes behind it.
fn split_plugin_failure(text: &str) -> (String, Vec<Value>) {
    let (sentence, behind) = crate::bridge::split_failure(text);
    (
        sentence,
        behind
            .iter()
            .map(|block| crate::render::cause(block, None, None))
            .collect(),
    )
}
