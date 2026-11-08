// SPDX-License-Identifier: GPL-3.0-or-later
//! Resolving the variables that hold templates: the whole map ahead of a task, and one variable
//! when a template reads it.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::{Map, Value};

use super::context::{Vars, context_of};
use super::tagged::{take_result, with_origin};
use super::templar::Templar;
use super::vm::{holds_trusted_template, reveal_vault};
use super::{Tainted, TemplateError};
use crate::tags::{Children, TagMap, TagSetId, TagTree, Tagged};

/// The tags of what the last render over `ctx` gave, for a string written with `tree`: what it
/// passed through, or the place the string was written and no trust.
fn result_tree(ctx: &minijinja::Value, tree: Option<&TagTree>) -> Option<TagTree> {
    let origin = tree.and_then(|t| crate::tags::get(t.own()).origin);
    with_origin(take_result(ctx), origin)
}

impl Templar {
    /// What a template reads a variable as when the variable holds a template: a string renders
    /// like any other, and a failure comes back to the reader. A list or mapping renders string
    /// by string and leaves one that fails as written, so a member nothing reads raises nothing.
    pub(super) fn render_read(
        &self,
        value: &Value,
        tree: Option<&TagTree>,
        ctx: &minijinja::Value,
        tainted: &Tainted,
        any: &mut bool,
    ) -> Result<(Value, Option<TagTree>), TemplateError> {
        match value {
            Value::String(text) => {
                let out = self.render_in(text, ctx, tainted)?;
                *any |= tainted.load(Ordering::Relaxed);
                Ok((out, result_tree(ctx, tree)))
            }
            other => Ok(self.render_value_lenient(other, tree, ctx, tainted, any)),
        }
    }

    /// Templates the trusted strings of a variable map against the map itself, a few passes, until
    /// nothing changes. Best effort: a value that fails to render is left as written, so the
    /// error surfaces where the value is used, as it does in Ansible.
    ///
    /// Each value that comes out carries the tags of what it rendered to, so a result is rendered
    /// no further, and one built from what a managed host sent carries that as well: a task's own
    /// `vars:` are resolved here and read nowhere else, so its arguments learn it from these tags.
    pub fn resolve_vars<'a>(&self, vars: impl Into<Vars<'a>>) -> Tagged {
        let vars = vars.into();
        let mut tags = vars.tags.cloned().unwrap_or_default();
        let holds = |map: &Map<String, Value>, tags: &TagMap| {
            tags.iter().any(|(name, tree)| {
                map.get(name)
                    .is_some_and(|v| holds_trusted_template(v, Some(tree)))
            })
        };
        let host = || {
            let named =
                |m: &Map<String, Value>| m.get("inventory_hostname")?.as_str().map(String::from);
            named(vars.map).or_else(|| vars.shared.and_then(|s| named(s)))
        };
        // Most maps hold no template at all: walking them once is far cheaper than the two
        // clones a pass costs.
        if !holds(vars.map, &tags) {
            crate::vault::set_names(&host().unwrap_or_default(), Vec::new());
            return Tagged::with_shared_tags(vars.map.clone(), tags);
        }
        let mut current = vars.map.clone();
        // What a `!vault` holds is text from here on: a sibling that reads it must see the text,
        // not the wrapper, and the name is data so no pass renders it.
        let mut vaulted = Vec::new();
        for (k, v) in &mut current {
            if let Value::String(s) = v
                && crate::yaml::vault_text(s).is_some()
            {
                vaulted.push((k.clone(), s.clone()));
            }
            // A container keeps its wrappers: they render to their text while the rest of the
            // container still renders.
            let trusted = tags
                .get(k)
                .is_some_and(|t| crate::tags::is_trusted(t.own()));
            if trusted && v.is_string() && reveal_vault(v) {
                Arc::make_mut(&mut tags).insert(k.clone(), crate::tags::from_host(None));
            }
        }
        crate::vault::set_names(&host().unwrap_or_default(), vaulted);
        for _ in 0..5 {
            if !holds(&current, &tags) {
                break;
            }
            // One context for the whole pass: every value of the map renders against the same
            // map, so building it per value paid for the same conversion once per variable.
            let (ctx, tainted) = context_of(
                self,
                Vars {
                    map: &current,
                    tags: Some(&tags),
                    hostvars: vars.hostvars,
                    shared: vars.shared,
                    data_lists: vars.data_lists,
                    facts: vars.facts,
                    base: vars.base,
                },
            );
            let mut next = current.clone();
            let mut next_tags = Arc::clone(&tags);
            let mut changed = false;
            for (name, tree) in tags.iter() {
                let Some(value) = current.get(name) else {
                    continue;
                };
                if !holds_trusted_template(value, Some(tree)) {
                    continue;
                }
                let mut any = false;
                let (rendered, fresh) =
                    self.render_value_lenient(value, Some(tree), &ctx, &tainted, &mut any);
                let fresh = if any {
                    Some(crate::tags::from_host(fresh))
                } else {
                    fresh
                };
                if rendered != *value || fresh.as_ref() != Some(tree) {
                    changed = true;
                    next.insert(name.clone(), rendered);
                    let table = Arc::make_mut(&mut next_tags);
                    match fresh {
                        Some(fresh) => _ = table.insert(name.clone(), fresh),
                        None => _ = table.swap_remove(name),
                    }
                }
            }
            current = next;
            tags = next_tags;
            if !changed {
                break;
            }
        }
        Tagged::with_shared_tags(current, tags)
    }

    fn render_value_lenient(
        &self,
        value: &Value,
        tree: Option<&TagTree>,
        ctx: &minijinja::Value,
        tainted: &Tainted,
        any: &mut bool,
    ) -> (Value, Option<TagTree>) {
        let own = tree.map_or(TagSetId::NONE, TagTree::own);
        match value {
            // A value that fails stays as written and keeps its tags: a loop variable it names is
            // bound later, and the render for that item is the one that can finish it.
            Value::String(_) if holds_trusted_template(value, tree) => {
                let Value::String(text) = value else {
                    unreachable!("the arm matched a string")
                };
                match self.render_in(text, ctx, tainted) {
                    Ok(out) => {
                        *any |= tainted.load(Ordering::Relaxed);
                        (out, result_tree(ctx, tree))
                    }
                    Err(_) => (value.clone(), tree.cloned()),
                }
            }
            Value::Array(items) => {
                let mut kids = Vec::new();
                let out = items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| {
                        let kid = tree.and_then(|t| t.index(i));
                        let (out, kid) = self.render_value_lenient(item, kid, ctx, tainted, any);
                        kids.extend(kid.map(|kid| (i, kid)));
                        out
                    })
                    .collect();
                (Value::Array(out), TagTree::node(own, Children::Seq(kids)))
            }
            Value::Object(map) => {
                let mut kids = Vec::new();
                let out = map
                    .iter()
                    .map(|(k, item)| {
                        let kid = tree.and_then(|t| t.at(k));
                        let (out, kid) = self.render_value_lenient(item, kid, ctx, tainted, any);
                        kids.extend(kid.map(|kid| (k.clone(), kid)));
                        (k.clone(), out)
                    })
                    .collect();
                (Value::Object(out), TagTree::node(own, Children::Map(kids)))
            }
            other => (other.clone(), tree.cloned()),
        }
    }
}
