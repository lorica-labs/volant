// SPDX-License-Identifier: GPL-3.0-or-later
//! Keyword helpers and the module tables the parser consults.

use anyhow::bail;
use saphyr::{Scalar, Yaml};
use serde_json::Value;
use volant_protocol::modules::{import_module, include_module, local, native, short_name};

use crate::yaml::{as_bool, field, to_json};

use super::model::{Connect, PlayTask};
use super::origin::value_origin;
pub(super) fn connect_keywords(yaml: &Yaml, context: &str) -> anyhow::Result<Connect> {
    let text = |key: &str| -> anyhow::Result<Option<String>> {
        match field(yaml, key) {
            None | Some(Yaml::Value(Scalar::Null)) => Ok(None),
            Some(Yaml::Value(Scalar::String(s))) => Ok(Some(s.to_string())),
            Some(Yaml::Value(Scalar::Integer(i))) if key == "port" => Ok(Some(i.to_string())),
            Some(other) => bail!("{context}'{key}' must be a name, found {other:?}"),
        }
    };
    let become_method = text("become_method")?;
    if let Some(method) = &become_method
        && !BECOME_METHODS.contains(&method.as_str())
    {
        // Exit 2, not the 4 the rest of a refused playbook gets: measured, the reference reads
        // this keyword happily and then fails the task that would have used it, which is 2.
        return Err(crate::stats::Refusal::at(
            2,
            format!("{context}become_method '{method}' is not supported yet"),
        ));
    }
    Ok(Connect {
        remote_user: text("remote_user")?,
        port: text("port")?,
        connection: text("connection")?,
        become_method,
        become_exe: text("become_exe")?,
        become_flags: text("become_flags")?,
    })
}

/// The escalation methods this release implements. `doas`, `pbrun` and the rest are refused by
/// name: escalating through a different program is not the same operation, and quietly using
/// `sudo` where the playbook asked for `doas` would run the task under rules the operator never
/// wrote.
pub const BECOME_METHODS: [&str; 2] = ["sudo", "su"];

/// The one name in the module column that is not a module: `meta` asks the engine itself for
/// something, so it is read here and compiled into a step of its own instead of travelling to
/// an agent.
pub const META: &str = "meta";

/// `become` and `become_user`, wherever they appear. `become_user` is read as text; `become`
/// goes through `as_bool`, so `yes`, `on` and `"true"` all work as they do in Ansible. The
/// method and its program are read with the connection keywords, which fold the same way.
pub(super) fn escalation(
    yaml: &Yaml,
    context: &str,
) -> anyhow::Result<(Option<bool>, Option<String>)> {
    let flag = boolean(yaml, "become")?;
    let user = match field(yaml, "become_user") {
        None | Some(Yaml::Value(Scalar::Null)) => None,
        Some(Yaml::Value(Scalar::String(s))) => Some(s.to_string()),
        Some(other) => bail!("{context}'become_user' must be a name, found {other:?}"),
    };
    Ok((flag, user))
}

/// The module an `action:` or `local_action:` value names and the value that module reads its
/// arguments from, as if it had been written under the module's own key.
///
/// Measured on ansible-core 2.19.12: `action: command hostname` and `action: {module: command,
/// cmd: hostname}` both run, the mapping with a deprecation warning, and an `args:` inside the
/// mapping is merged into the arguments beside it. A first word with a `=` in it is a
/// `module=command` spelling the reference cannot resolve, which fails the task rather than the
/// load, and a mapping with no `module:` has "no module/action".
pub(super) fn action_statement<'a>(node: &Yaml<'a>) -> anyhow::Result<(String, Yaml<'a>)> {
    match node {
        Yaml::Value(Scalar::String(text)) => {
            let text = text.trim();
            let (name, rest) = text
                .split_once(char::is_whitespace)
                .map_or((text, ""), |(name, rest)| (name, rest.trim_start()));
            if name.is_empty() {
                bail!("no module/action detected in task.");
            }
            if name.contains('=') {
                return Err(crate::stats::Refusal::at(
                    2,
                    format!("Cannot resolve '{name}' to an action or module."),
                ));
            }
            let args = if rest.is_empty() {
                Yaml::Value(Scalar::Null)
            } else {
                Yaml::Value(Scalar::String(rest.to_string().into()))
            };
            Ok((name.to_string(), args))
        }
        Yaml::Mapping(entries) => {
            warn_action_mapping(node);
            let mut name = None;
            let mut args = saphyr::Mapping::new();
            for (key, value) in entries {
                match (key.as_str(), value) {
                    (Some("module"), Yaml::Value(Scalar::String(module))) => {
                        name = Some(module.trim().to_string());
                    }
                    (Some("args"), Yaml::Mapping(inner)) => {
                        for (k, v) in inner {
                            args.insert(k.clone(), v.clone());
                        }
                    }
                    _ => {
                        args.insert(key.clone(), value.clone());
                    }
                }
            }
            match name.filter(|name| !name.is_empty()) {
                Some(name) => Ok((name, Yaml::Mapping(args))),
                None => bail!("no module/action detected in task."),
            }
        }
        _ => bail!("no module/action detected in task."),
    }
}

