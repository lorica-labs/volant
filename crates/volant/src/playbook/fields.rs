// SPDX-License-Identifier: GPL-3.0-or-later
//! Keyword values read out of a task or a play: tags, timeouts, conditions, module arguments.

use anyhow::{Context, anyhow, bail};
use saphyr::{Scalar, Yaml};
use serde_json::{Map, Value};
use volant_protocol::modules::short_name;

use crate::keywords::BLOCK_SECTIONS;
use crate::yaml::{field, to_json};

use super::keywords::is_free_form;
use super::model::{HandlerTask, Throttle};
use super::origin::{Origin, value_origin};
use super::task::parse_task;
/// `tags`, wherever they are written: on a play, on a block, on a role entry or on a task.
///
/// Read the way the reference reads them, measured on ansible-core 2.19.12: a string is split
/// on commas and each piece trimmed, so `tags: a, b` is two tags; a list is taken entry by
/// entry, and a nested list is flattened. Anything else is refused in the reference's own
/// words. The result is sorted and deduplicated, because every use of it is a set: the two
/// listings sort what they print, and the selection asks whether a name is in the list.
pub(super) fn tags(yaml: &Yaml, context: &str) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    collect_tags(field(yaml, "tags"), context, &mut out)?;
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

fn collect_tags(node: Option<&Yaml>, context: &str, out: &mut Vec<String>) -> anyhow::Result<()> {
    match node {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(()),
        Some(Yaml::Value(Scalar::String(s))) => {
            out.extend(
                s.split(',')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(str::to_string),
            );
            Ok(())
        }
        // A bare number or boolean is a tag too: the reference templates the value and keeps
        // whatever comes back, so `tags: 1` names the tag `1` rather than refusing the load.
        Some(Yaml::Value(Scalar::Integer(i))) => {
            out.push(i.to_string());
            Ok(())
        }
        Some(Yaml::Value(Scalar::Boolean(b))) => {
            out.push(if *b { "True" } else { "False" }.to_string());
            Ok(())
        }
        Some(Yaml::Sequence(items)) => {
            for item in items {
                collect_tags(Some(item), context, out)?;
            }
            Ok(())
        }
        Some(_) => bail!("{context}tags must be specified as a list"),
    }
}

/// `timeout`, on a task or on a block.
pub(super) fn timeout(yaml: &Yaml, context: &str) -> anyhow::Result<Option<u64>> {
    seconds(yaml, "timeout", context)
}

/// A keyword counting seconds: `timeout`, `async`, `poll`.
pub(super) fn seconds(yaml: &Yaml, key: &str, context: &str) -> anyhow::Result<Option<u64>> {
    match field(yaml, key) {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(None),
        Some(Yaml::Value(Scalar::Integer(i))) if *i >= 0 => Ok(Some(*i as u64)),
        Some(other) => {
            bail!("{context}'{key}' must be a non-negative integer, found {other:?}")
        }
    }
}

/// `throttle`, wherever it is written, kept as written: the reference renders and converts it
/// when the task runs, so a word, a template or a float is no error of the file.
pub(super) fn throttle(yaml: &Yaml) -> anyhow::Result<Option<Throttle>> {
    match field(yaml, "throttle") {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(None),
        Some(node) => Ok(Some(Throttle {
            value: to_json(node)?,
            origin: value_origin(node),
        })),
    }
}

/// What a `debugger` keyword says, which is when a result opens the debugger prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Debugger {
    Always,
    Never,
    OnFailed,
    OnUnreachable,
    OnSkipped,
    /// A name the reference does not know, kept with where it was written: like the reference,
    /// the task it ends up on fails when it runs, and the playbook still loads.
    Invalid(String, Option<Origin>),
}

/// `debugger`, wherever it is written. A name the reference does not know is kept for the run
/// to fail on: measured, the reference loads the playbook and fails the task after its banner.
pub(super) fn debugger(yaml: &Yaml, context: &str) -> anyhow::Result<Option<Debugger>> {
    match field(yaml, "debugger") {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(None),
        Some(node @ Yaml::Value(Scalar::String(s))) => Ok(Some(match &**s {
            "always" => Debugger::Always,
            "never" => Debugger::Never,
            "on_failed" => Debugger::OnFailed,
            "on_unreachable" => Debugger::OnUnreachable,
            "on_skipped" => Debugger::OnSkipped,
            other => Debugger::Invalid(other.to_string(), value_origin(node)),
        })),
        Some(other) => bail!("{context}'debugger' must be a name, found {other:?}"),
    }
}

