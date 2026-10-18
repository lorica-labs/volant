// SPDX-License-Identifier: GPL-3.0-or-later
//! Loop items: `loop`, `with_*` and their options.

use serde_json::{Map, Value};

use crate::playbook::PlayTask;
use crate::template::{Templar, TemplateError};
use crate::vars::HostVars;

use super::item::KEYWORD_ERROR;
use crate::executor::as_bool_value;

/// What `loop_control`'s `extended`, `extended_allitems` and `pause` come to once rendered.
pub(super) struct LoopOptions {
    pub(in crate::executor) extended: bool,
    pub(in crate::executor) allitems: bool,
    pub(in crate::executor) pause: Option<std::time::Duration>,
}

/// The three `loop_control` fields the reference templates per task, rendered against `vars` and
/// converted. Measured on ansible-core 2.19.12: a value that does not convert fails the task
/// with `Error processing keyword 'pause': The value 'nope' could not be converted to 'float'.`,
/// and an undefined name with `Error processing keyword 'pause': 'x' is undefined`; a negative
/// `pause` is accepted and waits for nothing.
pub(super) fn loop_options(
    task: &PlayTask,
    vars: &HostVars,
    templar: &Templar,
) -> Result<LoopOptions, TemplateError> {
    let rendered = |key: &str, raw: &Option<Value>| -> Result<Option<Value>, TemplateError> {
        raw.as_ref()
            .map(|raw| {
                templar
                    .render_value(raw, vars)
                    .map_err(|e| TemplateError(format!("{KEYWORD_ERROR}'{key}': {}", e.0)))
            })
            .transpose()
    };
    let unconverted = |key: &str, value: &Value, to: &str| {
        let shown = match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        TemplateError(format!(
            "{KEYWORD_ERROR}'{key}': The value '{shown}' could not be converted to '{to}'."
        ))
    };
    let flag = |key: &str, raw: &Option<Value>, default: bool| -> Result<bool, TemplateError> {
        match rendered(key, raw)? {
            None => Ok(default),
            Some(Value::Number(n)) if n.as_i64() == Some(0) => Ok(false),
            Some(Value::Number(n)) if n.as_i64() == Some(1) => Ok(true),
            Some(value) => as_bool_value(&value).ok_or_else(|| unconverted(key, &value, "bool")),
        }
    };
    let pause = match rendered("pause", &task.loop_pause)? {
        None => None,
        Some(value) => {
            let seconds = match &value {
                Value::Number(n) => n.as_f64(),
                Value::Bool(b) => Some(f64::from(u8::from(*b))),
                Value::String(s) => s.trim().parse::<f64>().ok(),
                _ => None,
            }
            .ok_or_else(|| unconverted("pause", &value, "float"))?;
            if seconds > 0.0 {
                Some(
                    std::time::Duration::try_from_secs_f64(seconds)
                        .map_err(|_| unconverted("pause", &value, "float"))?,
                )
            } else {
                None
            }
        }
    };
    Ok(LoopOptions {
        extended: flag("extended", &task.loop_extended, false)?,
        allitems: flag("extended_allitems", &task.loop_allitems, true)?,
        pause,
    })
}

/// The list a `loop:` or `with_items:` walks, and whether it came from a managed host.
pub(super) fn loop_list(
    templar: &Templar,
    task: &PlayTask,
    raw: &Value,
    vars: &HostVars,
) -> Result<(Vec<Value>, bool), TemplateError> {
    let (rendered, tainted) = templar.render_value_tainted(raw, vars)?;
    let list = match rendered {
        Value::Array(items) => items,
        other => {
            return Err(TemplateError(format!(
                "Invalid data passed to 'loop', it requires a list, got this instead: {other}"
            )));
        }
    };
    // `with_items` flattens one level; `loop` does not.
    let list = if task.with_items {
        flatten_once(list)
    } else {
        list
    };
    Ok((list, tainted))
}