/// A `[DEPRECATION WARNING]:` block on stderr, after the line that says how to silence them,
/// printed once per run.
pub(crate) fn print_deprecation(text: &str) {
    static HEADER: std::sync::Once = std::sync::Once::new();
    HEADER.call_once(|| {
        eprintln!(
            "[WARNING]: Deprecation warnings can be disabled by setting `deprecation_warnings=False` in ansible.cfg."
        );
    });
    eprintln!("[DEPRECATION WARNING]: {text}\n");
}

/// The deprecation the reference prints, on stderr, for a mapping given as an action.
fn warn_action_mapping(node: &Yaml) {
    let origin = value_origin(node)
        .map(|origin| format!("{}\n\n", crate::render::source_context(&origin)))
        .unwrap_or_default();
    print_deprecation(&format!(
        "Using a mapping for `action` is deprecated. This feature will be removed from ansible-core version 2.23.\n{origin}Use a string value for `action`."
    ));
}

/// A keyword whose value has to be a boolean, refused in the reference's own words when it is
/// not one, rather than quietly becoming the default.
///
/// Measured on ansible-core 2.19.12: `gather_facts: maybe` refuses the load at exit 4 with
/// `Error processing keyword 'gather_facts': The value 'maybe' could not be converted to
/// 'bool'.`, and `ignore_errors: maybe` carries that same sentence into the task it fails at
/// run time. `become: maybe` does the same. A default silently put in their place runs the
/// playbook under a value nobody wrote and nobody is told about, which is the family of bug the
/// split between loading and refusing exists to close.
///
/// All three are refused here, at load. That is one step earlier than the reference refuses
/// `ignore_errors` and `become`, and the divergence is deliberate: no task has run yet, so
/// there is nothing half-applied to explain. A template is the one exception, for
/// `ignore_errors`, `check_mode` and `diff`: see [`flag`].
pub(super) fn boolean(yaml: &Yaml, key: &str) -> anyhow::Result<Option<bool>> {
    match field(yaml, key) {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(None),
        Some(node) => match as_bool(node) {
            Some(b) => Ok(Some(b)),
            None => bail!(
                "Error processing keyword '{key}': The value {} could not be converted to 'bool'.",
                shown(node)
            ),
        },
    }
}

/// A boolean keyword as written: a literal, or a template rendered when the task runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flag {
    Fixed(bool),
    Template(String),
}

/// `ignore_errors`, `check_mode` and `diff`, which may be templates. Measured on ansible-core 2.19.12:
/// `ignore_errors: "{{ ansible_check_mode }}"`, which `geerlingguy.ntp` and `geerlingguy.docker`
/// write, is rendered when the task runs, so it is kept as written here and rendered by
/// `prepare`. Anything else that is not a boolean is still refused by [`boolean`].
pub(super) fn flag(yaml: &Yaml, key: &str) -> anyhow::Result<Option<Flag>> {
    match field(yaml, key) {
        Some(Yaml::Value(Scalar::String(s))) if crate::template::Templar::is_template(s) => {
            Ok(Some(Flag::Template(s.to_string())))
        }
        _ => Ok(boolean(yaml, key)?.map(Flag::Fixed)),
    }
}

/// `run_once`, `delegate_facts` and `delegate_to`, wherever the three are written: on a play
/// (`run_once` alone, which is all `Play.fattributes` carries), on a block, on a role entry or
/// on a task.
///
/// `delegate_to` is kept as written. It is a template rendered against the task's own variables
/// when the task runs, and rendering it here would resolve it against nothing.
pub(super) fn delegation(
    yaml: &Yaml,
    context: &str,
) -> anyhow::Result<(Option<bool>, Option<String>)> {
    let facts = boolean(yaml, "delegate_facts")?;
    let to = match field(yaml, "delegate_to") {
        None | Some(Yaml::Value(Scalar::Null)) => None,
        Some(Yaml::Value(Scalar::String(s))) => Some(s.to_string()),
        Some(other) => bail!("{context}'delegate_to' must be a host name, found {other:?}"),
    };
    Ok((facts, to))
}

/// A scalar the way that refusal shows it: a string in quotes, a number bare, a sequence or
/// mapping the way ansible-core's own Python repr prints it.
///
/// Measured on ansible-core 2.19.12, values checked in
/// `tests::hosts_may_be_a_list_and_gather_facts_defaults_to_true` below: a float is bare with
/// its decimal kept (`1.0`, `3.5`, never a Rust `OrderedFloat` wrapper), a sequence reads
/// `[1, 2]`, a mapping reads `{'a': 1}`, and `null` reads `None`. Ansible's own message for a
/// `null` host entry is a different sentence entirely ("Hosts list cannot contain values of
/// 'None'", not the generic invalid-value sentence this engine reuses for every shape); this
/// engine keeps the one sentence and only fixes the value rendered into it, which is this
/// function's whole job.
pub(super) fn shown(node: &Yaml) -> String {
    match node {
        Yaml::Value(Scalar::String(s)) => format!("'{s}'"),
        Yaml::Value(Scalar::Integer(i)) => i.to_string(),
        Yaml::Value(Scalar::FloatingPoint(f)) => format!("{:?}", f.into_inner()),
        Yaml::Value(Scalar::Boolean(b)) => if *b { "True" } else { "False" }.to_string(),
        Yaml::Value(Scalar::Null) => "None".to_string(),
        Yaml::Sequence(_) | Yaml::Mapping(_) => {
            to_json(node).map_or_else(|_| format!("{node:?}"), |v| python_repr(&v))
        }
        other => format!("{other:?}"),
    }
}

