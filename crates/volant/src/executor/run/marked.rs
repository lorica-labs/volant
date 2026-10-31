// SPDX-License-Identifier: GPL-3.0-or-later
//! The error markers and warnings of a `debug: var` whose value did not render.

use serde_json::Value;
use volant_protocol::TaskResult;

use super::argspec::action_name;
use crate::executor::prepare::Item;
use crate::playbook::PlayTask;

/// Each string in a `debug: var` value that is a template left as written because it did not
/// render, written as the reference's `<< error N - ... >>` after the warning that places it at
/// the entry `key` it was written under. The warnings go into `warned`, which the result shows
/// and registers; a loop shows the same one once.
pub(super) fn mark_failed(
    value: &mut Value,
    key: Option<&str>,
    render: &dyn Fn(&str) -> Result<Value, crate::template::TemplateError>,
    count: &mut usize,
    warned: &mut Vec<Value>,
) {
    match value {
        Value::String(text) if crate::template::holds_template(&Value::String(text.clone())) => {
            if let Err(e) = render(text) {
                *count += 1;
                let at = key.and_then(|key| crate::template::written_at(key, text));
                let text = format!(
                    "Encountered 1 template error.
error {count} - {}",
                    e.0
                );
                warned.push(placed_warning(&text, at.as_ref()));
                *value = Value::String(format!("<< error {count} - {} >>", e.0));
            }
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| mark_failed(item, None, render, count, warned)),
        Value::Object(map) => map
            .iter_mut()
            .for_each(|(k, item)| mark_failed(item, Some(k), render, count, warned)),
        _ => {}
    }
}

/// The failure of a vaulted value no secret opens, met rendering a template a `debug: var` value
/// holds. Measured on ansible-core 2.19.12: it fails the task, where any other failure there is
/// only marked ([`mark_failed`]).
pub(super) fn undecryptable_in(
    value: &Value,
    render: &dyn Fn(&str) -> Result<Value, crate::template::TemplateError>,
) -> Option<crate::template::TemplateError> {
    match value {
        Value::String(text) if crate::template::holds_template(value) => render(text)
            .err()
            .filter(|e| e.0.starts_with(crate::vault::UNDECRYPTABLE)),
        Value::Array(items) => items.iter().find_map(|item| undecryptable_in(item, render)),
        Value::Object(map) => map.values().find_map(|item| undecryptable_in(item, render)),
        _ => None,
    }
}

/// What ansible-core 2.19.12 says of a `debug: var` whose value is the `omit` placeholder.
pub(super) const OMITTED: &str =
    "The result of the `var` expression could not be omitted; a placeholder was used instead.";

/// A warning shown at `origin` and registered as `text` alone, as the reference's `debug` does.
pub(super) fn placed_warning(text: &str, origin: Option<&crate::playbook::Origin>) -> Value {
    let place = origin
        .map(|origin| format!("{}\n\n", crate::render::source_context(origin)))
        .unwrap_or_default();
    let raw = crate::render::RAW_WARNING;
    Value::String(format!("{raw}[WARNING]: {text}\n{place}\u{0}{text}"))
}

/// The failure of a `debug` whose arguments its argument spec refuses.
pub(super) fn debug_refusal(task: &PlayTask, item: &Item) -> Option<TaskResult> {
    // An argument the action does not have is refused, as its argument spec does, after
    // the conflict below: measured on ansible-core 2.19.12, a task with both says only that.
    let mut unknown: Vec<&str> = item
        .args
        .keys()
        .map(String::as_str)
        .filter(|key| !["msg", "var", "verbosity"].contains(key))
        .filter(|key| !key.starts_with("_ansible_") && !key.starts_with("_volant_"))
        .collect();
    unknown.sort_unstable();
    // The action refuses both before it looks at either, which is how a `module_defaults`
    // `msg` turns a task's own `var` into a failure.
    let mutually_exclusive = item.args.contains_key("msg") && item.args.contains_key("var");
    // Measured: `var` takes text alone, a number included.
    let not_text = item
        .args
        .get("var")
        .filter(|v| !v.is_null() && !v.is_string());
    if !unknown.is_empty() || mutually_exclusive || not_text.is_some() {
        let msg = if mutually_exclusive {
            "parameters are mutually exclusive: msg|var".to_string()
        } else if let (true, Some(v)) = (unknown.is_empty(), not_text) {
            format!(
                "argument 'var' is of type {} and we were unable to convert to _check_type_str_no_conversion: '{}' is not a string and conversion is not allowed",
                super::super::python_type(v),
                crate::playbook::python_repr(v)
            )
        } else {
            format!(
                "Unsupported parameters for ({}) module: {}. Supported parameters include: msg, var, verbosity.",
                action_name(
                    &task.module,
                    "ansible_collections.ansible.builtin.plugins.action.debug"
                ),
                unknown.join(", ")
            )
        };
        let msg = msg.as_str();
        let mut failed = TaskResult::failed_with(msg);
        failed.0.insert(
            crate::render::ERROR_KEY.into(),
            Value::from(crate::render::task_failed(msg)),
        );
        return Some(failed);
    }
    None
}

/// Whether `text` is a template holding one string literal and nothing else: measured on
/// ansible-core 2.19.12, what it renders to is not trusted, so a `debug: var` refuses it.
pub(super) fn literal_only(text: &str) -> bool {
    let inner = text
        .trim()
        .strip_prefix("{{")
        .and_then(|t| t.strip_suffix("}}"));
    let inner = inner.map(str::trim).unwrap_or_default();
    ['"', '\''].into_iter().any(|q| {
        inner.len() >= 2
            && inner.starts_with(q)
            && inner.ends_with(q)
            && !inner[1..inner.len() - 1].contains(q)
    })
}
