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

/// The task as one host runs it once its module template rendered to `name`: the statement's
/// arguments are read for that module, and an `args:` mapping fills in what they leave.
pub(crate) fn with_module(task: &PlayTask, name: &str) -> anyhow::Result<PlayTask> {
    let mut task = task.clone();
    let raw = task.deferred.module.take().map(|(_, raw)| raw);
    let mut args = match raw {
        Some(Value::String(text)) => {
            super::fields::module_args(name, &Yaml::Value(Scalar::String(text.into())))?
        }
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    for (key, value) in &task.args {
        args.entry(key.clone()).or_insert_with(|| value.clone());
    }
    task.module = name.to_string();
    task.args = args;
    Ok(task)
}

/// The module an `action:` statement names: rendered now when it needs no variables (the name
/// it was written as comes back, for the banner), or kept with its arguments as written to be
/// rendered for each host when the task runs.
pub(super) fn read_module(
    name: &mut String,
    args: &Yaml<'_>,
) -> anyhow::Result<(Option<String>, Option<super::deferred::DeferredModule>)> {
    let shown = rendered_module(name);
    let deferred = crate::template::Templar::is_template(name)
        .then(|| crate::yaml::to_json(args).map(|raw| (name.clone(), raw)))
        .transpose()?;
    Ok((shown, deferred))
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
