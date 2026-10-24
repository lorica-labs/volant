// SPDX-License-Identifier: GPL-3.0-or-later
//! `to_yaml` and `to_nice_yaml`: PyYAML's `safe_dump` with `allow_unicode=True`, which is what
//! ansible-core calls. ansible-core dumps through `CSafeDumper` whenever PyYAML was built with
//! libyaml (`module_utils/common/yaml.py`), as it is on the reference controller, so the
//! representer is PyYAML's and the emitter is **libyaml's**. The emitter below keeps the state
//! both share (`column`, `whitespace`, `indention`, the indent stack) and PyYAML's function
//! names, cut down to what a document of plain data reaches: no tags and no block scalars but
//! for the callbacks' literal blocks and the inventory's `!vault`, and anchors only in the
//! inventory's document. Where libyaml and `emitter.py` differ, libyaml is followed: no `...` after a plain
//! root scalar, a character outside the Basic Multilingual Plane or `\x85` is not printable (the
//! string is double-quoted, `"\U0001F600"`, `"\N"`), a double-quoted string folds only at a
//! space, and a key is simple up to 128 bytes, the empty one included.

use minijinja::value::Kwargs;
use minijinja::{Environment, Value};

use super::add_filter;

mod emitter;
mod scalar;

use emitter::{dump, emit};

/// A task result as the callbacks dump it with `result_format = yaml` (`_dump_results`): block
/// style, four-space indent, keys sorted, and a string with a line break as a literal block.
pub(crate) fn callback_yaml(value: &serde_json::Value) -> String {
    emit(
        &super::bigint::to_template(value),
        4,
        None,
        Some(false),
        true,
        true,
        false,
    )
}

/// What `ansible-inventory --yaml` prints: `yaml.dump` with `AnsibleDumper`,
/// `default_flow_style=False` and `allow_unicode=True`, keys sorted. A list or a mapping that
/// [`node`] or [`mapping`] built and that sits in two places is one object to PyYAML: written
/// once under an anchor, `&id001`, and as the alias `*id001` after. A `!vault` value is its
/// ciphertext under the tag, text marked unsafe is the text.
pub(crate) fn inventory_yaml(value: &Value) -> String {
    emit(value, 2, None, Some(false), true, false, true)
}

/// A JSON value as a node of [`inventory_yaml`]'s document; a clone of it is the same node.
pub(crate) fn node(value: &serde_json::Value) -> Value {
    match value {
        serde_json::Value::Array(items) => {
            Value::from_object(items.iter().map(node).collect::<Vec<Value>>())
        }
        serde_json::Value::Object(map) => mapping(map.iter().map(|(k, v)| (k.clone(), node(v)))),
        scalar => super::bigint::to_template(scalar),
    }
}

/// A mapping node of [`inventory_yaml`]'s document.
pub(crate) fn mapping(entries: impl IntoIterator<Item = (String, Value)>) -> Value {
    Value::from_object(
        entries
            .into_iter()
            .map(|(k, v)| (Value::from(k), v))
            .collect::<std::collections::BTreeMap<Value, Value>>(),
    )
}

pub fn register(env: &mut Environment<'static>) {
    // `to_yaml` keeps PyYAML's default flow style (`None`: a collection of scalars inline),
    // `to_nice_yaml` asks for block style everywhere. Both sort keys.
    add_filter(env, "to_yaml", |v: Value, kwargs: Kwargs| {
        dump(&v, None, kwargs, 2, None)
    });
    add_filter(
        env,
        "to_nice_yaml",
        |v: Value, indent: Option<usize>, kwargs: Kwargs| dump(&v, indent, kwargs, 4, Some(false)),
    );
}

#[cfg(test)]
mod tests;
