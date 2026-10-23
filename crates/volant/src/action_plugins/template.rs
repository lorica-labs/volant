// SPDX-License-Identifier: GPL-3.0-or-later
//! `template`: a template rendered on the controller, then copied the way `copy` copies.
//!
//! Read off `plugins/action/template.py` of ansible-core 2.19.12: the plugin checks its
//! arguments, finds the template under `templates/`, renders it once with the task's variables
//! and four of its own, and hands the text to the `copy` action with its own options taken out.
//!
//! The template's text is author content read on the controller. What it reads from a managed
//! host lands in the file as text: `Templar::render_file` renders once, and nothing renders the
//! result again.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::{Map, Value};

use super::copy::{CopyOf, copy_bytes, done, failing, local_mode};
use super::files::{check_host_named, not_found, search_paths};
use super::{Context, Plugin};
use crate::executor::as_bool_value;
use crate::template::{FileRender, Syntax, Vars};
use crate::vars::HostVars;
use volant_protocol::TaskResult;

mod encoding;
use encoding::encode;

/// The `Syntax` field one delimiter option sets.
type Field = fn(&mut Syntax) -> &mut String;

/// The six delimiter options with the `Syntax` field each one sets, in the order the reference
/// checks the types of its string options and then the fields of its overrides.
const DELIMITERS: [(&str, Field); 6] = [
    ("block_start_string", |s| &mut s.block_start),
    ("block_end_string", |s| &mut s.block_end),
    ("variable_start_string", |s| &mut s.variable_start),
    ("variable_end_string", |s| &mut s.variable_end),
    ("comment_start_string", |s| &mut s.comment_start),
    ("comment_end_string", |s| &mut s.comment_end),
];

/// The template's own options, which the reference takes out before `copy` sees the arguments.
const TEMPLATE_ONLY: &[&str] = &[
    "newline_sequence",
    "variable_start_string",
    "variable_end_string",
    "block_start_string",
    "block_end_string",
    "comment_start_string",
    "comment_end_string",
    "trim_blocks",
    "lstrip_blocks",
    "output_encoding",
];

pub(super) fn start(ctx: Context<'_>) -> Box<dyn Plugin> {
    match rendered(&ctx) {
        Ok(of) => copy_bytes(of),
        Err(Failed::Said(msg)) => failing(msg),
        Err(Failed::Chained(result)) => done(*result),
    }
}

/// Why `rendered` gave no copy: a sentence, or a plugin's failure with the chain the reference
/// prints for it.
enum Failed {
    Said(String),
    Chained(Box<TaskResult>),
}

impl From<String> for Failed {
    fn from(msg: String) -> Self {
        Failed::Said(msg)
    }
}

impl From<&str> for Failed {
    fn from(msg: &str) -> Self {
        Failed::Said(msg.to_string())
    }
}

/// The failure of a template that calls a filter, test or lookup that raised, measured on
/// ansible-core 2.19.12: `changed: false`, the plugin's sentence under `Task failed: `, and in
/// the error block the sentence at the template file with no line, then the causes behind it.
fn plugin_failed(file: &Path, sentence: &str) -> Failed {
    let (head, behind) = crate::bridge::split_failure(sentence);
    let at = crate::playbook::Origin {
        path: file.display().to_string().into(),
        line: 0,
        col: 0,
    };
    let mut chain = vec![
        crate::render::lead("Task failed."),
        crate::render::cause(&head, Some(&at), None),
    ];
    chain.extend(
        behind
            .iter()
            .map(|block| crate::render::cause(block, None, None)),
    );
    let mut result = TaskResult::failed_with(crate::render::task_failed(sentence));
    result.0.insert("changed".into(), Value::Bool(false));
    // Nothing ran on the host, so no interpreter was discovered.
    result
        .0
        .insert(crate::executor::RAISED_KEY.into(), Value::Bool(true));
    result.0.insert(
        crate::render::ERROR_KEY.into(),
        Value::from(crate::render::task_failed(sentence)),
    );
    result
        .0
        .insert(crate::render::CHAIN_KEY.into(), Value::Array(chain));
    Failed::Chained(Box::new(result))
}

/// A string argument, `None` when absent or null. The reference turns anything else into a
/// string before it reads it.
fn string_arg(args: &Map<String, Value>, name: &str) -> Option<String> {
    match args.get(name)? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// A boolean option as the reference reads it with `boolean(strict=False)`: a word that is not
/// a boolean is false, and only an absent option takes the default.
fn flag(args: &Map<String, Value>, name: &str, default: bool) -> bool {
    args.get(name)
        .map_or(default, |v| as_bool_value(v).unwrap_or(false))
}

/// The six delimiter options, in the reference's words when one is wrong. The reference turns a
/// number into text, refuses a list or a mapping, and reads an empty value as none, which its
/// overrides then refuse.
fn syntax_of(args: &Map<String, Value>) -> Result<Syntax, String> {
    let mut syntax = Syntax::default();
    let mut given: Vec<(&str, Option<String>)> = Vec::new();
    for (name, _) in DELIMITERS {
        given.push((
            name,
            match args.get(name) {
                None => continue,
                Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.clone()).filter(|s| !s.is_empty()),
                Some(Value::Bool(b)) => Some(if *b { "True" } else { "False" }.into()),
                Some(v @ (Value::Array(_) | Value::Object(_))) => {
                    return Err(format!(
                        "Invalid value provided for 'string': {}",
                        crate::playbook::python_repr(v)
                    ));
                }
                Some(number) => Some(number.to_string()),
            },
        ));
    }
    for (name, field) in DELIMITERS {
        match given.iter().find(|(n, _)| *n == name) {
            Some((_, Some(value))) => field(&mut syntax).clone_from(value),
            Some((_, None)) => {
                return Err(format!(
                    "TemplateOverrides.{name} must be <class 'str'> instead of <class 'NoneType'>"
                ));
            }
            None => {}
        }
    }
    syntax.check_starts().map_err(str::to_string)?;
    Ok(syntax)
}

