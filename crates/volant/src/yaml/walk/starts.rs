// SPDX-License-Identifier: GPL-3.0-or-later
//! Where each mapping of a document starts, and where each key and value of it starts.

use saphyr::{MarkedYaml, Yaml, YamlData};
use saphyr_parser::ScalarStyle;

use super::super::Starts;

/// Where each mapping of the source tree starts, and each key and value of it, in the order a
/// walk down the tree meets them. A `|` or `>` value starts at its indicator, as in the
/// reference: saphyr's span starts at its first line of text.
pub(in crate::yaml) fn mapping_starts(root: &MarkedYaml<'_>, text: &str, out: &mut Starts) {
    enum Work<'t, 'a> {
        Node(&'t MarkedYaml<'a>),
        Entry(&'t MarkedYaml<'a>, &'t MarkedYaml<'a>),
        Item(&'t MarkedYaml<'a>),
    }
    // A tagged node starts at its tag, as PyYAML marks it: `var: !unsafe x` is at the `!`. The
    // parser marks the content, so the tag is found in front of it on the same line.
    let at = |n: &MarkedYaml<'_>| {
        let (line, col) = (n.span.start.line(), n.span.start.col() + 1);
        let tagged = match &n.data {
            YamlData::Tagged(tag, _) | YamlData::Representation(_, _, Some(tag)) => {
                text.lines().nth(line - 1).and_then(|row| {
                    let before: String = row.chars().take(col - 1).collect();
                    let tag = format!("{}{}", tag.handle, tag.suffix);
                    let head = before.trim_end().strip_suffix(tag.as_str())?;
                    Some(head.chars().count() + 1)
                })
            }
            _ => None,
        };
        (line, tagged.unwrap_or(col))
    };
    let mut work = vec![Work::Node(root)];
    while let Some(item) = work.pop() {
        match item {
            Work::Node(node) => match &node.data {
                YamlData::Sequence(items) => work.extend(items.iter().rev().map(Work::Item)),
                YamlData::Mapping(map) => {
                    out.0.push(at(node));
                    let entries: Vec<_> = map.iter().map(|(k, v)| Work::Entry(k, v)).collect();
                    work.extend(entries.into_iter().rev());
                }
                YamlData::Tagged(_, inner) => work.push(Work::Node(inner)),
                _ => {}
            },
            Work::Entry(key, value) => {
                out.1
                    .push(block_indicator(value, text).unwrap_or_else(|| at(value)));
                out.2.push(at(key));
                work.push(Work::Node(value));
                work.push(Work::Node(key));
            }
            // An item of a sequence is placed like a value, its own start standing for its key.
            Work::Item(item) => {
                out.1.push(at(item));
                out.2.push(at(item));
                work.push(Work::Node(item));
            }
        }
    }
}

/// The address of each mapping of the lowered tree, and of each value in it, in the order
/// [`mapping_starts`] lists the positions.
pub(in crate::yaml) fn mapping_addresses(root: &Yaml<'_>, out: &mut (Vec<usize>, Vec<usize>)) {
    enum Work<'t, 'a> {
        Node(&'t Yaml<'a>),
        Entry(&'t Yaml<'a>, &'t Yaml<'a>),
        Item(&'t Yaml<'a>),
    }
    let mut work = vec![Work::Node(root)];
    while let Some(item) = work.pop() {
        match item {
            Work::Node(node) => match node {
                Yaml::Sequence(items) => work.extend(items.iter().rev().map(Work::Item)),
                Yaml::Mapping(map) => {
                    out.0.push(std::ptr::from_ref(map).addr());
                    let entries: Vec<_> = map.iter().map(|(k, v)| Work::Entry(k, v)).collect();
                    work.extend(entries.into_iter().rev());
                }
                Yaml::Tagged(_, inner) => work.push(Work::Node(inner)),
                _ => {}
            },
            Work::Entry(key, value) => {
                out.1.push(std::ptr::from_ref(value).addr());
                work.push(Work::Node(value));
                work.push(Work::Node(key));
            }
            Work::Item(item) => {
                out.1.push(std::ptr::from_ref(item).addr());
                work.push(Work::Node(item));
            }
        }
    }
}

/// The line and column of the `|` or `>` that opens `node`, a block scalar.
fn block_indicator(node: &MarkedYaml<'_>, text: &str) -> Option<(usize, usize)> {
    let YamlData::Representation(_, ScalarStyle::Literal | ScalarStyle::Folded, _) = &node.data
    else {
        return None;
    };
    let start = text.char_indices().nth(node.span.start.index())?.0;
    let at = text[..start].rfind(['|', '>'])?;
    let line_start = text[..at].rfind('\n').map_or(0, |n| n + 1);
    let line = text[..at].matches('\n').count() + 1;
    Some((line, text[line_start..at].chars().count() + 1))
}
