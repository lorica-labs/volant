// SPDX-License-Identifier: GPL-3.0-or-later
//! Parsing a block and a task.

use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use saphyr::{Scalar, Yaml};
use serde_json::{Map, Value};
use volant_protocol::modules::{import_module, include_module, is_known, short_name};

use crate::keywords::{block_keyword, handler_keyword, loop_control_keyword, task_keyword};
use crate::yaml::{field, to_json};

use super::deferred::{Deferred, boolean_or_template, seconds_or_template};
use super::fields::{
    condition, conditions, debugger, mapping, module_args, names, tags, throttle, timeout,
};
use super::keywords::{META, action_statement, connect_keywords, delegation, escalation, flag};
use super::load::task_list;
use super::model::{Apply, Block, PlayTask};
use super::origin::{entry_origins, key_origin, located, task_origin, value_origin};
use super::play::{collections, environment, module_defaults};
/// A block, with its three sections parsed recursively and its inherited keywords read into a
/// module-less [`PlayTask`].
///
/// Every key is checked against the reference's own Block attribute list, so a key that belongs
/// on a task and not on a block - `loop`, `register`, `until` - is refused in the reference's
/// words instead of being taken for a module or quietly ignored.
pub(super) fn parse_block(yaml: &Yaml) -> anyhow::Result<Block> {
    if yaml.as_mapping().is_none() {
        bail!("a block must be a mapping");
    }
    let name = field(yaml, "name")
        .and_then(Yaml::as_str)
        .unwrap_or("block")
        .to_string();
    let keywords = block_keywords(yaml, &name)?;
    Ok(Block {
        body: task_list(field(yaml, "block"), "block")?,
        rescue: task_list(field(yaml, "rescue"), "rescue")?,
        always: task_list(field(yaml, "always"), "always")?,
        keywords,
    })
}

