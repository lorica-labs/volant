// SPDX-License-Identifier: GPL-3.0-or-later
//! One task prepared for one host, and the keywords rendered along the way.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};
use volant_protocol::TaskResult;

use crate::action_plugins::Kind;
use crate::playbook::{Flag, PlayTask, python_repr};
use crate::python::ModulePayload;
use crate::render::ansible_json;
use crate::template::{Templar, TemplateError};
use crate::transport::{ConnectionKeywords, Escalation};
use crate::vars::HostVars;

use super::display;
use super::loops::loop_options;

/// The head of a task whose arguments a plugin failed to render, then the action in quotes:
/// `Finalization of task args for 'ansible.builtin.debug' failed: Error while resolving value
/// for 'msg': The lookup plugin 'x' failed: …`, measured on ansible-core 2.19.12.
pub(in crate::executor) const FINALIZING: &str = "Finalization of task args for ";

/// A task rendered for one host: what to do with it.
pub(in crate::executor) enum Prepared {
    /// Every item (or the single non-loop item) had a false `when`: results are ready. No
    /// delegate travels with it: measured on ansible-core 2.19.12, a task a `when` left out
    /// prints `skipping: [h1]` with no arrow, because it never reached the delegate.
    Skipped(Vec<Item>),
    /// `set_fact` or `debug`: run on the controller. A `delegate_to` changes nothing about
    /// where it runs - measured, a delegated `debug` still runs here - so the delegate travels
    /// only as the name the line shows.
    Local(Vec<Item>, Option<String>),
    /// Send to the agent, one `Task` per item, over a link running as this task's escalated
    /// user. Escalation belongs to the task rather than to an item: it decides which agent on
    /// the host the whole task talks to, so every item of a loop shares it.
    ///
    /// The host is the delegate when the task has one, and that is the host the link is opened
    /// to, keyed by, and reused from: measured on ansible-core 2.19.12, a task delegated away
    /// from a host nothing can reach runs perfectly well, so the delegating host's own
    /// connection is never opened for it. The delegate travels with its own effective
    /// variables, which are what its connection is built from; the task's own host needs none
    /// here, since every item already carries them.
    Remote(
        Vec<Item>,
        Option<Box<Escalation>>,
        Option<(String, HostVars)>,
        /// Present when this task is a Python module: the module's half of the payload every
        /// item of it runs with. One per task rather than one per item, because the module is
        /// the task's, not the item's - a loop varies the arguments, never the module.
        ///
        /// The module's half only. The interpreter is the host's, and this is built before a
        /// link to that host exists, so what would go in that field here is a guess; the wire
        /// payload is assembled where the chosen interpreter is in hand. A payload carrying an
        /// interpreter nobody chose is a module running under something other than what was
        /// asked for, so it is made unrepresentable rather than filled in later.
        ///
        /// `None` for every task this release runs today: a module that needs a payload is
        /// refused before the first connection, so nothing reaches here carrying one until the
        /// driver has a union blob to name.
        ///
        /// Boxed because it is three strings and a map against the two pointers of the variant
        /// beside it, and every `Prepared` a run builds - one per task and per host, payload or
        /// not - would otherwise be that size. Clippy's `large_enum_variant` is denied here and
        /// says so; unboxing it is not a simplification.
        Option<Box<ModulePayload>>,
        /// Present when an action plugin this release runs backs the task. Such a task carries
        /// no payload of its own: the sub-tasks the plugin asks for each carry theirs.
        Option<Kind>,
        /// The task's `remote_user`, `port` and `connection`, rendered: they decide the link
        /// together with the host's own variables, and a task that differs ends the batch.
        Box<ConnectionKeywords>,
    ),
}

impl Prepared {
    /// The items of the task, whichever way it goes.
    pub(in crate::executor) fn items(&self) -> &[Item] {
        match self {
            Self::Skipped(items) | Self::Local(items, _) | Self::Remote(items, ..) => items,
        }
    }
}

/// The start of the message a `loop_control` field that fails to render or convert reports.
pub(super) const KEYWORD_ERROR: &str = "Error processing keyword ";

/// The text a task that dies before it runs reports: the reference's `Task failed: ` prefix,
/// except on a keyword error, which it states as it is.
pub(in crate::executor) fn failure_text(err: &TemplateError) -> String {
    if err.0.starts_with(KEYWORD_ERROR)
        || err.0.starts_with(NOTIFY_FAILED)
        || err.0.starts_with("Task failed: ")
    {
        err.0.clone()
    } else {
        format!("Task failed: {}", err.0)
    }
}

