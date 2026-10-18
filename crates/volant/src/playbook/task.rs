// SPDX-License-Identifier: GPL-3.0-or-later
//! Parsing a block and a task.

use anyhow::{Context, anyhow, bail};
use saphyr::{Scalar, Yaml};
use serde_json::{Map, Value};
use volant_protocol::modules::{import_module, include_module, is_known, short_name};

use crate::keywords::{
    Support, block_keyword, handler_keyword, loop_control_keyword, task_keyword,
};
use crate::yaml::{field, to_json};

use super::fields::{
    condition, conditions, debugger, mapping, module_args, names, seconds, tags, throttle, timeout,
};
use super::keywords::{
    META, action_statement, boolean, connect_keywords, delegation, escalation, flag,
};
use super::load::task_list;
use super::model::{Block, PlayTask};
use super::origin::{key_origin, task_origin, value_origin};
use super::play::{collections, environment, module_defaults};
/// A block, with its three sections parsed recursively and its inherited keywords read into a
/// module-less [`PlayTask`].
///
/// Every key is checked against the reference's own Block attribute list, so a key that belongs
/// on a task and not on a block - `loop`, `register`, `until` - is refused in the reference's
/// words instead of being taken for a module or quietly ignored.
pub(super) fn parse_block(yaml: &Yaml) -> anyhow::Result<Block> {
    let map = yaml
        .as_mapping()
        .ok_or_else(|| anyhow!("a block must be a mapping"))?;
    let name = field(yaml, "name")
        .and_then(Yaml::as_str)
        .unwrap_or("block")
        .to_string();
    let mut unsupported = Vec::new();
    for (key, _) in map {
        let key = key
            .as_str()
            .ok_or_else(|| anyhow!("block '{name}': keys must be strings"))?;
        match block_keyword(key) {
            // The reference's own words, measured: a module key next to `block:` reads as an
            // attribute the block does not have, `'debug' is not a valid attribute for a Block`.
            None => bail!("'{key}' is not a valid attribute for a Block"),
            Some(k) if k.support == Support::Preflight => unsupported.push(k.name),
            Some(_) => {}
        }
    }
    unsupported.sort_unstable();
    let context = format!("block '{name}': ");
    let (r#become, become_user) = escalation(yaml, &context)?;
    let (delegate_facts, delegate_to) = delegation(yaml, &context)?;
    Ok(Block {
        body: task_list(field(yaml, "block"), "block")?,
        rescue: task_list(field(yaml, "rescue"), "rescue")?,
        always: task_list(field(yaml, "always"), "always")?,
        keywords: PlayTask {
            name,
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
            unsupported,
            ..PlayTask::empty()
        },
    })
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
    let mut unsupported = Vec::new();
    for (key, value) in map {
        let key = key
            .as_str()
            .ok_or_else(|| anyhow!("task '{label}': keys must be strings"))?;
        if handler && let Some(k) = handler_keyword(key) {
            if k.support == Support::Preflight {
                unsupported.push(k.name);
            }
            continue;
        }
        match task_keyword(key) {
            Some(k) if k.support == Support::Preflight => unsupported.push(k.name),
            Some(k) if matches!(k.name, "action" | "local_action") => {
                if statement.is_some() {
                    bail!("action and local_action are mutually exclusive");
                }
                statement = Some((k.name, value));
            }
            Some(_) => {}
            // Whatever is left is the module, which is how the reference reads a task too: a
            // key it does not know is an action, and a second one is the conflict below rather
            // than an unknown-keyword error.
            None => {
                if let Some((first, _)) = &module {
                    bail!("conflicting action statements: {first}, {key}");
                }
                module = Some((key.to_string(), value));
            }
        }
    }
    // `loop_control` is a mapping, so it has a grammar of its own. The reference refuses a
    // sub-key it does not know at load - measured, exit 4, `'nosuch' is not a valid attribute
    // for a LoopControl` - and one it knows and this release cannot honour would be parked for
    // the pre-flight like any other keyword.
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
            if k.support == Support::Preflight {
                unsupported.push(k.name);
                continue;
            }
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
    unsupported.sort_unstable();
    // The statement's value is the action: a string is `module args`, a mapping names the module
    // under `module:` and holds its arguments beside it. It is read after the whole mapping so a
    // conflict is reported the same whichever key came first.
    let rewritten;
    let mut local = false;
    let source = statement.map(|(_, node)| node);
    let module = match statement {
        Some((word, node)) => {
            let (name, args) = action_statement(node)?;
            if let Some((other, _)) = &module {
                bail!("conflicting action statements: {name}, {other}");
            }
            local = word == "local_action";
            rewritten = args;
            Some((name, &rewritten))
        }
        None => module,
    };
    // A task whose only content this release cannot execute has no module to find - `block:`
    // is the spelling - so it is loaded empty and refused whole by
    // the pre-flight, instead of being blamed for a module it was never asked to name.
    let (module, value) = match module {
        Some(found) => found,
        None if !unsupported.is_empty() => {
            return Ok(PlayTask {
                name: label.clone(),
                named: name.is_some(),
                module: String::new(),
                tags: tags(yaml, &format!("task '{label}': "))?,
                unsupported,
                ..PlayTask::empty()
            });
        }
        None => bail!("task '{label}': no module given"),
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
            args.insert(k.to_string(), to_json(v)?);
        }
    }
    let context = format!("task '{label}': ");
    let ignore_errors = flag(yaml, "ignore_errors")?;
    let timeout = timeout(yaml, &context)?;
    let async_val = seconds(yaml, "async", &context)?.unwrap_or(0);
    let poll = seconds(yaml, "poll", &context)?;
    let throttle = throttle(yaml)?;
    let debugger = debugger(yaml, &context)?;
    let vars = mapping(yaml, "vars", &context)?;
    let when = conditions(yaml, "when", &context)?;
    let changed_when = conditions(yaml, "changed_when", &context)?;
    let failed_when = conditions(yaml, "failed_when", &context)?;
    let register = match field(yaml, "register") {
        None => None,
        Some(Yaml::Value(Scalar::String(s))) => Some(s.to_string()),
        Some(_) => bail!("task '{label}': 'register' must be a variable name"),
    };
    // Every loop keyword this release runs: `loop`, `with_items`, and each `with_<lookup>` whose
    // row says it runs. The others were parked above for the pre-flight.
    let mut loops = map.iter().filter_map(|(key, value)| {
        let key = key.as_str()?;
        let runs = key == "loop"
            || (key.starts_with("with_")
                && task_keyword(key).is_some_and(|k| k.support != Support::Preflight));
        runs.then_some((key, value))
    });
    let (loop_items, with_items, loop_with) = match (loops.next(), loops.next()) {
        (Some((first, _)), Some((second, _))) => {
            bail!("task '{label}': '{first}' and '{second}' cannot both be given")
        }
        (Some((key, v)), None) => (
            Some(to_json(v).with_context(|| format!("task '{label}': {key}"))?),
            key == "with_items",
            key.strip_prefix("with_")
                .filter(|lookup| *lookup != "items")
                .map(str::to_string),
        ),
        (None, _) => (None, false, None),
    };
    let (r#become, become_user) = escalation(yaml, &context)?;
    let (delegate_facts, mut delegate_to) = delegation(yaml, &context)?;
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
        ("notify", field(yaml, "notify")),
        ("var", var),
    ] {
        if let Some(origin) = node.and_then(value_origin) {
            value_origins.insert(key.to_string(), origin);
        }
    }
    if let Some(origin) = key_origin(source.unwrap_or(value)) {
        value_origins.insert("action".to_string(), origin);
    }
    for node in [Some(source.unwrap_or(value)), field(yaml, "args")]
        .into_iter()
        .flatten()
    {
        if let Yaml::Mapping(map) = node {
            for (k, v) in map {
                if let (Some(k), Some(origin)) = (k.as_str(), value_origin(v)) {
                    value_origins.insert(format!("args.{k}"), origin);
                }
            }
        }
    }
    Ok(PlayTask {
        named: name.is_some(),
        name: name.unwrap_or_else(|| module.clone()),
        module,
        args,
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
        unsupported,
        origin: task_origin(map),
        value_origins,
    })
}
