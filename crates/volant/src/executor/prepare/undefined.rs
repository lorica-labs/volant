// SPDX-License-Identifier: GPL-3.0-or-later
//! How a task that dies on a name with no value is worded and where its error points, for the
//! keywords whose wording is not a task argument's.

use serde_json::Value;

use crate::playbook::PlayTask;
use crate::template::TemplateError;

/// What the reference puts before the sentence of a condition it cannot evaluate.
pub(in crate::executor) const CONDITIONAL: &str = "Error while evaluating conditional: ";

/// `err` as a `when` that read a missing name fails: any other failure is left as it is.
pub(super) fn conditional_error(err: TemplateError) -> TemplateError {
    if err.is_undefined() {
        TemplateError(format!("{CONDITIONAL}{}", err.0))
    } else {
        err
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

/// Where a `loop_control` keyword's value was written, when the failure is that value reading a
/// missing name. Measured on ansible-core 2.19.12: the error stands alone, at the value.
pub(in crate::executor) fn keyword_at(task: &PlayTask, err: &TemplateError) -> Option<Value> {
    let rest = err.0.strip_prefix(super::item::KEYWORD_ERROR)?;
    let (key, why) = rest.strip_prefix('\'')?.split_once("': ")?;
    let origin = task.value_origins.get(&format!("loop_control.{key}"))?;
    TemplateError(why.to_string())
        .is_undefined()
        .then(|| crate::render::cause("", Some(origin), None))
}