/// The keywords a block's tasks inherit, read into a module-less [`PlayTask`], with every key
/// checked against the reference's own Block attribute list.
fn block_keywords(yaml: &Yaml, name: &str) -> anyhow::Result<PlayTask> {
    let map = yaml
        .as_mapping()
        .ok_or_else(|| anyhow!("a block must be a mapping"))?;
    for (key, _) in map {
        let key = key
            .as_str()
            .ok_or_else(|| anyhow!("block '{name}': keys must be strings"))?;
        // The reference's own words, measured: a module key next to `block:` reads as an
        // attribute the block does not have, `'debug' is not a valid attribute for a Block`.
        if block_keyword(key).is_none() {
            bail!("'{key}' is not a valid attribute for a Block");
        }
    }
    let context = format!("block '{name}': ");
    let mut deferred = Deferred::default();
    let (r#become, become_user) = escalation(yaml, &context, &mut deferred)?;
    let (delegate_facts, delegate_to) = delegation(yaml, &context, &mut deferred)?;
    let timeout = timeout(yaml, &context, &mut deferred)?;
    let no_log = boolean_or_template(yaml, "no_log", &mut deferred.no_log)?;
    let run_once = boolean_or_template(yaml, "run_once", &mut deferred.run_once)?;
    let any_errors_fatal =
        boolean_or_template(yaml, "any_errors_fatal", &mut deferred.any_errors_fatal)?;
    let ignore_unreachable =
        boolean_or_template(yaml, "ignore_unreachable", &mut deferred.ignore_unreachable)?;
    Ok(PlayTask {
        name: name.to_string(),
        tags: tags(yaml, &context)?,
        ignore_errors: flag(yaml, "ignore_errors")?,
        timeout,
        throttle: throttle(yaml)?,
        debugger: debugger(yaml, &context)?,
        vars: mapping(yaml, "vars", &context)?,
        when: conditions(yaml, "when", &context)?,
        r#become,
        become_user,
        connect: connect_keywords(yaml, &context)?,
        notify: names(yaml, "notify", &context)?,
        no_log,
        environment: environment(yaml, &context)?,
        module_defaults: module_defaults(yaml, &context)?,
        collections: collections(yaml, &context)?,
        check_mode: flag(yaml, "check_mode")?,
        diff: flag(yaml, "diff")?,
        run_once,
        any_errors_fatal,
        ignore_unreachable,
        delegate_facts,
        delegate_to,
        deferred,
        ..PlayTask::empty()
    })
}

/// The `apply:` of an `include_tasks` or `include_role` statement.
///
/// Measured on ansible-core 2.19.12: the reference loads it as a block, so it takes every block
/// keyword and nothing else, and an empty mapping is the same as none. A value that is not a
/// mapping stops the load at exit 4. A key that is not a block keyword stops the run at exit 4
/// when the statement runs, which is why it is kept as a sentence in [`Apply::Refused`].
fn apply_of(module: &str, value: &Yaml, yaml: &Yaml) -> anyhow::Result<Option<Apply>> {
    if !matches!(short_name(module), "include_tasks" | "include_role") {
        return Ok(None);
    }
    let node =
        field(value, "apply").or_else(|| field(yaml, "args").and_then(|a| field(a, "apply")));
    let Some(node) = node else {
        return Ok(None);
    };
    let Yaml::Mapping(map) = node else {
        let kind = match node {
            Yaml::Sequence(_) => "a list",
            Yaml::Value(Scalar::String(_)) => "a string",
            Yaml::Value(Scalar::Null) => "null",
            Yaml::Value(Scalar::Boolean(_)) => "a boolean",
            _ => "a number",
        };
        bail!("Expected a dict for apply but got {kind} instead");
    };
    if map.is_empty() {
        return Ok(None);
    }
    Ok(Some(match block_keywords(node, "apply") {
        Ok(keywords) => Apply::Keywords(Arc::new(keywords)),
        Err(err) => Apply::Refused(format!("{err:#}").into()),
    }))
}

/// One task. `handler` says whether the mapping is a handler, which is the one shape that may
/// carry `listen`: read here so the key is not mistaken for a module, and read from its own table
/// so the row can be flipped like any other.
#[expect(clippy::too_many_lines)]
pub(super) fn parse_task(yaml: &Yaml, handler: bool) -> anyhow::Result<PlayTask> {
    let map = yaml
        .as_mapping()
        .ok_or_else(|| anyhow!("a task must be a mapping"))?;
    let name = field(yaml, "name")
        .and_then(Yaml::as_str)
        .map(str::to_string);
    let label = name.clone().unwrap_or_else(|| "unnamed".to_string());
    let mut module: Option<(String, &Yaml)> = None;
    // `action:` or `local_action:`, the spellings that name the module in their value.
    let mut statement: Option<(&'static str, &Yaml)> = None;
    for (key, value) in map {
        let key = key
            .as_str()
            .ok_or_else(|| anyhow!("task '{label}': keys must be strings"))?;
        if handler && handler_keyword(key).is_some() {
            continue;
        }
        match task_keyword(key) {
            Some(k) if matches!(k.name, "action" | "local_action") => {
                if statement.is_some() {
                    bail!("action and local_action are mutually exclusive");
                }
                statement = Some((k.name, value));
            }
            Some(_) => {}
            // A `with_` key is a loop or an invalid attribute, never an action: the reference
            // reads it below and the pre-flight settles which.
            None if key.starts_with("with_") => {}
            // Whatever is left is the module, which is how the reference reads a task too: a
            // key it does not know is an action, and a second one is the conflict below rather
            // than an unknown-keyword error.
            None => {
                if let Some((first, _)) = &module {
                    return Err(located(
                        format!("conflicting action statements: {first}, {key}"),
                        map,
                    ));
                }
                module = Some((key.to_string(), value));
            }
        }
    }
    // `loop_control` is a mapping, so it has a grammar of its own. The reference refuses a
    // sub-key it does not know at load - measured, exit 4, `'nosuch' is not a valid attribute
    // for a LoopControl`.
    let mut loop_var = "item".to_string();
    let mut loop_label = None;
    let mut index_var = None;
    let mut loop_extended = None;
    let mut loop_allitems = None;
    let mut loop_pause = None;
    let mut break_when = Vec::new();
    if let Some(control) = field(yaml, "loop_control") {
        let control = control
            .as_mapping()
            .ok_or_else(|| anyhow!("task '{label}': 'loop_control' must be a mapping"))?;
        for (key, value) in control {
            let key = key
                .as_str()
                .ok_or_else(|| anyhow!("task '{label}': 'loop_control' keys must be strings"))?;
            let Some(k) = loop_control_keyword(key) else {
                bail!("'{key}' is not a valid attribute for a LoopControl");
            };
            let name = || {
                value.as_str().map(str::to_string).ok_or_else(|| {
                    anyhow!("task '{label}': 'loop_control.{}' must be a name", k.name)
                })
            };
            match k.name {
                "loop_var" => loop_var = name()?,
                "label" => loop_label = Some(name()?),
                "index_var" => index_var = Some(name()?),
                "extended" => loop_extended = Some(to_json(value)?),
                "extended_allitems" => loop_allitems = Some(to_json(value)?),
                "pause" => loop_pause = Some(to_json(value)?),
                "break_when" => {
                    let context = format!("task '{label}': ");
                    break_when = match value {
                        Yaml::Sequence(items) => items
                            .iter()
                            .map(|n| condition(n, "break_when", &context))
                            .collect::<anyhow::Result<_>>()?,
                        node => vec![condition(node, "break_when", &context)?],
                    };
                }
                other => unreachable!("loop_control sub-key '{other}' has no reader"),
            }
        }
    }
    // The statement's value is the action: a string is `module args`, a mapping names the module
    // under `module:` and holds its arguments beside it. It is read after the whole mapping so a
    // conflict is reported the same whichever key came first.
    let rewritten;
    let mut local = false;
    let (mut shown, mut deferred_module) = (None, None);
    let source = statement.map(|(_, node)| node);
    let module = match statement {
        Some((word, node)) => {
            let (mut name, args) = action_statement(node)?;
            (shown, deferred_module) = super::splat::read_module(&mut name, &args)?;
            if let Some((other, _)) = &module {
                return Err(located(
                    format!("conflicting action statements: {name}, {other}"),
                    map,
                ));
            }
            local = word == "local_action";
            rewritten = args;
            Some((name, &rewritten))
        }
        None => module,
    };
    let Some((module, value)) = module else {
        bail!("no module/action detected in task.");
    };
    // Arguments are read only for a module this release can run. The reference resolves the
    // action before it looks at what was handed to it, so a name it cannot resolve is refused
    // for its name and never for the shape of arguments nobody promised to read; the pre-flight
    // raises that refusal a moment later. Reading them anyway made `nosuchmodule: echo a`
    // complain about `key=value`, which sends the operator to the wrong line.
    let mut args = if short_name(&module) == META {
        // `meta` is not a module the agent runs, so its argument is read here and nowhere else:
        // the compiler turns the task into a step of its own and the pre-flight decides whether
        // this release honours the action.
        let action = value
            .as_str()
            .ok_or_else(|| anyhow!("task '{label}': 'meta' takes an action name"))?;
        let mut args = Map::new();
        args.insert("_raw_params".into(), Value::String(action.to_string()));
        args
    } else if is_known(&module)
        || crate::python::is_python_module(&module)
        || crate::action_plugins::kind(&module).is_some()
        || import_module(&module).is_some()
        || include_module(&module).is_some()
    {
        // The import and include statements are read here for the same reason `meta` is: the
        // compiler needs what they name, or the coordinator does once the play is running, and
        // nothing else ever will, because no step is left for a host.
        module_args(&module, value).with_context(|| format!("task '{label}'"))?
    } else {
        Map::new()
    };
    if let Some(Yaml::Mapping(extra)) = field(yaml, "args") {
        for (k, v) in extra {
            let k = k
                .as_str()
                .ok_or_else(|| anyhow!("task '{label}': 'args' keys must be strings"))?;
            // The reference lets the module's own string or mapping override `args`, so a key
            // the action line already gave stays as it is.
            if !args.contains_key(k) {
                args.insert(k.to_string(), to_json(v)?);
            }
        }
    }
    let context = format!("task '{label}': ");
    let mut deferred = Deferred::with_module(deferred_module);
    let ignore_errors = flag(yaml, "ignore_errors")?;
    let timeout = timeout(yaml, &context, &mut deferred)?;
    let async_val =
        seconds_or_template(yaml, "async", &context, &mut deferred.async_val)?.unwrap_or(0);
    let poll = seconds_or_template(yaml, "poll", &context, &mut deferred.poll)?;
    let throttle = throttle(yaml)?;
    let debugger = debugger(yaml, &context)?;
    let vars = mapping(yaml, "vars", &context)?;
    let when = conditions(yaml, "when", &context)?;
    let changed_when = conditions(yaml, "changed_when", &context)?;
    let failed_when = conditions(yaml, "failed_when", &context)?;
    let register = match field(yaml, "register") {
        None => None,
        Some(Yaml::Value(Scalar::String(s))) => Some(s.to_string()),
        // Measured on ansible-core 2.19.12, exit 4 before the first play: the refusal at the
        // value, with the rule for variable names under its cause.
        Some(node) => {
            let value = to_json(node).unwrap_or_default();
            let cause = format!(
                "Invalid variable name '{}' of type '{}'.",
                crate::playbook::python_repr(&value),
                crate::executor::python_type(&value)
            );
            match value_origin(node) {
                Some(origin) => {
                    return Err(super::origin::chained_refusal(
                        &format!("Invalid 'register' specified: {cause}"),
                        "Invalid 'register' specified.",
                        &cause,
                        Some(
                            "Variable names must be strings starting with a letter or underscore character, and contain only letters, numbers and underscores.",
                        ),
                        origin,
                    ));
                }
                None => bail!("task '{label}': Invalid 'register' specified: {cause}"),
            }
        }
    };
    // `loop` and every `with_<lookup>`, read in the order the reference's `preprocess_data` reads
    // them. A `with_` key is a loop whatever follows the prefix: whether that is a lookup plugin
    // is asked of the controller's ansible-core before the first connection, because collections
    // and a `lookup_plugins/` directory add names no table here can hold.
    let mut loop_items = None;
    let mut loop_node = None;
    let mut loop_with: Option<String> = None;
    for (key, value) in map {
        let Some(key) = key.as_str() else { continue };
        if key == "loop" {
            loop_items = Some(to_json(value).with_context(|| format!("task '{label}': {key}"))?);
            loop_node = Some(value);
        } else if let Some(lookup) = key.strip_prefix("with_") {
            if loop_items.is_some() || loop_with.is_some() {
                return Err(crate::stats::Refusal::at(
                    1,
                    format!("duplicate loop in task: {lookup}"),
                ));
            }
            if matches!(value, Yaml::Value(Scalar::Null)) {
                return Err(crate::stats::Refusal::at(
                    1,
                    format!("you must specify a value when using {key}"),
                ));
            }
            loop_items = Some(to_json(value).with_context(|| format!("task '{label}': {key}"))?);
            loop_node = Some(value);
            loop_with = Some(lookup.to_string());
        }
    }
    // `with_items` flattens one level, and so does the `items` lookup under any of its names.
    let with_items = loop_with
        .as_deref()
        .is_some_and(|lookup| crate::collection::canonical(lookup) == "items");
    if with_items {
        loop_with = None;
    }
    let (r#become, become_user) = escalation(yaml, &context, &mut deferred)?;
    let apply = apply_of(&module, value, yaml)?;
    let (delegate_facts, mut delegate_to) = delegation(yaml, &context, &mut deferred)?;
    let no_log = boolean_or_template(yaml, "no_log", &mut deferred.no_log)?;
    let run_once = boolean_or_template(yaml, "run_once", &mut deferred.run_once)?;
    let any_errors_fatal =
        boolean_or_template(yaml, "any_errors_fatal", &mut deferred.any_errors_fatal)?;
    let ignore_unreachable =
        boolean_or_template(yaml, "ignore_unreachable", &mut deferred.ignore_unreachable)?;
    // `local_action` runs on the controller whatever `delegate_to` says: measured, the line reads
    // `h1 -> localhost` with both written.
    if local {
        delegate_to = Some("localhost".to_string());
    }
    let mut value_origins = std::collections::BTreeMap::new();
    let var = field(value, "var").or_else(|| field(yaml, "args").and_then(|a| field(a, "var")));
    for (key, node) in [
        ("until", field(yaml, "until")),
        ("retries", field(yaml, "retries")),
        ("delay", field(yaml, "delay")),
        ("timeout", field(yaml, "timeout")),
        ("notify", field(yaml, "notify")),
        ("check_mode", field(yaml, "check_mode")),
        ("diff", field(yaml, "diff")),
        ("ignore_errors", field(yaml, "ignore_errors")),
        ("loop", loop_node),
        ("var", var),
        // A list of conditions is not placed: the reference points at the one that failed.
        (
            "when",
            field(yaml, "when").filter(|n| !matches!(n, Yaml::Sequence(_))),
        ),
    ] {
        if let Some(origin) = node.and_then(value_origin) {
            value_origins.insert(key.to_string(), origin);
        }
    }
    if let Some(origin) = key_origin(source.unwrap_or(value)) {
        value_origins.insert("action".to_string(), origin);
    }
    // Where the `action:` statement's own value starts, which the reference points the cause of a
    // name it cannot resolve at.
    if let Some(origin) = source.and_then(value_origin) {
        value_origins.insert("action.value".to_string(), origin);
    }
    // A free-form action line is the module's `_raw_params`, at the line's value.
    if !matches!(source.unwrap_or(value), Yaml::Mapping(_))
        && let Some(origin) = value_origin(source.unwrap_or(value))
    {
        value_origins.insert("args._raw_params".to_string(), origin);
    }
    if let Some(control) = field(yaml, "loop_control") {
        entry_origins(control, "loop_control", &mut value_origins);
    }
    for node in [Some(source.unwrap_or(value)), field(yaml, "args")]
        .into_iter()
        .flatten()
    {
        entry_origins(node, "args", &mut value_origins);
    }
    // An argument written in a `key=value` line is placed where the line starts, as the
    // reference places it.
    if let Some(line) = value_origins.get("args._raw_params").cloned() {
        for key in args.keys().filter(|k| *k != "_raw_params") {
            let at = value_origins.entry(format!("args.{key}"));
            at.or_insert_with(|| line.clone());
        }
        if args.contains_key("var") {
            value_origins.entry("var".to_string()).or_insert(line);
        }
    }
    let mut task = PlayTask {
        named: name.is_some(),
        implicit: false,
        name: name.unwrap_or_else(|| shown.unwrap_or_else(|| module.clone())),
        module,
        args,
        splats: Vec::new(),
        tags: tags(yaml, &context)?,
        ignore_errors,
        timeout,
        async_val,
        poll,
        throttle,
        debugger,
        vars,
        when,
        loop_items,
        with_items,
        loop_with,
        loop_var,
        loop_label,
        index_var,
        loop_extended,
        loop_allitems,
        loop_pause,
        break_when,
        register,
        changed_when,
        failed_when,
        r#become,
        become_user,
        connect: connect_keywords(yaml, &context)?,
        notify: names(yaml, "notify", &context)?,
        until: conditions(yaml, "until", &context)?,
        retries: field(yaml, "retries").map(to_json).transpose()?,
        delay: field(yaml, "delay").map(to_json).transpose()?,
        no_log,
        environment: environment(yaml, &context)?,
        module_defaults: module_defaults(yaml, &context)?,
        collections: collections(yaml, &context)?,
        check_mode: flag(yaml, "check_mode")?,
        diff: flag(yaml, "diff")?,
        run_once,
        any_errors_fatal,
        ignore_unreachable,
        delegate_facts,
        delegate_to,
        deferred,
        origin: task_origin(map),
        apply,
        value_origins,
    };
    super::splat::read_splats(&mut task, yaml, source);
    Ok(task)
}
