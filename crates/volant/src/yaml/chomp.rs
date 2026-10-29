// SPDX-License-Identifier: GPL-3.0-or-later
//! The line break a block scalar keeps when it ends a text that has none.
//!
//! A `|` or `>` scalar keeps the line break after its last line, and `|+` the empty lines after
//! it too. When the scalar ends a text without a final newline, PyYAML has no break to keep,
//! while saphyr keeps one anyway: `k: |\n  a\n  b` loads as `a\nb\n` instead of `a\nb`, and a
//! `command: >` runs with a newline at its end.

use saphyr::{MarkedYaml, YamlData};
use saphyr_parser::ScalarStyle;

/// Drops the line break saphyr adds to the block scalar that ends `text`, if `text` has no final
/// newline.
pub(super) fn unclip_last(docs: &mut [MarkedYaml<'_>], text: &str) {
    if text.ends_with('\n') {
        return;
    }
    let Some(mut node) = docs.last_mut() else {
        return;
    };
    loop {
        node = match &mut node.data {
            YamlData::Sequence(items) => match items.last_mut() {
                Some(item) => item,
                None => return,
            },
            YamlData::Mapping(map) => match map.values_mut().max_by_key(|v| v.span.end.index()) {
                Some(value) => value,
                None => return,
            },
            YamlData::Tagged(_, inner) => inner,
            YamlData::Representation(value, ScalarStyle::Literal | ScalarStyle::Folded, _) => {
                // A comment after the scalar ends it on a line break of its own, and an alias
                // copies a scalar that ended on one.
                let at = |index: usize| {
                    text.char_indices()
                        .nth(index)
                        .map_or(text.len(), |(at, _)| at)
                };
                let alias = text[at(node.span.start.index())..].starts_with('*');
                let after = &text[at(node.span.end.index())..];
                if value.ends_with('\n') && after.trim().is_empty() && !alias {
                    value.to_mut().pop();
                }
                return;
            }
            _ => return,
        };
    }
}