/// A `serde_json::Value` printed the way Python's own `repr` renders it, since that is what
/// ansible-core's error messages quote a structured value with: strings single-quoted, `null` as
/// `None`, booleans capitalized, everything else bare. Used by [`shown`], for the sequence and
/// mapping shapes `to_json` already knows how to walk, and by the `environment` warning, which
/// quotes the raw stack of layers the same way.
pub(crate) fn python_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => python_str_repr(s),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(python_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(k, v)| format!("{}: {}", python_str_repr(k), python_repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Python's `repr` of a string: single quotes unless the text holds one and no double quote,
/// the chosen quote and the backslash escaped, control characters as `\n`, `\t`, `\r` or `\xNN`.
// Other characters Python finds unprintable (` `, format characters) go through
// as they are; escape them by Unicode category if a label ever needs it.
fn python_str_repr(s: &str) -> String {
    use std::fmt::Write as _;
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() && u32::from(c) < 0x100 => {
                let _ = write!(out, "\\x{:02x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Whether this task asks the engine for something instead of naming a module to run.
pub fn is_meta(task: &PlayTask) -> bool {
    short_name(&task.module) == META
}

/// The `meta` actions that run once for the batch; see [`PlayTask::meta_once`].
pub(crate) const META_ONCE: [&str; 5] = [
    "clear_facts",
    "clear_host_errors",
    "end_batch",
    "end_play",
    "refresh_inventory",
];

/// The collection modules the reference reads a string argument of as one command line: the two
/// of `ansible.windows` in ansible-core 2.19.12's `FREEFORM_ACTIONS`, whose other entries are all
/// builtin names.
pub(crate) const FREE_FORM_COLLECTION_MODULES: &[&str] =
    &["ansible.windows.win_command", "ansible.windows.win_shell"];

/// The builtin actions ansible-core 2.19.12 lets keep a string argument that is not `key=value`
/// (`_BUILTIN_RAW_PARAM_MODULES_SIMPLE` in `parsing/mod_args.py`); its free-form modules are
/// [`is_free_form`]'s. `set_fact: foo` sets a fact named `_raw_params` there, and here.
const BUILTIN_RAW_PARAM_MODULES: [&str; 9] = [
    "include_vars",
    "include_tasks",
    "include_role",
    "import_tasks",
    "import_role",
    "add_host",
    "group_by",
    "set_fact",
    "meta",
];

/// What the loader kept of a module's string argument that is not `key=value`, when the module
/// does not read that string as a command line or as its own raw parameters.
pub(crate) fn stray_raw_params(task: &PlayTask) -> Option<&str> {
    let takes_it = if crate::python::is_resolved_name(&task.module) {
        FREE_FORM_COLLECTION_MODULES.contains(&task.module.as_str())
    } else {
        is_free_form(&task.module) || BUILTIN_RAW_PARAM_MODULES.contains(&short_name(&task.module))
    };
    if takes_it {
        return None;
    }
    task.args
        .get("_raw_params")
        .and_then(Value::as_str)
        .filter(|raw| !raw.is_empty())
}

/// ansible-core's `is_possibly_all_template`: a string that opens with a Jinja override header,
/// or that opens and closes with Jinja delimiters, whichever pair.
pub(crate) fn possibly_all_template(text: &str) -> bool {
    text.starts_with("#jinja2:")
        || (["{%", "{{", "{#"].iter().any(|m| text.starts_with(m))
            && ["%}", "}}", "#}"].iter().any(|m| text.ends_with(m)))
}

/// Whether the module's string form is one command line rather than `key=value` pairs.
///
/// The three registries answer for their own rows, so a module moving between them keeps the
/// form its table gives it. `include_tasks: sub.yml` and `include_vars: v.yml` name a file the
/// way `import_tasks: sub.yml` does, and `include_role` wants `name=` like `import_role`.
pub(super) fn is_free_form(module: &str) -> bool {
    FREE_FORM_COLLECTION_MODULES.contains(&module)
        || native(module).is_some_and(|m| m.free_form)
        || local(module).is_some_and(|m| m.free_form)
        || import_module(module) == Some(true)
        || include_module(module) == Some(true)
        // A command line like `command`'s, in the reference's `MODULE_REQUIRE_ARGS`.
        || crate::action_plugins::kind(module)
            == Some(crate::action_plugins::Kind::Bridged("script"))
}
