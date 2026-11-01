// SPDX-License-Identifier: GPL-3.0-or-later
//! A task's arguments rendered key by key: which of them read a managed host, and the tags of
//! what each one rendered to.

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use serde_json::{Map, Value};

use super::context::{Vars, context_of, is_literal};
use super::tagged::{take_result, with_origin};
use super::{RESOLVING, Tainted, Templar, TemplateError, bridged};
use crate::tags::{Children, OriginId, TagMap, TagSet, TagTree};

/// A map rendered key by key: the map, the keys whose render read a managed host, and the tags
/// of what each key rendered to.
pub type RenderedArgs = (Map<String, Value>, BTreeSet<String>, TagMap);

/// Where each key of a map was written.
type OriginOf<'a> = &'a dyn Fn(&str) -> Option<OriginId>;

/// Where the value being rendered was written, for the tags of a result that passed nothing
/// through.
#[derive(Clone, Copy)]
pub(super) struct Tagging {
    origin: Option<OriginId>,
}

/// `own`'s tags over `children`.
fn with_children(own: Option<TagTree>, children: Children) -> Option<TagTree> {
    TagTree::node(
        own.map_or(crate::tags::TagSetId::NONE, |t| t.own()),
        children,
    )
}

impl Templar {
    /// `render_value` over a map, keeping the answer **per key**: the names whose own render
    /// read a managed host come back beside the rendered map.
    ///
    /// A task's arguments are mostly data, and data is what a rendered value should be. A few
    /// of them are not: an argument the engine reads back as source text - the name a
    /// `debug: var:` compiles - is engine input, and the only moment its provenance exists is
    /// the render that produced it. One answer for the whole map would refuse an argument the
    /// playbook wrote because a sibling argument came from a host.
    pub fn render_map_tainted<'a>(
        &self,
        map: &Map<String, Value>,
        vars: impl Into<Vars<'a>>,
    ) -> Result<(Map<String, Value>, BTreeSet<String>), TemplateError> {
        let (out, untrusted, _) = self.render_map_in(map, vars.into(), None)?;
        Ok((out, untrusted))
    }

    /// [`Templar::render_map_tainted`], with the tags of each rendered value: what its render
    /// passed through, or else where the key was written (`origin_of`) and, for text written as
    /// it is, trust.
    pub fn render_map_tagged<'a>(
        &self,
        map: &Map<String, Value>,
        vars: impl Into<Vars<'a>>,
        origin_of: OriginOf<'_>,
    ) -> Result<RenderedArgs, TemplateError> {
        self.render_map_in(map, vars.into(), Some(origin_of))
    }

    fn render_map_in(
        &self,
        map: &Map<String, Value>,
        vars: Vars<'_>,
        origin_of: Option<OriginOf<'_>>,
    ) -> Result<RenderedArgs, TemplateError> {
        let (ctx, tainted) = context_of(self, vars);
        let mut out = Map::new();
        let mut untrusted = BTreeSet::new();
        let mut tags = TagMap::new();
        for (key, value) in map {
            let mut any = false;
            let tagging = origin_of.map(|of| Tagging { origin: of(key) });
            let (rendered, tree) = self
                .render_value_in(value, &ctx, &tainted, &mut any, false, tagging)
                .map_err(|err| {
                    // Measured on ansible-core 2.19.12: a plugin's failure names the value it
                    // was resolving, `Error while resolving value for 'msg': The lookup plugin
                    // 'x' failed: …`.
                    // A read with no value is worded the same way: `... for 'msg': 'x' is
                    // undefined`.
                    if bridged::is_plugin_failure(&err.0)
                        || err.is_undefined()
                        || err.0.starts_with(super::NO_FILTER)
                        || err.0.starts_with("Error rendering template: ")
                        || err.0.starts_with("Syntax error in template: ")
                        || super::include::is_not_found(&err.0)
                    {
                        TemplateError(format!("{RESOLVING}'{key}': {}", err.0))
                    } else if err.0.starts_with(crate::vault::UNDECRYPTABLE) {
                        TemplateError(format!(
                            "{RESOLVING}'{key}': Error rendering template: {}",
                            err.0
                        ))
                    } else {
                        err
                    }
                })?;
            out.insert(key.clone(), rendered);
            if let Some(tree) = tree {
                tags.insert(key.clone(), tree);
            }
            if any {
                untrusted.insert(key.clone());
            }
        }
        Ok((out, untrusted, tags))
    }

    /// Templates every string inside `value`; with `tagging`, the tags of what comes out too.
    pub(super) fn render_value_in(
        &self,
        value: &Value,
        ctx: &minijinja::Value,
        tainted: &Tainted,
        any: &mut bool,
        keep: bool,
        tagging: Option<Tagging>,
    ) -> Result<(Value, Option<TagTree>), TemplateError> {
        let leaf = |trusted| {
            let t = tagging?;
            let own = crate::tags::intern(TagSet {
                origin: t.origin,
                trusted,
                ..TagSet::default()
            });
            TagTree::node(own, Children::Seq(Vec::new()))
        };
        Ok(match value {
            Value::String(s) => {
                take_result(ctx);
                let out = self.render_in(s, ctx, tainted)?;
                *any |= tainted.load(Ordering::Relaxed) || (keep && is_literal(ctx));
                let tree = match tagging {
                    Some(t) if Templar::is_template(s) => with_origin(take_result(ctx), t.origin),
                    _ => leaf(true),
                };
                (out, tree)
            }
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                let mut kids = Vec::new();
                for (i, v) in items.iter().enumerate() {
                    let (item, tree) = self.render_value_in(v, ctx, tainted, any, keep, tagging)?;
                    out.push(item);
                    kids.extend(tree.map(|t| (i, t)));
                }
                (
                    Value::Array(out),
                    with_children(leaf(false), Children::Seq(kids)),
                )
            }
            Value::Object(map) => {
                let mut out = Map::new();
                let mut kids = Vec::new();
                for (k, v) in map {
                    let (item, tree) = self.render_value_in(v, ctx, tainted, any, keep, tagging)?;
                    out.insert(k.clone(), item);
                    kids.extend(tree.map(|t| (k.clone(), t)));
                }
                (
                    Value::Object(out),
                    with_children(leaf(false), Children::Map(kids)),
                )
            }
            Value::Number(_) => (value.clone(), leaf(false)),
            other => (other.clone(), None),
        })
    }
}
