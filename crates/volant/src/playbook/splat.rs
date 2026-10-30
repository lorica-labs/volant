// SPDX-License-Identifier: GPL-3.0-or-later
//! A module's arguments given as one template, and a module named by one.

use saphyr::{Scalar, Yaml};
use serde_json::{Map, Value};

use crate::yaml::field;

use super::PlayTask;
use super::keywords::{possibly_all_template, stray_raw_params};
use super::origin::value_origin;

/// The argument splat, rendered when the task runs: a template given as `args:` (on the task,
/// then inside an `action:` mapping), then one given as the action's own value, which a module
/// that reads no raw parameters takes as its arguments. Measured on ansible-core 2.19.12, the
/// action's template wins over `args:`'s, and an `args:` mapping and the action's own
/// `key=value` words win over both.
pub(super) fn read_splats(task: &mut PlayTask, yaml: &Yaml<'_>, action: Option<&Yaml<'_>>) {
    for node in [field(yaml, "args"), action.and_then(|a| field(a, "args"))]
        .into_iter()
        .flatten()
    {
        if let Yaml::Value(Scalar::String(text)) = node
            && possibly_all_template(text)
        {
            task.splats.push((text.to_string(), value_origin(node)));
        }
    }
    if let Some(raw) = stray_raw_params(task)
        .filter(|raw| possibly_all_template(raw))
        .map(str::to_string)
    {
        task.args.remove("_raw_params");
        let at = task.value_origins.get("args._raw_params").cloned();
        task.splats.push((raw, at));
    }
}

/// A module named by a template, rendered when the task is read, with no variables; the task
/// keeps the template, returned, as its name. Measured on ansible-core 2.19.12, `action:
/// {module: "{{ 'echo' }}"}` runs `echo` under the banner `TASK [{{ 'echo' }}]`.
pub(super) fn rendered_module(name: &mut String) -> Option<String> {
    if !crate::template::Templar::is_template(name) {
        return None;
    }
    let templar = crate::template::Templar::new(std::path::PathBuf::from("."));
    match templar.render(name, &Map::new()) {
        Ok(Value::String(rendered)) => Some(std::mem::replace(name, rendered)),
        _ => None,
    }
}
