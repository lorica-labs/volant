// SPDX-License-Identifier: GPL-3.0-or-later
//! The tags of a loaded value, as ansible-core's loader puts them: each string, number, list and
//! mapping carries the `Origin` it was written at, and each string of a trusted file is
//! `TrustedAsTemplate`, except under `!unsafe`. Booleans and nulls carry nothing.

use saphyr::{Scalar, Yaml};
use serde_json::Value;

use super::{Origins, is_vault, scalar, walk::key_text};
use crate::tags::{self, Children, TagSet, TagTree};

/// The tags of `node`, a node of the documents `origins` came with, written in the file `path`
/// (absolute). With no `path`, or a node the origins do not know, values carry no `Origin`.
pub fn tags_of(
    node: &Yaml<'_>,
    origins: &Origins,
    path: Option<&str>,
    trusted: bool,
) -> Option<TagTree> {
    let at = origins.of_value(node);
    Walk { origins, path }.tree(node, at, trusted)
}

struct Walk<'o> {
    origins: &'o Origins,
    path: Option<&'o str>,
}

impl Walk<'_> {
    fn set(&self, at: Option<(usize, usize)>, trusted: bool) -> tags::TagSetId {
        let origin = self.path.zip(at).map(|(path, (line, col))| {
            tags::origin_at(
                path,
                u32::try_from(line).unwrap_or(0),
                u32::try_from(col).unwrap_or(0),
            )
        });
        tags::intern(TagSet {
            origin,
            trusted,
            ..TagSet::default()
        })
    }

    fn leaf(&self, at: Option<(usize, usize)>, trusted: bool) -> Option<TagTree> {
        TagTree::node(self.set(at, trusted), Children::Seq(Vec::new()))
    }

    fn tree(&self, node: &Yaml<'_>, at: Option<(usize, usize)>, trusted: bool) -> Option<TagTree> {
        match node {
            Yaml::Value(Scalar::Null | Scalar::Boolean(_)) => None,
            Yaml::Value(Scalar::String(_)) => self.leaf(at, trusted),
            Yaml::Value(_) => self.leaf(at, false),
            Yaml::Sequence(items) => {
                let children = items
                    .iter()
                    .enumerate()
                    .filter_map(|(i, item)| {
                        let tree = self.tree(item, self.origins.of_value(item), trusted)?;
                        Some((i, tree))
                    })
                    .collect();
                TagTree::node(self.set(at, false), Children::Seq(children))
            }
            Yaml::Mapping(map) => {
                let mut children: Vec<(String, TagTree)> = Vec::new();
                for (key, value) in map {
                    let Ok(key) = key_text(unsafe_key(key)) else {
                        continue;
                    };
                    // A key written twice keeps its last value, and so its last tags.
                    children.retain(|(k, _)| *k != key);
                    if let Some(tree) = self.tree(value, self.origins.of_value(value), trusted) {
                        children.push((key, tree));
                    }
                }
                let own = self.set(self.origins.of(map).or(at), false);
                TagTree::node(own, Children::Map(children))
            }
            // The tagged node itself is placed at its tag; its content has no place of its own.
            Yaml::Tagged(tag, inner) if tag.handle == "!" && tag.suffix == "unsafe" => {
                self.tree(inner, at, false)
            }
            // The wrapper a `!vault` value loads as must still render to decrypt.
            Yaml::Tagged(tag, _) if tag.handle == "!" && is_vault(&tag.suffix) => {
                self.leaf(at, true)
            }
            Yaml::Tagged(tag, inner) if scalar::wide_digits(tag, inner).is_some() => {
                self.leaf(at, false)
            }
            _ => None,
        }
    }
}

/// A mapping key as `to_json` reads it: `!unsafe a` is the key `a`.
fn unsafe_key<'t, 'a>(key: &'t Yaml<'a>) -> &'t Yaml<'a> {
    match key {
        Yaml::Tagged(tag, inner) if tag.handle == "!" && tag.suffix == "unsafe" => inner,
        key => key,
    }
}

/// A value as JSON written by ansible-core carries it, and its tags: every string is trusted, as
/// the reference trusts this JSON, except one that arrives as `{"__ansible_unsafe": text}`.
pub(crate) fn unsafe_markers(value: &Value, origin: tags::OriginId) -> (Value, Option<TagTree>) {
    let set = |trusted| {
        tags::intern(TagSet {
            origin: Some(origin),
            trusted,
            ..TagSet::default()
        })
    };
    match value {
        Value::Object(map)
            if map.len() == 1 && map.get("__ansible_unsafe").is_some_and(Value::is_string) =>
        {
            (
                map["__ansible_unsafe"].clone(),
                Some(TagTree::Leaf(set(false))),
            )
        }
        Value::Object(map) => {
            let mut kids = Vec::new();
            let out = map
                .iter()
                .map(|(k, v)| {
                    let (v, tree) = unsafe_markers(v, origin);
                    kids.extend(tree.map(|tree| (k.clone(), tree)));
                    (k.clone(), v)
                })
                .collect();
            (
                Value::Object(out),
                TagTree::node(set(false), Children::Map(kids)),
            )
        }
        Value::Array(items) => {
            let mut kids = Vec::new();
            let out = items
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let (v, tree) = unsafe_markers(v, origin);
                    kids.extend(tree.map(|tree| (i, tree)));
                    v
                })
                .collect();
            (
                Value::Array(out),
                TagTree::node(set(false), Children::Seq(kids)),
            )
        }
        other => (other.clone(), tags::uniform(other, origin, true)),
    }
}