/// The pause the loop `items` belong to waits between two of them, if it has one.
pub(in crate::executor) fn pause_of(
    task: &PlayTask,
    items: &[Item],
    templar: &Templar,
) -> Option<std::time::Duration> {
    let first = items.first()?;
    loop_options(task, &first.vars, templar).ok()?.pause
}

/// One loop item (or the whole task when there is no loop), rendered.
pub(in crate::executor) struct Item {
    /// The loop element, present only for loops.
    pub(in crate::executor) element: Option<Value>,
    pub(in crate::executor) label: Option<String>,
    pub(in crate::executor) args: Map<String, Value>,
    /// The arguments whose render read a value that came from a managed host. Only the ones the
    /// engine reads back as source text consult it; everything else treats an argument as the
    /// data it is and ships it to the agent.
    pub(in crate::executor) args_untrusted: BTreeSet<String>,
    /// Variables in force for this item, for `changed_when`, `failed_when` and local modules.
    pub(in crate::executor) vars: HostVars,
    /// The variables the module runs with, this item's layers merged and rendered. Per item
    /// rather than per task because a layer's values are templates and may name the loop
    /// variable.
    pub(in crate::executor) environment: BTreeMap<String, String>,
    /// Set when `when` was false: the skip result to report.
    pub(in crate::executor) skipped: Option<TaskResult>,
    /// The task's `ignore_errors` as rendered for this item, when it was written as a template
    /// and the item was not skipped. The readers fall back to the task's keyword without it.
    pub(in crate::executor) ignore_errors: Option<bool>,
    /// The task's `check_mode` and `diff` as rendered for this item: a keyword written as a
    /// template is rendered per item, and an item a `when` left out renders neither.
    pub(in crate::executor) check_mode: Option<bool>,
    pub(in crate::executor) diff: Option<bool>,
    /// A `set_fact`'s keys as rendered, once: see `run::set_fact_names`. Each entry is the key as
    /// written, the name it rendered to, and whether that name holds a managed host's value.
    pub(in crate::executor) set_fact_names: std::sync::OnceLock<Result<Vec<FactName>, TaskResult>>,
    /// The handler names the task's `notify` rendered to for this item, empty for an item a `when`
    /// left out.
    pub(in crate::executor) notify: Vec<String>,
}

/// One rendered `set_fact` key: the key as written, the name it rendered to, and whether that
/// name holds a managed host's value.
pub(super) type FactName = (String, String, bool);

/// One task's `environment`, layer by layer: the play's, then each block's, then the task's,
/// each rendered against this item's variables and merged over the ones before it.
///
/// Measured on ansible-core 2.19.12: a layer that does not render to a mapping warns on stderr
/// and is skipped while the task still runs, and the layers around it stay in force. Values are
/// what Python's `str()` makes of them - `42` is `"42"`, `true` is `"True"`, `null` is `"None"`.
///
/// The warning goes into `warnings` rather than onto the terminal: `prepare` runs inside a host
/// driver, and every line a task puts on the terminal belongs on the coordinator's queue, where
/// the `no_log` policy can see it.
pub(super) fn environment_for(
    task: &PlayTask,
    vars: &HostVars,
    templar: &Templar,
    warnings: &mut Vec<String>,
) -> Result<BTreeMap<String, String>, TemplateError> {
    let mut out = BTreeMap::new();
    for layer in &task.environment {
        // The reference's own prefix for a keyword that cannot be rendered, measured: an
        // undefined variable in an `environment:` fails the task with
        // `Task failed: Error processing keyword 'environment': ...`.
        let rendered = templar.render_value(layer, vars).map_err(|e| {
            TemplateError(format!("Error processing keyword 'environment': {}", e.0))
        })?;
        let Value::Object(map) = rendered else {
            // The raw stack of layers, not the rendered one. Measured on ansible-core 2.19.12
            // with a `no_log` task whose `environment` is `"{{ secret }}"`: the warning reads
            // `could not parse environment value, skipping: ['{{ secret }}']` - brackets
            // included, because the whole list is reported, and unrendered, so the secret never
            // reaches the line. Reporting the rendered value instead put it there.
            warnings.push(format!(
                "could not parse environment value, skipping: {}",
                python_repr(&Value::Array(task.environment.clone()))
            ));
            continue;
        };
        for (k, v) in map {
            out.insert(k, environment_value(&v));
        }
    }
    Ok(out)
}

