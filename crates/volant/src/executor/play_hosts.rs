// SPDX-License-Identifier: GPL-3.0-or-later
//! A play's `hosts` written as a template, rendered when the play starts.
//!
//! The reference renders it with the play and no host: the groups, the extra variables, the play's
//! `vars:` and `vars_files`, the defaults and exported variables of its roles, and the magic
//! variables that need no host. It does not see an inventory variable, a `group_vars` file, a
//! fact or `inventory_hostname`. `hostvars` is there once a play has run, and not before.
//! `ansible_play_hosts_all` and its two siblings list every host of the inventory, narrowed by
//! `--limit`, because the pattern is not known yet.
//!
//! The render is one expression pass, with the further passes the templar gives a variable that
//! holds a template. A value that came from a host is data: reading one makes the result data, so
//! text a host sent back is a pattern of its own words and is never evaluated.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

use super::hosts_vars::settle;
use super::play_checks::read_vars_files;
use super::python_type;
use crate::compile::Compiled;
use crate::playbook::Play;
use crate::stats::Refusal;
use crate::template::Templar;
use crate::vars::{Scope, VarStore};

/// What the render reads besides the play.
pub(crate) struct Context<'a> {
    pub templar: &'a Templar,
    pub vars: &'a Mutex<VarStore>,
    pub compiled: &'a Compiled,
    /// Every host of the inventory, narrowed by `--limit`.
    pub all_hosts: &'a [String],
    /// Whether a play has run already: `hostvars` exists from then on.
    pub with_hostvars: bool,
}

/// Whether the play has a `hosts` that needs rendering.
pub(crate) fn is_templated(play: &Play) -> bool {
    play.host_patterns.iter().any(|p| Templar::is_template(p))
}

/// The play with its `hosts` rendered, or the play itself when it holds no template.
///
/// A play with no `name` takes the rendered pattern for its name, as the reference does.
///
/// # Errors
/// Exit 4, before the play's banner, for a template that does not render and for a result that is
/// not a string or a list of strings.
pub(crate) fn render_hosts<'a>(
    play: &'a Play,
    context: &Context<'_>,
) -> anyhow::Result<Cow<'a, Play>> {
    if !is_templated(play) {
        return Ok(Cow::Borrowed(play));
    }
    // The reference renders the name of a play first, and a play with no name has its pattern
    // for one: an undefined variable is reported under that keyword.
    let unnamed = !play.host_list && play.name == play.hosts;
    let refuse = |why: &str| Refusal::at(4, format!("Error processing keyword 'hosts': {why}"));
    let undefined = |why: &str| {
        let keyword = if unnamed { "name" } else { "hosts" };
        Refusal::at(4, format!("Error processing keyword '{keyword}': {why}"))
    };
    let mut scope = Scope {
        play_vars: play.vars.clone(),
        role_defaults: context.compiled.exported.defaults.clone(),
        role_vars: context.compiled.exported.vars.clone(),
        play_hosts: context.all_hosts.into(),
        batch_hosts: context.all_hosts.into(),
        all_play_hosts: context.all_hosts.into(),
        ..Scope::default()
    };
    let mut view = {
        let mut store = context.vars.lock().expect("vars lock");
        store.play_view(&scope, context.with_hostvars)
    };
    if !play.vars_files.is_empty() {
        // Quiet: the entry that has no value is warned about when the play's hosts read their
        // own files, once, as the reference's display says it once.
        let dir = context
            .vars
            .lock()
            .expect("vars lock")
            .playbook_dir()
            .to_path_buf();
        scope.vars_files = read_vars_files(
            &play.vars_files,
            context.templar,
            view.vars(),
            &dir,
            &mut HashMap::new(),
            &mut |_| {},
        )?;
        view = context
            .vars
            .lock()
            .expect("vars lock")
            .play_view(&scope, context.with_hostvars);
    }
    let (settled, data) =
        settle(&play.host_patterns, context.templar, &view).map_err(|why| undefined(&why))?;
    let mut patterns: Vec<String> = Vec::new();
    let mut shown = None;
    let mut nameless = false;
    for written in &play.host_patterns {
        // One pass over the settled variables: what it makes is the pattern, and nothing renders
        // it again.
        let (rendered, _) = context
            .templar
            .render_one_pass_tainted(written, view.vars_over(&settled, &data))
            .map_err(|err| undefined(&err.0))?;
        match rendered {
            Value::String(text) => {
                if text.is_empty() {
                    return Err(refuse(
                        "Keyword 'hosts' is required, and cannot have empty values.",
                    ));
                }
                shown = Some(text.clone());
                patterns.push(text);
            }
            // A list is a list of patterns when `hosts` was one string. Inside a list written as
            // one it is an item that is not a string.
            Value::Array(items) if !play.host_list => {
                let mut texts = Vec::new();
                for item in &items {
                    texts.push(string_item(item).ok_or_else(|| not_a_string(&refuse, item))?);
                }
                shown = Some(crate::listing::patterns(&texts));
                patterns.extend(texts);
            }
            // `none` is the pattern `None`, which matches nothing and is warned about. The
            // reference names the play with nothing: its banner has no brackets.
            Value::Null => {
                nameless = true;
                patterns.push("None".to_string());
            }
            other => return Err(not_a_string(&refuse, &other)),
        }
    }
    let mut play = play.clone();
    play.hosts = patterns.join(",");
    if unnamed {
        play.name = shown.unwrap_or_else(|| play.hosts.clone());
        play.nameless = nameless;
    }
    play.host_patterns = patterns;
    Ok(Cow::Owned(play))
}

fn string_item(item: &Value) -> Option<String> {
    item.as_str().map(str::to_string)
}

fn not_a_string(refuse: &impl Fn(&str) -> anyhow::Error, item: &Value) -> anyhow::Error {
    refuse(&format!(
        "Keyword 'hosts' items must be of type 'str', not '{}'.",
        python_type(item)
    ))
}