/// A keyword whose value has to be a mapping, kept raw: `vars`, on a task or on a block.
pub(super) fn mapping(yaml: &Yaml, key: &str, context: &str) -> anyhow::Result<Map<String, Value>> {
    match field(yaml, key) {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(Map::new()),
        Some(v) => match to_json(v).with_context(|| format!("{context}'{key}'"))? {
            Value::Object(map) => Ok(map),
            _ => bail!("{context}'{key}' must be a mapping"),
        },
    }
}

/// `notify` and `listen`: one name or a list of them, kept in the order they were written - the
/// order a `notify` is written in decides nothing (handlers run in definition order, measured),
/// but keeping it is what lets a refusal name the first one the playbook wrote.
pub(super) fn names(yaml: &Yaml, key: &str, context: &str) -> anyhow::Result<Vec<String>> {
    one_or_many(yaml, key, |node| {
        node.as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("{context}'{key}' must be a name or a list of names"))
    })
}

/// One value or a list of them, kept in the order they were written. An absent or null field is
/// an empty list, which is what the reference does with `notify:` written with nothing under it.
fn one_or_many<T>(
    yaml: &Yaml,
    key: &str,
    one: impl Fn(&Yaml) -> anyhow::Result<T>,
) -> anyhow::Result<Vec<T>> {
    match field(yaml, key) {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(Vec::new()),
        Some(Yaml::Sequence(items)) => items.iter().map(one).collect(),
        Some(node) => Ok(vec![one(node)?]),
    }
}

/// A play's `handlers:` list, in the order it was written, which is the order they run in.
///
/// A block written in a handlers list is refused rather than flattened: the reference runs one,
/// and this release has nowhere to put a block's `rescue` in a flush. Refusing it by name beats
/// reading the block and running its tasks under rules nobody wrote.
pub(super) fn parse_handlers(node: Option<&Yaml>) -> anyhow::Result<Vec<HandlerTask>> {
    let Some(node) = node else {
        return Ok(Vec::new());
    };
    let items = match node {
        Yaml::Sequence(items) => items,
        Yaml::Value(Scalar::Null) => return Ok(Vec::new()),
        _ => bail!("'handlers' must be a list"),
    };
    let mut handlers = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let context = format!("handler {}: ", i + 1);
        let map = item
            .as_mapping()
            .ok_or_else(|| anyhow!("{context}a handler must be a mapping"))?;
        if map
            .keys()
            .any(|k| k.as_str().is_some_and(|k| BLOCK_SECTIONS.contains(&k)))
        {
            bail!("{context}a block in a handlers list is not supported yet");
        }
        let listen = names(item, "listen", &context)?;
        handlers.push(HandlerTask {
            task: parse_task(item, true).context(context)?,
            listen,
        });
    }
    Ok(handlers)
}

/// `when`, `changed_when`, `failed_when`: one expression or a list of them. A YAML boolean is
/// spelled back as Python would (`True`/`False`) so the expression evaluator reads it.
pub(super) fn conditions(yaml: &Yaml, key: &str, context: &str) -> anyhow::Result<Vec<String>> {
    one_or_many(yaml, key, |node| condition(node, key, context))
}

pub(super) fn condition(node: &Yaml, key: &str, context: &str) -> anyhow::Result<String> {
    match node {
        Yaml::Value(Scalar::String(s)) => Ok(s.to_string()),
        Yaml::Value(Scalar::Boolean(b)) => Ok(if *b { "True" } else { "False" }.to_string()),
        other => bail!(
            "{context}'{key}' must be an expression or a list of expressions, found {other:?}"
        ),
    }
}