/// A copy of the item's variables, its provenance kept, with the four the plugin adds.
///
/// `ansible_managed` is the controller's own word. `template_path` and `template_fullpath` are a
/// host's value when the `src` render read one, and so is `template_destpath` when the `dest`
/// render did: a template that prints one gets the text, and the text is never rendered again.
fn template_vars(
    item_vars: &HostVars,
    args_untrusted: &BTreeSet<String>,
    src: &str,
    fullpath: &str,
    dest: &str,
) -> HostVars {
    let mut vars = item_vars.clone();
    if vars.get("ansible_managed").is_none() {
        vars.insert("ansible_managed".into(), Value::from("Ansible managed"));
    }
    if args_untrusted.contains("src") {
        vars.insert_untrusted("template_path".into(), Value::from(src));
        vars.insert_untrusted("template_fullpath".into(), Value::from(fullpath));
    } else {
        vars.insert("template_path".into(), Value::from(src));
        vars.insert("template_fullpath".into(), Value::from(fullpath));
    }
    if args_untrusted.contains("dest") {
        vars.insert_untrusted("template_destpath".into(), Value::from(dest));
    } else {
        vars.insert("template_destpath".into(), Value::from(dest));
    }
    vars
}

/// The rendered template and the arguments `copy` gets, or the task's failure.
fn rendered(ctx: &Context<'_>) -> Result<CopyOf, Failed> {
    let args = ctx.args;
    // The reference's checks, in its order and in its words.
    if string_arg(args, "state").is_some() {
        return Err("'state' cannot be specified on a template".into());
    }
    let (Some(src), Some(dest)) = (string_arg(args, "src"), string_arg(args, "dest")) else {
        return Err("src and dest are required".into());
    };
    // The escaped spellings are what a YAML file usually carries.
    let newline_sequence = match string_arg(args, "newline_sequence").as_deref() {
        None | Some("\n" | "\\n") => "\n",
        Some("\r" | "\\r") => "\r",
        Some("\r\n" | "\\r\\n") => "\r\n",
        Some(_) => return Err("newline_sequence needs to be one of: \n, \r or \r\n".into()),
    };
    // The delimiters pick which part of the author's template is code. ansible-core takes a
    // value a managed host chose, so `allow` does too; the stricter modes refuse it.
    if ctx.host_named_paths != crate::config::HostNamedPaths::Allow
        && let Some((name, _)) = DELIMITERS
            .iter()
            .find(|(name, _)| ctx.args_untrusted.contains(*name))
    {
        return Err(format!(
            "the '{name}' of this task was named by a managed host, and a delimiter a host chose is never used"
        )
        .into());
    }
    let syntax = syntax_of(args)?;
    let encoding = string_arg(args, "output_encoding").filter(|e| !e.is_empty());
    check_host_named(
        ctx.host_named_paths,
        ctx.args_untrusted,
        "src",
        &src,
        "templates",
        ctx.origin,
        ctx.playbook_dir,
    )?;
    let searched = search_paths(ctx.origin, ctx.playbook_dir, "templates", &src);
    let Some(found) = searched.iter().find(|p| p.exists()) else {
        return Err(not_found(&src, &searched).into());
    };
    let mut bytes = std::fs::read(found)
        .map_err(|err| format!("could not read src={}: {err}", found.display()))?;
    if crate::vault::is_encrypted(&bytes) {
        bytes = crate::vault::decrypt(&bytes, &crate::vault::run_secrets())
            .map_err(|err| format!("could not read src={}: {err}", found.display()))?;
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("could not read src={} as utf-8", found.display()))?;

    let vars = template_vars(
        ctx.item_vars,
        ctx.args_untrusted,
        &src,
        &found.display().to_string(),
        &dest,
    );
    let options = FileRender {
        trim_blocks: flag(args, "trim_blocks", true),
        lstrip_blocks: flag(args, "lstrip_blocks", false),
        newline_sequence: newline_sequence.into(),
        syntax,
        name: Some(src.clone()),
        ..FileRender::default()
    };
    let out = ctx
        .templar
        .render_file(&text, Vars::from(&vars), &options)
        .map_err(|err| {
            if err.0.starts_with("The ") && crate::template::is_plugin_failure(&err.0) {
                plugin_failed(found, &err.0)
            } else {
                format!("could not render {src}: {err}").into()
            }
        })?;
    let bytes = encode(&out, encoding.as_deref().unwrap_or("utf-8"))?;

    let mut args: Map<String, Value> = args
        .iter()
        .filter(|(k, _)| !TEMPLATE_ONLY.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if args.get("mode").and_then(Value::as_str) == Some("preserve") {
        args.insert("mode".into(), Value::String(local_mode(found)?));
    }
    Ok(CopyOf {
        bytes,
        basename: found
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        args,
    })
}

#[cfg(test)]
mod tests;
