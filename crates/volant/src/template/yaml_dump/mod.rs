// SPDX-License-Identifier: GPL-3.0-or-later
//! `to_yaml` and `to_nice_yaml`: PyYAML's `safe_dump` with `allow_unicode=True`, which is what
//! ansible-core calls. ansible-core dumps through `CSafeDumper` whenever PyYAML was built with
//! libyaml (`module_utils/common/yaml.py`), as it is on the reference controller, so the
//! representer is PyYAML's and the emitter is **libyaml's**. The emitter below keeps the state
//! both share (`column`, `whitespace`, `indention`, the indent stack) and PyYAML's function
//! names, cut down to what a document of plain data reaches: no anchors, no tags, no block
//! scalars. Where libyaml and `emitter.py` differ, libyaml is followed: no `...` after a plain
//! root scalar, a character outside the Basic Multilingual Plane or `\x85` is not printable (the
//! string is double-quoted, `"\U0001F600"`, `"\N"`), a double-quoted string folds only at a
//! space, and a key is simple up to 128 bytes, the empty one included.

use minijinja::value::Kwargs;
use minijinja::{Environment, Value};

use super::add_filter;

mod emitter;
mod scalar;

use emitter::dump;

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