/// The handler names one item's `notify` renders to. Measured on ansible-core 2.19.12: a template
/// that fails fails the task with `Error processing keyword 'notify': ...` before the module
/// runs; a rendered list notifies each of its names; a number or boolean is looked up as the text
/// `str()` makes of it, so `{{ 5 }}` finds no handler called `5`. A mapping makes the reference
/// crash, and is refused here.
/// The reference names the variable a keyword template read and found missing, bare. The engine
/// cannot say which one, so a template that is nothing but `{{ name }}` is worded here and any
/// other keeps the engine's sentence.
pub(in crate::executor) fn undefined_name(template: &str, detail: String) -> String {
    let inner = template
        .trim()
        .strip_prefix("{{")
        .and_then(|t| t.strip_suffix("}}"))
        .map(str::trim);
    match inner {
        Some(name)
            if detail.starts_with("The task includes an option with an undefined variable.")
                && !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
        {
            format!("'{name}' is undefined")
        }
        _ => detail,
    }
}

/// How the message of a `notify` that fails to render starts: the reference's `Task failed: ` and
/// the keyword, which [`failure_text`] leaves alone.
pub(in crate::executor) const NOTIFY_FAILED: &str =
    "Task failed: Error processing keyword 'notify': ";

pub(super) fn notify_for(
    task: &PlayTask,
    vars: &HostVars,
    templar: &Templar,
) -> Result<Vec<String>, TemplateError> {
    let keyword = |detail: String| TemplateError(format!("{NOTIFY_FAILED}{detail}"));
    let mut names = Vec::new();
    for entry in &task.notify {
        let rendered = if Templar::is_template(entry) {
            templar
                .render(entry, vars)
                .map_err(|e| keyword(undefined_name(entry, e.0)))?
        } else {
            Value::String(entry.clone())
        };
        let list = match rendered {
            Value::Array(list) => list,
            one => vec![one],
        };
        for name in list {
            names.push(match name {
                Value::String(s) => s,
                Value::Bool(_) | Value::Number(_) | Value::Null => display(&name),
                other => {
                    return Err(keyword(format!(
                        "{} is not a handler name",
                        python_repr(&other)
                    )));
                }
            });
        }
    }
    Ok(names)
}

/// One environment value as the process sees it, which is what `str()` makes of the Python
/// object the reference hands to `os.environ`. Measured: `{B: true, N: null, S: plain}` reaches
/// the shell as `True`, `None` and `plain`, and `{N: 42}` as `42`.
///
/// A list or a mapping is not measured and is written as JSON here rather than as Python's own
/// repr; nothing in ansible's documentation suggests writing one.
pub(super) fn environment_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Null => "None".to_string(),
        other => ansible_json(other),
    }
}

/// The play's `gather_subset` as this host renders it: items must be strings, and a template
/// that renders to a string with a comma is refused, since the module would split it where the
/// reference lists it whole. A failure here is the host's own task failure.
pub(super) fn check_gather_subset(
    raw: Option<&Value>,
    rendered: Option<&Value>,
) -> Result<(), TemplateError> {
    let Some(rendered) = rendered else {
        return Ok(());
    };
    let items = match rendered {
        Value::Array(items) => items.as_slice(),
        one => std::slice::from_ref(one),
    };
    if let Some(bad) = items.iter().find(|item| !item.is_string()) {
        return Err(TemplateError(format!(
            "Keyword 'gather_subset' items must be of type 'str', not '{}'.",
            crate::executor::python_type(bad)
        )));
    }
    if let (Some(Value::String(text)), Value::String(out)) = (raw, rendered)
        && Templar::is_template(text)
        && out.contains(',')
    {
        return Err(TemplateError(
            "a gather_subset template that renders to a string with a comma in it is not supported yet"
                .to_string(),
        ));
    }
    Ok(())
}

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
        Some(Flag::Template(raw)) if !skipped => boolean_keyword(key, raw, vars, templar).map(Some),
        _ => Ok(None),
    }
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
    let rendered = templar.render(raw, vars).map_err(|e| keyword(e.0))?;
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
pub(super) fn strict_boolean(value: &Value) -> Option<bool> {
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
