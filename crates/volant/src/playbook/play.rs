// SPDX-License-Identifier: GPL-3.0-or-later
//! Parsing one play and its roles, vars prompts and play-level lists.

use std::path::Path;

use anyhow::{Context, anyhow, bail};
use saphyr::{Scalar, Yaml};
use serde_json::{Map, Value};

use crate::keywords::{block_keyword, play_keyword};
use crate::roles::{RoleEntry, RoleFrom};
use crate::yaml::{field, to_json};

use super::fields::{
    conditions, debugger, mapping, names, parse_handlers, tags, throttle, timeout,
};
use super::keywords::{boolean, connect_keywords, delegation, escalation, flag, shown};
use super::load::task_list;
use super::model::{Play, PlayTask};
use super::origin::task_origin;
#[expect(clippy::too_many_lines)]
pub(super) fn parse_play(yaml: &Yaml, dir: &Path) -> anyhow::Result<Play> {
    let map = yaml
        .as_mapping()
        .ok_or_else(|| anyhow!("a play must be a mapping"))?;
    for (key, _) in map {
        let key = key.as_str().unwrap_or_default();
        // The reference's own words, measured: an attribute it does not have refuses the load
        // with exit 4.
        if play_keyword(key).is_none() {
            bail!("'{key}' is not a valid attribute for a Play");
        }
    }
    let host_patterns = match field(yaml, "hosts") {
        Some(Yaml::Value(Scalar::String(s))) => vec![s.to_string()],
        // Every entry, or none: `filter_map` here dropped a non-string entry and ran the play
        // against the rest, which is a host list quietly shorter than the one written. The
        // reference refuses it instead, measured: `Hosts list contains an invalid host value:
        // '3'`, exit 4.
        Some(Yaml::Sequence(items)) => items
            .iter()
            .map(|i| {
                i.as_str().map(str::to_string).ok_or_else(|| {
                    anyhow!("Hosts list contains an invalid host value: '{}'", shown(i))
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?,
        None => bail!("a play needs 'hosts'"),
        Some(other) => bail!("'hosts' must be a string or a list, found {other:?}"),
    };
    let hosts = host_patterns.join(",");
    let name = field(yaml, "name")
        .and_then(Yaml::as_str)
        .unwrap_or(&hosts)
        .to_string();
    let gather_written = boolean(yaml, "gather_facts")?;
    let gather_facts = gather_written.unwrap_or(true);
    let vars = match field(yaml, "vars") {
        None | Some(Yaml::Value(Scalar::Null)) => Map::new(),
        Some(v) => match to_json(v).context("'vars'")? {
            Value::Object(map) => map,
            _ => bail!("'vars' must be a mapping"),
        },
    };
    let vars_files = match field(yaml, "vars_files") {
        None | Some(Yaml::Value(Scalar::Null)) => Vec::new(),
        Some(Yaml::Sequence(items)) => items
            .iter()
            .map(|i| {
                i.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("'vars_files' entries must be strings"))
            })
            .collect::<anyhow::Result<_>>()?,
        Some(Yaml::Value(Scalar::String(s))) => vec![s.to_string()],
        _ => bail!("'vars_files' must be a list of paths"),
    };
    let tasks = task_list(field(yaml, "tasks"), "tasks")?;
    let pre_tasks = task_list(field(yaml, "pre_tasks"), "pre_tasks")?;
    let post_tasks = task_list(field(yaml, "post_tasks"), "post_tasks")?;
    let roles = match field(yaml, "roles") {
        None | Some(Yaml::Value(Scalar::Null)) => Vec::new(),
        Some(node) => parse_role_entries(node).context("'roles'")?,
    };
    let strategy = match field(yaml, "strategy") {
        None | Some(Yaml::Value(Scalar::Null)) => None,
        Some(Yaml::Value(Scalar::String(s))) => Some(s.to_string()),
        Some(other) => bail!("'strategy' must be a name, found {other:?}"),
    };
    let order = match field(yaml, "order") {
        None | Some(Yaml::Value(Scalar::Null)) => None,
        Some(Yaml::Value(Scalar::String(s))) => Some(s.to_string()),
        Some(Yaml::Value(Scalar::Integer(i))) => Some(i.to_string()),
        Some(other) => bail!("'order' must be a name, found {other:?}"),
    };
    // Kept as it was written, template included: the reference renders it when the play starts,
    // against the play's own variables and the run's extra variables.
    let serial = match field(yaml, "serial") {
        None | Some(Yaml::Value(Scalar::Null)) => None,
        Some(v) => Some(to_json(v).context("'serial'")?),
    };
    let (r#become, become_user) = escalation(yaml, "")?;
    Ok(Play {
        origin: task_origin(map),
        name,
        hosts,
        host_patterns,
        tags: tags(yaml, "")?,
        gather_facts,
        gather_written: gather_written.is_some(),
        vars,
        vars_files,
        tasks,
        pre_tasks,
        post_tasks,
        roles,
        handlers: parse_handlers(field(yaml, "handlers"))?,
        force_handlers: boolean(yaml, "force_handlers")?,
        dir: dir.to_path_buf(),
        r#become,
        become_user,
        connect: connect_keywords(yaml, "")?,
        strategy,
        order,
        timeout: timeout(yaml, "")?,
        throttle: throttle(yaml)?,
        debugger: debugger(yaml, "")?,
        serial,
        no_log: boolean(yaml, "no_log")?,
        environment: environment(yaml, "")?,
        module_defaults: module_defaults(yaml, "")?,
        collections: collections(yaml, "")?,
        check_mode: flag(yaml, "check_mode")?,
        diff: flag(yaml, "diff")?,
        run_once: boolean(yaml, "run_once")?,
        any_errors_fatal: boolean(yaml, "any_errors_fatal")?,
        ignore_errors: flag(yaml, "ignore_errors")?,
        ignore_unreachable: boolean(yaml, "ignore_unreachable")?,
        max_fail_percentage: match field(yaml, "max_fail_percentage") {
            None | Some(Yaml::Value(Scalar::Null)) => None,
            Some(v) => Some(to_json(v).context("'max_fail_percentage'")?),
        },
        gather_subset: raw_keyword(yaml, "gather_subset")?,
        gather_timeout: raw_keyword(yaml, "gather_timeout")?,
        fact_path: raw_keyword(yaml, "fact_path")?,
        vars_prompt: vars_prompt(yaml)?,
    })
}

/// A keyword kept as written, `None` when it is absent or null.
fn raw_keyword(yaml: &Yaml, key: &str) -> anyhow::Result<Option<Value>> {
    match field(yaml, key) {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(None),
        Some(v) => Ok(Some(to_json(v).with_context(|| format!("'{key}'"))?)),
    }
}

/// The keys of a `vars_prompt` entry, as the reference's loader lists them.
const PROMPT_KEYS: &[&str] = &[
    "name",
    "prompt",
    "default",
    "private",
    "confirm",
    "encrypt",
    "salt_size",
    "salt",
    "unsafe",
];

/// The play's `vars_prompt`: a list of mappings, or one mapping, each with a `name` and no key
/// the reference does not know.
fn vars_prompt(yaml: &Yaml) -> anyhow::Result<Vec<Map<String, Value>>> {
    let entries = match raw_keyword(yaml, "vars_prompt")? {
        None => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(one) => vec![one],
    };
    entries
        .into_iter()
        .map(|entry| {
            let Value::Object(map) = entry else {
                bail!("Invalid vars_prompt data structure, expected a mapping per entry");
            };
            if !map.contains_key("name") {
                bail!("Invalid vars_prompt data structure, missing 'name' key");
            }
            if let Some(key) = map.keys().find(|k| !PROMPT_KEYS.contains(&k.as_str())) {
                bail!("Invalid vars_prompt data structure, found unsupported key '{key}'");
            }
            Ok(map)
        })
        .collect()
}

/// `environment`, wherever it is written: at most one layer per level, kept raw because its
/// values are templates the task renders against its own variables.
///
/// The value is not required to be a mapping here. Measured on ansible-core 2.19.12: a value
/// that is not one warns and is skipped while the task still runs, so refusing the load would
/// refuse a playbook the reference accepts.
pub(super) fn environment(yaml: &Yaml, context: &str) -> anyhow::Result<Vec<Value>> {
    match field(yaml, "environment") {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(Vec::new()),
        Some(v) => Ok(vec![
            to_json(v).with_context(|| format!("{context}'environment'"))?,
        ]),
    }
}

/// `module_defaults`, wherever it is written: one layer per level, a mapping or a list of
/// mappings. The keys are action or group names and are read as they are; the values are
/// templates, rendered when a task runs.
pub(super) fn module_defaults(yaml: &Yaml, context: &str) -> anyhow::Result<Vec<Value>> {
    let layers = match field(yaml, "module_defaults") {
        None | Some(Yaml::Value(Scalar::Null)) => return Ok(Vec::new()),
        Some(Yaml::Sequence(items)) => items.iter().collect(),
        Some(node) => vec![node],
    };
    layers
        .into_iter()
        .map(|node| match to_json(node).with_context(|| format!("{context}'module_defaults'"))? {
            layer @ Value::Object(_) => Ok(layer),
            _ => bail!(
                "{context}The field 'module_defaults' is supposed to be a dictionary or list of dictionaries, the keys of which must be static action, module, or group names. Only the values may contain templates. For example: {{'ping': \"{{{{ ping_defaults }}}}\"}}"
            ),
        })
        .collect()
}

/// `collections`, wherever it is written: a name or a list of names, searched in order. Absent
/// is `None`, so the layer above is not hidden; a null or an empty list says none.
pub(super) fn collections(yaml: &Yaml, context: &str) -> anyhow::Result<Option<Vec<String>>> {
    match field(yaml, "collections") {
        None => Ok(None),
        Some(_) => names(yaml, "collections", context).map(Some),
    }
}

/// A play's `roles:` list, or the `dependencies:` of a `meta/main.yml`. The two are the same
/// grammar, read by the same code so they cannot drift: measured, a dependency carries its own
/// parameters and runs before the role that depends on it.
pub(crate) fn parse_role_entries(node: &Yaml) -> anyhow::Result<Vec<RoleEntry>> {
    let Yaml::Sequence(items) = node else {
        bail!("'roles' must be a list of roles");
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| parse_role_entry(item).with_context(|| format!("role {}", i + 1)))
        .collect()
}

/// One role entry: a bare name, or a mapping naming the role and carrying keywords, the four
/// `*_from` selectors, and parameters.
///
/// The split between a keyword and a parameter is measured, not guessed: a free key on the entry
/// is a role parameter and beats a `set_fact` on the same name, while `vars:` on the entry does
/// not - so `vars:` is read as the block keyword it is and only the leftovers are parameters.
fn parse_role_entry(yaml: &Yaml) -> anyhow::Result<RoleEntry> {
    if let Some(name) = yaml.as_str() {
        return Ok(RoleEntry {
            name: name.to_string(),
            from: RoleFrom::default(),
            params: Map::new(),
            keywords: PlayTask::empty(),
        });
    }
    let map = yaml
        .as_mapping()
        .ok_or_else(|| anyhow!("a role must be a name or a mapping"))?;
    let name = match field(yaml, "role").or_else(|| field(yaml, "name")) {
        Some(Yaml::Value(Scalar::String(s))) => s.to_string(),
        Some(other) => bail!("'role' must be a name, found {other:?}"),
        None => bail!("a role entry needs 'role'"),
    };
    let mut from = RoleFrom::default();
    let mut params = Map::new();
    for (key, value) in map {
        let key = key
            .as_str()
            .ok_or_else(|| anyhow!("role '{name}': keys must be strings"))?;
        let selector = match key {
            "tasks_from" => Some(&mut from.tasks),
            "vars_from" => Some(&mut from.vars),
            "defaults_from" => Some(&mut from.defaults),
            "handlers_from" => Some(&mut from.handlers),
            _ => None,
        };
        if let Some(slot) = selector {
            *slot = value
                .as_str()
                .ok_or_else(|| anyhow!("role '{name}': '{key}' must be a file name"))?
                .to_string();
            continue;
        }
        if key == "role" || key == "name" {
            continue;
        }
        // A role entry takes the keywords a block takes, which is the list the reference checks
        // one against; anything else is a parameter for the role itself.
        if block_keyword(key).is_none() {
            params.insert(key.to_string(), to_json(value)?);
        }
    }
    let context = format!("role '{name}': ");
    let (r#become, become_user) = escalation(yaml, &context)?;
    let (delegate_facts, delegate_to) = delegation(yaml, &context)?;
    Ok(RoleEntry {
        name,
        from,
        params,
        keywords: PlayTask {
            name: String::new(),
            tags: tags(yaml, &context)?,
            ignore_errors: flag(yaml, "ignore_errors")?,
            timeout: timeout(yaml, &context)?,
            throttle: throttle(yaml)?,
            debugger: debugger(yaml, &context)?,
            vars: mapping(yaml, "vars", &context)?,
            when: conditions(yaml, "when", &context)?,
            r#become,
            become_user,
            connect: connect_keywords(yaml, &context)?,
            notify: names(yaml, "notify", &context)?,
            no_log: boolean(yaml, "no_log")?,
            environment: environment(yaml, &context)?,
            module_defaults: module_defaults(yaml, &context)?,
            collections: collections(yaml, &context)?,
            check_mode: flag(yaml, "check_mode")?,
            diff: flag(yaml, "diff")?,
            run_once: boolean(yaml, "run_once")?,
            any_errors_fatal: boolean(yaml, "any_errors_fatal")?,
            ignore_unreachable: boolean(yaml, "ignore_unreachable")?,
            delegate_facts,
            delegate_to,
            ..PlayTask::empty()
        },
    })
}