/// The items of a `with_<lookup>` loop: the terms, rendered, handed to the lookup as its
/// arguments, and what it returns taken as a list. Read from ansible-core 2.19.12's
/// `TaskExecutor._get_loop_items`: a string term is resolved and anything that is not a list
/// becomes a list of one, and the lookup runs with `wantlist=True`.
///
/// `with_first_found` alone drops a term inside the list that renders undefined: its plugin does
/// that when it is invoked as `with_` (`_recurse_terms(terms, omit_undefined=True)`). This is what
/// lets the `raspberrypi` role's task, which names `detected_distribution` before any host has
/// set it, reach its `when` on a host that is not a Pi.
///
/// The second half of the answer is whether the terms read a managed host. A path the lookup
/// found is controller content, but one built from a fact is not the playbook's own choice, so
/// the items of such a loop are bound as data, like those of a `loop:` over a registered list.
pub(super) fn with_lookup_items(
    templar: &Templar,
    lookup: &str,
    module: &str,
    raw: &Value,
    vars: &HostVars,
) -> Result<(Vec<Value>, bool), TemplateError> {
    let mut tainted = false;
    // A term written as one string is resolved before the lookup sees it (`resolve_to_container`
    // in `_get_loop_items`), so an undefined variable there is an error even for `first_found`.
    let omit_undefined = lookup == "first_found" && !raw.is_string();
    let terms = render_terms(templar, raw, vars, omit_undefined, &mut tainted)?;
    let terms = match terms {
        Some(Value::Array(terms)) => terms,
        Some(term) => vec![term],
        None => Vec::new(),
    };
    // Bound in a copy only this call reads, under a name no playbook writes: the terms reach the
    // lookup as values, never as text to compile.
    const TERMS: &str = "__volant_with_terms";
    let mut scope = vars.clone();
    scope.insert(TERMS.into(), Value::Array(terms));
    // What `first_found.py` does when it runs for a `with_` loop: it searches a subdirectory of
    // each entry first, chosen by the task's action as written - `templates` when the name holds
    // `template`, `vars` when it holds `var`, `files` otherwise - and a plain `lookup()` never does.
    // `template/lookups.rs` reads the same name.
    if lookup == "first_found" {
        let subdir = ["template", "var", "file"]
            .into_iter()
            .find(|word| module.contains(word))
            .unwrap_or("file");
        scope.insert(
            "volant::first_found_subdir".into(),
            Value::String(format!("{subdir}s")),
        );
    }
    let (found, from_lookup) = templar.evaluate_tainted(
        &format!("lookup({lookup:?}, *{TERMS}, wantlist=True)"),
        &scope,
    )?;
    // What a lookup read at run time is data, as a host's answer is: its items are never rendered.
    tainted |= from_lookup;
    let list = match found {
        Value::Array(list) => list,
        one => vec![one],
    };
    Ok((list, tainted))
}

/// Every string of `raw` rendered on its own, so an undefined one can be left out rather than
/// fail the whole list when `omit_undefined` says so. `None` is a value left out.
pub(super) fn render_terms(
    templar: &Templar,
    raw: &Value,
    vars: &HostVars,
    omit_undefined: bool,
    tainted: &mut bool,
) -> Result<Option<Value>, TemplateError> {
    Ok(Some(match raw {
        Value::String(_) => match templar.render_value_tainted(raw, vars) {
            Ok((value, from_host)) => {
                *tainted |= from_host;
                value
            }
            Err(e) if omit_undefined && e.is_undefined() => return Ok(None),
            Err(e) => return Err(e),
        },
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                out.extend(render_terms(templar, item, vars, omit_undefined, tainted)?);
            }
            Value::Array(out)
        }
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                if let Some(value) = render_terms(templar, value, vars, omit_undefined, tainted)? {
                    out.insert(key.clone(), value);
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }))
}

pub(super) fn flatten_once(list: Vec<Value>) -> Vec<Value> {
    list.into_iter()
        .flat_map(|v| match v {
            Value::Array(inner) => inner,
            other => vec![other],
        })
        .collect()
}
