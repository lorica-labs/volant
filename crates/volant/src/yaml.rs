// SPDX-License-Identifier: GPL-3.0-or-later
//! YAML loading shared by playbooks, variable files and inventories, with the PyYAML habits
//! Ansible content relies on (`yes`/`no` booleans, leading-zero and underscore-grouped
//! integers), the merge key `<<` and the tags Ansible adds (`!vault`, `!unsafe`).

use std::borrow::Cow;
use std::fmt::Write;

use anyhow::{Context, anyhow, bail};
use saphyr::{Mapping, MarkedYaml, Scalar, Yaml, YamlLoader};
use saphyr_parser::{ScalarStyle, Tag};
use serde_json::Value;

mod limits;
mod merge;
mod walk;

use walk::{lower, mapping_addresses, mapping_starts};

/// The deepest nesting a YAML text may have, for the code that must read a value back.
pub(crate) const MAX_DEPTH: usize = limits::MAX_DEPTH;

/// Loads every document of `text`. Errors name `source`.
///
/// A key written twice in one mapping keeps its last value, as PyYAML does for ansible-core
/// (which only warns). saphyr refuses it unless told otherwise.
pub fn load<'a>(text: &'a str, source: &str) -> anyhow::Result<Vec<Yaml<'a>>> {
    let docs = marked(text, source)?;
    if holds_nothing(text) {
        return Ok(docs.iter().map(|_| Yaml::Value(Scalar::Null)).collect());
    }
    lower_all(docs, source)
}

/// Whether `error`, from [`load`] or [`load_with_origins`], refused a stream for holding a second
/// document. Callers word that refusal the way ansible-core does where it reads the file.
pub fn is_second_document(error: &anyhow::Error) -> bool {
    error
        .root_cause()
        .downcast_ref::<saphyr_parser::ScanError>()
        .is_some_and(|e| e.info() == limits::SECOND_DOCUMENT)
}

/// Every document of `text` with saphyr's positions, a key written twice keeping its last value.
fn marked<'a>(text: &'a str, source: &str) -> anyhow::Result<Vec<MarkedYaml<'a>>> {
    let mut loader = YamlLoader::<MarkedYaml>::default();
    loader.early_parse(false);
    loader.allow_duplicate_keys(true);
    merge::parse(text, source, &mut loader)
        .and_then(|()| loader.error().cloned().map_or(Ok(()), |e| Err(e.into())))
        .with_context(|| format!("{source}: invalid YAML"))?;
    Ok(loader.into_documents())
}

/// Where each mapping of a loaded document starts, as `(line, column)`, both counted from 1:
/// the position ansible-core's `Origin` gives a task. Keyed by the mapping's address in the
/// documents [`load_with_origins`] returned, which stay where they are while nothing changes them.
///
/// The values of every mapping entry are kept the same way, keyed by the value node's address:
/// the reference points a keyword's error at the value (`until: x` at the `x`).
#[derive(Debug, Default)]
pub struct Origins {
    maps: std::collections::HashMap<usize, (usize, usize)>,
    values: std::collections::HashMap<usize, (usize, usize)>,
    /// Where the key of each entry starts, by the address of its value.
    keys: std::collections::HashMap<usize, (usize, usize)>,
}

impl Origins {
    /// Where `map` starts, when it is a mapping of the documents these origins came with.
    pub fn of(&self, map: &Mapping<'_>) -> Option<(usize, usize)> {
        self.maps.get(&std::ptr::from_ref(map).addr()).copied()
    }

    /// Where `value` starts, when it is the value of a mapping entry of those documents.
    pub fn of_value(&self, value: &Yaml<'_>) -> Option<(usize, usize)> {
        self.values.get(&std::ptr::from_ref(value).addr()).copied()
    }

    /// Where the key of `value`'s entry starts, when it is the value of a mapping entry of
    /// those documents: the reference points a task's action at its key (`debug:`).
    pub fn of_key(&self, value: &Yaml<'_>) -> Option<(usize, usize)> {
        self.keys.get(&std::ptr::from_ref(value).addr()).copied()
    }
}

/// [`load`], and where each mapping of the documents starts.
pub fn load_with_origins<'a>(
    text: &'a str,
    source: &str,
) -> anyhow::Result<(Vec<Yaml<'a>>, Origins)> {
    let marked = marked(text, source)?;
    if holds_nothing(text) {
        return Ok((
            marked.iter().map(|_| Yaml::Value(Scalar::Null)).collect(),
            Origins::default(),
        ));
    }
    // The mappings in the order a walk meets them, on both trees: the marked one gives the
    // positions, the lowered one the addresses.
    let mut starts = (Vec::new(), Vec::new(), Vec::new());
    for doc in &marked {
        mapping_starts(doc, &mut starts);
    }
    let docs = lower_all(marked, source)?;
    let mut addresses = (Vec::new(), Vec::new());
    for doc in &docs {
        mapping_addresses(doc, &mut addresses);
    }
    // The two walks meet the same nodes in the same order. Should they ever disagree, no
    // position is better than a wrong one: an error block then shows no `Origin:` line.
    if starts.0.len() != addresses.0.len() || starts.1.len() != addresses.1.len() {
        return Ok((docs, Origins::default()));
    }
    Ok((
        docs,
        Origins {
            maps: addresses.0.into_iter().zip(starts.0).collect(),
            keys: addresses.1.iter().copied().zip(starts.2).collect(),
            values: addresses.1.into_iter().zip(starts.1).collect(),
        },
    ))
}

