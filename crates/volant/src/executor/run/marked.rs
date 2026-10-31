// SPDX-License-Identifier: GPL-3.0-or-later
//! The error markers and warnings of a `debug: var` whose value did not render.

use serde_json::Value;

/// The warning ansible-core 2.19.12 shows before a `debug: var` result whose expression failed,
/// at where the expression was written; a loop warns once for the same place. `n` numbers the
/// errors of one value.
pub(super) fn warn_template_error(
    n: usize,
    message: &str,
    origin: Option<&crate::playbook::Origin>,
) {
    let place = origin
        .map(|origin| format!("{}\n\n", crate::render::source_context(origin)))
        .unwrap_or_default();
    crate::render::warn_once(&format!(
        "[WARNING]: Encountered 1 template error.\nerror {n} - {message}\n{place}"
    ));
}

/// Each string in a `debug: var` value that is a template left as written because it did not
/// render, written as the reference's `<< error N - ... >>` after the warning that places it at
/// the entry `key` it was written under.
pub(super) fn mark_failed(
    value: &mut Value,
    key: Option<&str>,
    render: &dyn Fn(&str) -> Result<Value, crate::template::TemplateError>,
    count: &mut usize,
) {
    match value {
        Value::String(text) if crate::template::holds_template(&Value::String(text.clone())) => {
            if let Err(e) = render(text) {
                *count += 1;
                let at = key.and_then(|key| crate::template::written_at(key, text));
                warn_template_error(*count, &e.0, at.as_ref());
                *value = Value::String(format!("<< error {count} - {} >>", e.0));
            }
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| mark_failed(item, None, render, count)),
        Value::Object(map) => map
            .iter_mut()
            .for_each(|(k, item)| mark_failed(item, Some(k), render, count)),
        _ => {}
    }
}
