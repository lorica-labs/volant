// SPDX-License-Identifier: GPL-3.0-or-later
//! What only the inventory's dumper writes: anchors and aliases for a node reached twice, and
//! `!vault` values.

use std::collections::{BTreeMap, HashMap, HashSet};

use minijinja::Value;
use minijinja::value::ValueKind;

use super::super::scalar::analyze;
use super::Emitter;

/// The address of a list or a mapping [`super::super::node`] built: a clone is the same node.
fn identity(v: &Value) -> Option<usize> {
    if let Some(items) = v.downcast_object_ref::<Vec<Value>>() {
        return Some(std::ptr::from_ref(items).addr());
    }
    v.downcast_object_ref::<BTreeMap<Value, Value>>()
        .map(|map| std::ptr::from_ref(map).addr())
}

/// The nodes the anchor pass reached, by address, with the name of those reached twice, and the
/// anchored nodes already written, an alias from then on.
#[derive(Default)]
pub(super) struct Anchors {
    named: HashMap<usize, Option<String>>,
    written: HashSet<usize>,
    last: usize,
}

impl Emitter {
    /// PyYAML's `Serializer.anchor_node` (libyaml's `_anchor_node`): the document walked in the
    /// order it is written, a node named `id001`, `id002` when it is reached the second time, and
    /// not entered again.
    pub(super) fn anchor_node(&mut self, v: &Value) {
        if let Some(id) = identity(v) {
            if let Some(anchor) = self.anchors.named.get_mut(&id) {
                if anchor.is_none() {
                    self.anchors.last += 1;
                    *anchor = Some(format!("id{:03}", self.anchors.last));
                }
                return;
            }
            self.anchors.named.insert(id, None);
        }
        match v.kind() {
            ValueKind::Map => {
                for (_, item) in self.entries(v) {
                    self.anchor_node(&item);
                }
            }
            ValueKind::Seq | ValueKind::Iterable => {
                for item in v.try_iter().into_iter().flatten() {
                    self.anchor_node(&item);
                }
            }
            _ => {}
        }
    }

    /// The anchor in front of a node the anchor pass named, or its alias, written instead of the
    /// node, once the node was written: `true` when the alias is all there is to write.
    pub(super) fn alias(&mut self, v: &Value) -> bool {
        let Some(id) = identity(v) else {
            return false;
        };
        let Some(Some(anchor)) = self.anchors.named.get(&id) else {
            return false;
        };
        let anchor = anchor.clone();
        if self.anchors.written.insert(id) {
            self.write_indicator(&format!("&{anchor}"), true, false, false);
            false
        } else {
            self.write_indicator(&format!("*{anchor}"), true, false, false);
            true
        }
    }

    /// A `!vault` value, or text marked unsafe written as the text: `false` for any other value.
    pub(super) fn tagged_scalar(&mut self, v: &Value, simple_key: bool) -> bool {
        let Some(text) = v.as_str() else {
            return false;
        };
        if let Some(cipher) = crate::yaml::vault_text(text) {
            self.vault(&cipher, simple_key);
        } else if let Some(plain) = crate::yaml::unsafe_text(text) {
            self.scalar_text(&Value::from(plain), simple_key);
        } else {
            return false;
        }
        true
    }

    /// `AnsibleDumper.get_node_from_ciphertext`: the ciphertext under the `!vault` tag, asked
    /// for as a literal block, which libyaml double-quotes where a block cannot stand.
    fn vault(&mut self, cipher: &str, simple_key: bool) {
        let chars: Vec<char> = cipher.chars().collect();
        let a = analyze(&chars);
        // `yaml_emitter_write_tag_handle`: a space unless one is there, then the tag.
        if !self.whitespace {
            self.write(&[' ']);
        }
        self.write(&['!', 'v', 'a', 'u', 'l', 't']);
        self.whitespace = false;
        self.indention = false;
        self.increase_indent(true, false);
        if a.allow_block && self.flow_level == 0 && !simple_key {
            self.write_literal(&chars);
        } else {
            self.write_double_quoted(&chars, !simple_key);
        }
        self.pop_indent();
    }
}
