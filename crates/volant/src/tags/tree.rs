// SPDX-License-Identifier: GPL-3.0-or-later
//! The tags of a value's parts, in the value's own shape. Only the tagged parts and the
//! containers above them have a node: an untagged value has no tree at all.

use serde_json::Value;

use super::TagSetId;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TagTree {
    Leaf(TagSetId),
    /// A container: its own tags, which may be none, and its tagged children.
    Node(TagSetId, Children),
}

/// The tagged children of a container, in the container's order.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Children {
    Map(Vec<(String, TagTree)>),
    Seq(Vec<(usize, TagTree)>),
}

/// The tree of each variable of a layer, by name.
pub type TagMap = indexmap::IndexMap<String, TagTree>;

impl TagTree {
    /// A node with these children, or nothing when neither it nor they carry a tag.
    pub fn node(own: TagSetId, children: Children) -> Option<TagTree> {
        let empty = match &children {
            Children::Map(entries) => entries.is_empty(),
            Children::Seq(entries) => entries.is_empty(),
        };
        match (own.is_none(), empty) {
            (true, true) => None,
            (false, true) => Some(TagTree::Leaf(own)),
            _ => Some(TagTree::Node(own, children)),
        }
    }

    pub fn own(&self) -> TagSetId {
        match self {
            TagTree::Leaf(id) | TagTree::Node(id, _) => *id,
        }
    }

    pub fn at(&self, key: &str) -> Option<&TagTree> {
        match self {
            TagTree::Node(_, Children::Map(entries)) => {
                entries.iter().find(|(k, _)| k == key).map(|(_, tree)| tree)
            }
            _ => None,
        }
    }

    pub fn index(&self, i: usize) -> Option<&TagTree> {
        match self {
            TagTree::Node(_, Children::Seq(entries)) => entries
                .iter()
                .find(|(at, _)| *at == i)
                .map(|(_, tree)| tree),
            _ => None,
        }
    }
}

/// The tree of `merge_hash(old, new)`, from the trees of `old` and `new`: mappings merge key by
/// key all the way down, so each leaf keeps the tags of the side its value came from; anything
/// else is `new`'s, with `new`'s tree or none. The merged mapping keeps `old`'s own tags, as
/// `merge_hash` works on a copy of `old`.
pub fn merge(
    mine: Option<&TagTree>,
    theirs: Option<&TagTree>,
    old: &Value,
    new: &Value,
) -> Option<TagTree> {
    let (Value::Object(old), Value::Object(new)) = (old, new) else {
        return theirs.cloned();
    };
    let mut children = Vec::new();
    for (key, value) in old {
        let tree = match new.get(key) {
            Some(fresh) => merge(
                mine.and_then(|t| t.at(key)),
                theirs.and_then(|t| t.at(key)),
                value,
                fresh,
            ),
            None => mine.and_then(|t| t.at(key)).cloned(),
        };
        children.extend(tree.map(|tree| (key.clone(), tree)));
    }
    for key in new.keys().filter(|key| !old.contains_key(*key)) {
        if let Some(tree) = theirs.and_then(|t| t.at(key)) {
            children.push((key.clone(), tree.clone()));
        }
    }
    let own = mine.map_or(TagSetId::NONE, TagTree::own);
    TagTree::node(own, Children::Map(children))
}