pub(super) fn module_args(module: &str, value: &Yaml) -> anyhow::Result<Map<String, Value>> {
    let mut args = Map::new();
    match value {
        // A `set_fact` key YAML reads as a number, a boolean or null keeps that type into the
        // name check, as the reference's does: `{1: x}` fails with `Invalid variable name '1' of
        // type 'int'.`, `{true: x}` with `'True' of type 'bool'`. Written as the template of
        // that value, the key renders back to it where the keys are rendered.
        Yaml::Mapping(entries) if short_name(module) == "set_fact" => {
            for (key, value) in entries {
                let key = match key {
                    Yaml::Value(Scalar::Integer(i)) => format!("{{{{ {i} }}}}"),
                    Yaml::Value(Scalar::FloatingPoint(f)) => {
                        format!("{{{{ {:?} }}}}", f.into_inner())
                    }
                    Yaml::Value(Scalar::Boolean(true)) => "{{ True }}".to_string(),
                    Yaml::Value(Scalar::Boolean(false)) => "{{ False }}".to_string(),
                    Yaml::Value(Scalar::Null) => "{{ None }}".to_string(),
                    other => other
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("mapping keys must be strings"))?
                        .to_string(),
                };
                args.insert(key, to_json(value)?);
            }
        }
        Yaml::Mapping(_) => {
            if let Value::Object(map) = to_json(value)? {
                args = map;
            }
        }
        // `script`'s line is the reference's `parse_kv` with `check_raw`: its options come out,
        // the rest is the script and its arguments.
        Yaml::Value(Scalar::String(s))
            if crate::action_plugins::kind(module)
                == Some(crate::action_plugins::Kind::Bridged("script")) =>
        {
            let (options, raw) = crate::splitter::parse_kv_raw(s)?;
            for (k, v) in options {
                args.insert(k, Value::String(v));
            }
            if let Some(raw) = raw {
                args.insert("_raw_params".into(), Value::String(raw));
            }
        }
        Yaml::Value(Scalar::String(s)) if is_free_form(module) => {
            args.insert("_raw_params".into(), Value::String(s.to_string()));
        }
        // A collection's module, or a name a `library/` may hold, read as the reference's
        // `parse_kv` reads it: a word without `=` is kept in `_raw_params`, and a string that
        // does not split is kept whole, for the pre-flight to refuse once the name resolved.
        // Refused here instead, a role's file for another platform - read only to build the
        // union - would refuse a run no host of which reaches it.
        Yaml::Value(Scalar::String(s)) if crate::python::is_resolved_name(module) => {
            let (options, raw) =
                crate::splitter::parse_kv(s).unwrap_or((Vec::new(), Some(s.to_string())));
            for (k, v) in options {
                args.insert(k, Value::String(v));
            }
            if let Some(raw) = raw {
                args.insert("_raw_params".into(), Value::String(raw));
            }
        }
        // A builtin's: a word that is not `key=value` is kept the same way and fails the task
        // when it runs, unless the action takes raw parameters. Only a string `parse_kv` cannot
        // split stops the load, as it stops the reference's (`failed at splitting arguments`).
        Yaml::Value(Scalar::String(s)) => {
            let (options, raw) = crate::splitter::parse_kv(s)?;
            for (k, v) in options {
                args.insert(k, Value::String(v));
            }
            if let Some(raw) = raw {
                args.insert("_raw_params".into(), Value::String(raw));
            }
        }
        Yaml::Value(Scalar::Null) => {}
        // The class names are the reference's own words for what a task's argument was.
        Yaml::Value(Scalar::Boolean(_)) => {
            bail!("unexpected parameter type in action: <class 'bool'>")
        }
        Yaml::Value(Scalar::Integer(_)) => bail!(
            "unexpected parameter type in action: <class 'ansible.module_utils._internal._datatag._AnsibleTaggedInt'>"
        ),
        Yaml::Value(Scalar::FloatingPoint(_)) => bail!(
            "unexpected parameter type in action: <class 'ansible.module_utils._internal._datatag._AnsibleTaggedFloat'>"
        ),
        Yaml::Sequence(_) => bail!(
            "unexpected parameter type in action: <class 'ansible.module_utils._internal._datatag._AnsibleTaggedList'>"
        ),
        other => bail!("module arguments must be a mapping or a string, found {other:?}"),
    }
    Ok(args)
}