type Starts = (
    Vec<(usize, usize)>,
    Vec<(usize, usize)>,
    Vec<(usize, usize)>,
);

/// Whether every line of `text` is blank, a comment or a document marker. Such a stream holds
/// only empty documents, which PyYAML reads as null; saphyr hands each back as an empty string.
/// Measured on ansible-core 2.19.12, a vars file holding only `---` and a comment loads as empty.
///
/// Decided from the text rather than from the node: saphyr gives an empty plain scalar, an empty
/// block scalar (`a: |`) and an empty `!!str` the same empty string over a zero-width span, and
/// PyYAML reads the last two as `''`. A stream with no line of content holds neither.
fn holds_nothing(text: &str) -> bool {
    text.lines().all(|line| {
        let line = line.trim();
        let line = line
            .strip_prefix("---")
            .or_else(|| line.strip_prefix("..."))
            .map_or(line, str::trim_start);
        line.is_empty() || line.starts_with('#')
    })
}

/// Every spelling PyYAML reads as `true`. The single source for both the scalar resolver
/// below and [`bool_from_str`]; the `bool` filter's set is a different one and lives with the
/// filter (see [`crate::template`]).
pub const PYYAML_TRUE: &[&str] = &[
    "true", "True", "TRUE", "yes", "Yes", "YES", "on", "On", "ON",
];

/// Every spelling PyYAML reads as `false`, the counterpart of [`PYYAML_TRUE`].
pub const PYYAML_FALSE: &[&str] = &[
    "false", "False", "FALSE", "no", "No", "NO", "off", "Off", "OFF",
];

/// Every document of `docs` without the source positions `MarkedYaml` tracks, each scalar read
/// as PyYAML reads it (see [`scalar`]). Errors name `source`, and carry PyYAML's own text.
fn lower_all<'a>(docs: Vec<MarkedYaml<'a>>, source: &str) -> anyhow::Result<Vec<Yaml<'a>>> {
    docs.into_iter()
        .map(lower)
        .collect::<anyhow::Result<_>>()
        .with_context(|| format!("{source}: invalid YAML"))
}

/// A scalar with the tag and the style it was written with. An alias of it arrives the same way.
///
/// `!unsafe` and `!vault` resolve the text as a plain scalar whatever its quotes are, as
/// ansible-core's constructor does: `!unsafe "5"` is the integer 5. A `!vault` value must come
/// out a string.
fn read_scalar<'a>(
    value: Cow<'a, str>,
    style: ScalarStyle,
    tag: Option<Cow<'a, Tag>>,
) -> anyhow::Result<Yaml<'a>> {
    match tag {
        Some(tag) if tag.handle == "!" && tag.suffix == "unsafe" => {
            let inner = scalar::plain(value).map_err(|e| anyhow!(e))?;
            Ok(Yaml::Tagged(tag, Box::new(inner)))
        }
        Some(tag) if tag.handle == "!" && is_vault(&tag.suffix) => {
            let inner = scalar::plain(value).map_err(|e| anyhow!(e))?;
            read_tagged(tag, inner)
        }
        tag => scalar::read(value, style, tag.as_deref()).map_err(|e| anyhow!(e)),
    }
}

/// A node that carries a tag of its own. Only `!unsafe` and `!vault` are Ansible's.
fn read_tagged<'a>(tag: Cow<'a, Tag>, inner: Yaml<'a>) -> anyhow::Result<Yaml<'a>> {
    if tag.handle == "!" && tag.suffix == "unsafe" {
        return Ok(Yaml::Tagged(tag, Box::new(inner)));
    }
    if tag.handle == "!" && is_vault(&tag.suffix) {
        if !matches!(inner, Yaml::Value(Scalar::String(_))) {
            bail!("The '{tag}' tag requires a string value.");
        }
        return Ok(Yaml::Tagged(tag, Box::new(inner)));
    }
    Err(anyhow!(scalar::no_constructor(&tag)))
}

fn is_vault(suffix: &str) -> bool {
    suffix == "vault" || suffix == "vault-encrypted"
}

/// The value under `key` when `node` is a mapping, `None` otherwise or when absent.
pub fn field<'a>(node: &'a Yaml<'a>, key: &str) -> Option<&'a Yaml<'a>> {
    node.as_mapping_get(key)
}

