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

use serde_json::{Map, Value};

use super::copy::{CopyOf, copy_bytes, failing, local_mode};
use super::files::{not_found, refuse_host_named, search_paths};
use super::{Context, Plugin};
use crate::executor::as_bool_value;
use crate::template::{FileRender, Vars};
use crate::vars::HostVars;

/// The delimiters Jinja reads, with the default each one has. A template written for others is
/// refused rather than rendered as if it used these.
const DELIMITERS: &[(&str, &str)] = &[
    ("variable_start_string", "{{"),
    ("variable_end_string", "}}"),
    ("block_start_string", "{%"),
    ("block_end_string", "%}"),
    ("comment_start_string", "{#"),
    ("comment_end_string", "#}"),
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
        Err(msg) => failing(msg),
    }
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

fn flag(args: &Map<String, Value>, name: &str, default: bool) -> bool {
    args.get(name).and_then(as_bool_value).unwrap_or(default)
}

/// A copy of the item's variables, its provenance kept, with the four the plugin adds.
///
/// `ansible_managed`, `template_path` and `template_fullpath` are the controller's own words: a
/// `src` whose render read a host is refused before this is reached, so the path found cannot be
/// one a host chose. `template_destpath` is the `dest` as rendered, and stays a host's value when
/// its render read one.
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
    vars.insert("template_path".into(), Value::from(src));
    vars.insert("template_fullpath".into(), Value::from(fullpath));
    if args_untrusted.contains("dest") {
        vars.insert_untrusted("template_destpath".into(), Value::from(dest));
    } else {
        vars.insert("template_destpath".into(), Value::from(dest));
    }
    vars
}

/// The rendered template and the arguments `copy` gets, or the task's failure.
fn rendered(ctx: &Context<'_>) -> Result<CopyOf, String> {
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
    if let Some((name, _)) = DELIMITERS
        .iter()
        .find(|(name, default)| string_arg(args, name).is_some_and(|v| v != *default))
    {
        return Err(format!(
            "argument '{name}' is not supported yet on 'template'"
        ));
    }
    if let Some(encoding) = string_arg(args, "output_encoding").filter(|e| !e.is_empty())
        && !matches!(
            encoding.to_ascii_lowercase().replace('_', "-").as_str(),
            "utf-8" | "utf8"
        )
    {
        return Err(format!(
            "output_encoding '{encoding}' is not supported yet on 'template': only utf-8 is"
        ));
    }
    refuse_host_named(ctx.args_untrusted, "src")?;
    let searched = search_paths(ctx.origin, ctx.playbook_dir, "templates", &src);
    let Some(found) = searched.iter().find(|p| p.exists()) else {
        return Err(not_found(&src, &searched));
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
        name: Some(src.clone()),
    };
    let out = ctx
        .templar
        .render_file(&text, Vars::from(&vars), &options)
        .map_err(|err| format!("could not render {src}: {err}"))?;

    let mut args: Map<String, Value> = args
        .iter()
        .filter(|(k, _)| !TEMPLATE_ONLY.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if args.get("mode").and_then(Value::as_str) == Some("preserve") {
        args.insert("mode".into(), Value::String(local_mode(found)?));
    }
    Ok(CopyOf {
        bytes: out.into_bytes(),
        basename: found
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        args,
    })
}

#[cfg(test)]
mod tests;
