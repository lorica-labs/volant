// SPDX-License-Identifier: GPL-3.0-or-later
//! The boolean keywords a task writes as templates, rendered for one item.

use serde_json::Value;

use crate::playbook::{Flag, python_repr};
use crate::template::{Templar, TemplateError};
use crate::vars::HostVars;

use super::item::undefined_name;

/// `ignore_errors` written as a template, rendered for one item and read the way the reference
/// reads a boolean keyword ([`strict_boolean`]). Measured on ansible-core 2.19.12, the reference
/// renders it when the task runs, and a value that is not a boolean fails the task with the
/// sentence the loader refuses a literal with.
pub(super) fn ignore_errors_for(
    raw: &str,
    vars: &HostVars,
    templar: &Templar,
) -> Result<bool, TemplateError> {
    boolean_keyword("ignore_errors", raw, vars, templar)
}

/// A task's `check_mode` or `diff` for one item: a literal as written, a template rendered
/// against the item's variables, and nothing for an item a `when` left out.
pub(super) fn flag_for(
    key: &str,
    flag: Option<&Flag>,
    vars: &HostVars,
    templar: &Templar,
    skipped: bool,
) -> Result<Option<bool>, TemplateError> {
    match flag {
        Some(Flag::Fixed(b)) => Ok(Some(*b)),
        Some(Flag::Template(raw)) if omitted(raw, vars, templar) => Ok(None),
        Some(Flag::Template(raw)) if !skipped => boolean_keyword(key, raw, vars, templar).map(Some),
        _ => Ok(None),
    }
}

/// Whether a keyword's template renders to `omit`, which leaves the keyword to the layers around
/// the task: measured on ansible-core 2.19.12, `become: '{{ omit }}'` under a play's `become:
/// yes` escalates.
pub(super) fn omitted(raw: &str, vars: &HostVars, templar: &Templar) -> bool {
    templar
        .render(raw, vars)
        .is_ok_and(|v| v == crate::vars::omit_token())
}

/// A boolean keyword written as a template, rendered and read as ansible-core reads it.
pub(super) fn boolean_keyword(
    key: &str,
    raw: &str,
    vars: &HostVars,
    templar: &Templar,
) -> Result<bool, TemplateError> {
    let keyword =
        |detail: String| TemplateError(format!("Error processing keyword '{key}': {detail}"));
    let rendered = templar
        .render(raw, vars)
        .map_err(|e| keyword(undefined_name(raw, e.0)))?;
    strict_boolean(&rendered).ok_or_else(|| {
        keyword(format!(
            "The value {} could not be converted to 'bool'.",
            python_repr(&rendered)
        ))
    })
}

/// ansible-core's `boolean(value, strict=True)`, ported from
/// `ansible/module_utils/parsing/convert_bool.py` (2.19.12), which a boolean keyword is read
/// with: a real boolean; `1`, `1.0`, `0`, `0.0`; or one of `y yes on 1 true t` and
/// `n no off 0 false f`, trimmed and in any case. Anything else is not a boolean.
pub(in crate::executor) fn strict_boolean(value: &Value) -> Option<bool> {
    let word = match value {
        Value::Bool(b) => return Some(*b),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.trim().to_lowercase(),
        _ => return None,
    };
    match word.as_str() {
        "y" | "yes" | "on" | "1" | "1.0" | "true" | "t" => Some(true),
        "n" | "no" | "off" | "0" | "0.0" | "-0.0" | "false" | "f" => Some(false),
        _ => None,
    }
}

/// `ansible_python_module_rlimit_nofile` that does not render fails the task, as the reference's
/// module wrapper renders it: measured on ansible-core 2.19.12, `"{{ undefined_variable }}"`
/// fails it, and `"{{ omit }}"` leaves the limit alone.
pub(super) fn check_rlimit(vars: &HostVars, templar: &Templar) -> Result<(), TemplateError> {
    match crate::vars::host_setting(vars, "ansible_python_module_rlimit_nofile") {
        Some(Value::String(raw)) if Templar::is_template(raw) => {
            templar.render(raw, vars).map(drop)
        }
        _ => Ok(()),
    }
}