/// PyYAML's boolean spelling, which Ansible content uses for keywords like `ignore_errors: yes`.
/// `load` already resolves an unquoted `yes`/`no`/`on`/`off` to `Scalar::Boolean` (matching
/// PyYAML's own reading of a variable's value), so the string arm below only ever fires for a
/// *quoted* spelling: a keyword field like `gather_facts: "yes"` is still a boolean to Ansible,
/// even though the same quoting keeps a generic data value a string in `to_json`.
///
/// `0` and `1` count too. Measured on ansible-core 2.19.12: `gather_facts: 1` gathers facts and
/// `gather_facts: 0` does not, while `gather_facts: 2` refuses the load with `The value 2 could
/// not be converted to 'bool'.` Any other integer is not a boolean here either.
pub fn as_bool(node: &Yaml) -> Option<bool> {
    match node {
        Yaml::Value(Scalar::Boolean(b)) => Some(*b),
        Yaml::Value(Scalar::String(s)) => bool_from_str(s.as_ref()),
        Yaml::Value(Scalar::Integer(0)) => Some(false),
        Yaml::Value(Scalar::Integer(1)) => Some(true),
        _ => None,
    }
}

/// The spellings above, on their own, for the places that read a boolean out of plain text
/// rather than out of a YAML node: `ansible.cfg` keys and the environment variables that
/// override them, which Ansible reads with the same set of words.
pub fn bool_from_str(text: &str) -> Option<bool> {
    if PYYAML_TRUE.contains(&text) {
        return Some(true);
    }
    PYYAML_FALSE.contains(&text).then_some(false)
}

/// A string under `!unsafe` is data, never a template. Where it holds template syntax it is
/// stored as a one-expression template that renders back to its own text through
/// `volant_unsafe`, which also stops the render from going over the result again. Every string
/// leaf of a list or mapping is treated the same way; mapping keys are never rendered.
pub(crate) fn unsafe_data(mut value: Value) -> Value {
    let mut open = vec![&mut value];
    while let Some(node) = open.pop() {
        match node {
            Value::String(s) if crate::template::Templar::is_template(s) => {
                *s = wrap(s, "volant_unsafe");
            }
            Value::Array(items) => open.extend(items.iter_mut()),
            Value::Object(map) => open.extend(map.values_mut()),
            _ => {}
        }
    }
    value
}

/// `text` as data that is never rendered, whatever it holds.
pub(crate) fn unsafe_string(text: &str) -> String {
    wrap(text, "volant_unsafe")
}

/// `text` as the one-expression template `{{ "<text>" | <filter> }}`.
fn wrap(text: &str, filter: &str) -> String {
    let mut lit = String::from("{{ \"");
    for c in text.chars() {
        match c {
            '\\' => lit.push_str(r"\\"),
            '"' => lit.push_str(r#"\""#),
            '\n' => lit.push_str(r"\n"),
            '\r' => lit.push_str(r"\r"),
            '\t' => lit.push_str(r"\t"),
            c if c.is_control() => _ = write!(lit, "\\u{:04x}", c as u32),
            c => lit.push(c),
        }
    }
    lit.push_str("\" | ");
    lit.push_str(filter);
    lit.push_str(" }}");
    lit
}

/// The text `unsafe_data` wrapped, when `s` is exactly such a wrapper and nothing else. Read
/// back by hand, never through the templater: `s` may be any string of a variable map.
pub(crate) fn unsafe_text(s: &str) -> Option<String> {
    unwrapped(s, "volant_unsafe")
}

/// What a `!vault` value loads as: its ciphertext in a wrapper that decrypts when the value is
/// rendered, so a value nothing reads is never decrypted. The result is data, as `!unsafe` is:
/// measured on ansible-core 2.19.12, `{{ x }}` inside a decrypted value is not rendered.
pub(crate) fn vault_data(ciphertext: &str) -> String {
    wrap(ciphertext, "volant_vault")
}

/// The ciphertext `vault_data` wrapped, read back by hand like [`unsafe_text`].
pub(crate) fn vault_text(s: &str) -> Option<String> {
    unwrapped(s, "volant_vault")
}

fn unwrapped(s: &str, filter: &str) -> Option<String> {
    let body = s
        .strip_prefix("{{ \"")?
        .strip_suffix(format!("\" | {filter} }}}}").as_str())?;
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return None,
            '\\' => match chars.next()? {
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                }
                _ => return None,
            },
            c => out.push(c),
        }
    }
    Some(out)
}

/// Converts a node to JSON. `!vault` and `!unsafe` load as data, see `vault_data` and
/// `unsafe_data`.
pub fn to_json(node: &Yaml) -> anyhow::Result<Value> {
    walk::to_json(node)
}

mod scalar;

#[cfg(test)]
mod tests;
