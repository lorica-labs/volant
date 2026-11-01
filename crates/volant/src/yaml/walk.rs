// SPDX-License-Identifier: GPL-3.0-or-later
//! The walks over a loaded document: dropping the source positions, listing where each mapping
//! starts, and converting to JSON.
//!
//! None of them calls itself. Each keeps its own stack of the nodes it has open, so the stack of
//! the thread holds one small frame however deep the document goes: a recursive walk took about
//! 3.6 KB of stack for each level in a debug build, which let 489 levels fill a 2 MiB thread.

use std::borrow::Cow;

use anyhow::{anyhow, bail};
use saphyr::{Mapping, MarkedYaml, Scalar, Tag, Yaml, YamlData};
use serde_json::{Map, Value};

use crate::template::typed_keys::Typed;

mod starts;
pub(super) use starts::{mapping_addresses, mapping_starts};

use super::{is_vault, read_scalar, read_tagged, scalar, unsafe_data, vault_data};

/// A collection of the source tree whose children are being lowered.
#[expect(
    clippy::large_enum_variant,
    reason = "the frames live in a heap vector"
)]
enum Lowering<'a> {
    Sequence {
        items: std::vec::IntoIter<MarkedYaml<'a>>,
        out: Vec<Yaml<'a>>,
    },
    Mapping {
        entries: Box<dyn Iterator<Item = (MarkedYaml<'a>, MarkedYaml<'a>)> + 'a>,
        /// The value of the entry whose key is being lowered.
        value: Option<MarkedYaml<'a>>,
        /// The key of the entry whose value is being lowered.
        key: Option<Yaml<'a>>,
        out: Mapping<'a>,
    },
    Tagged(Cow<'a, Tag>),
}

/// Whether `node` has no children to walk.
fn is_leaf(node: &MarkedYaml<'_>) -> bool {
    matches!(node.data, YamlData::Value(_) | YamlData::Representation(..))
}

/// A scalar of the source tree, read as PyYAML reads it from its text, style and tag.
fn lower_scalar(node: MarkedYaml<'_>) -> anyhow::Result<Yaml<'_>> {
    Ok(match node.data {
        YamlData::Representation(value, style, tag) => read_scalar(value, style, tag)?,
        YamlData::Value(scalar) => Yaml::Value(scalar),
        _ => Yaml::BadValue,
    })
}

/// Drops the source positions `MarkedYaml` tracks once loading is done, reading each scalar as
/// PyYAML does (see [`scalar`]). Every other function in this module keeps working on the plain
/// `Yaml` tree it already expected. A tag with no constructor, or a `!vault` over a value that is
/// not a string, is an error with PyYAML's own text.
pub(super) fn lower(root: MarkedYaml<'_>, strict: bool) -> anyhow::Result<Yaml<'_>> {
    let mut open: Vec<Lowering<'_>> = Vec::new();
    let mut next = Some(root);
    let mut done: Option<Yaml<'_>> = None;
    loop {
        if let Some(node) = next.take() {
            match node.data {
                YamlData::Value(_) | YamlData::Representation(..) => {
                    done = Some(lower_scalar(node)?);
                }
                // A collection of scalars, the usual one, is lowered at once, with no frame.
                YamlData::Sequence(items) if items.iter().all(is_leaf) => {
                    done = Some(Yaml::Sequence(
                        items
                            .into_iter()
                            .map(lower_scalar)
                            .collect::<anyhow::Result<_>>()?,
                    ));
                }
                YamlData::Mapping(map) if map.iter().all(|(k, v)| is_leaf(k) && is_leaf(v)) => {
                    let mut out = Mapping::new();
                    for (k, v) in map {
                        insert(
                            &mut out,
                            key_of(lower_scalar(k)?, strict)?,
                            lower_scalar(v)?,
                        );
                    }
                    done = Some(Yaml::Mapping(out));
                }
                YamlData::Sequence(items) => open.push(Lowering::Sequence {
                    out: Vec::with_capacity(items.len()),
                    items: items.into_iter(),
                }),
                YamlData::Mapping(map) => open.push(Lowering::Mapping {
                    entries: Box::new(map.into_iter()),
                    value: None,
                    key: None,
                    out: Mapping::new(),
                }),
                YamlData::Tagged(tag, inner) => {
                    open.push(Lowering::Tagged(tag));
                    next = Some(*inner);
                    continue;
                }
                YamlData::Alias(id) => done = Some(Yaml::Alias(id)),
                YamlData::BadValue => done = Some(Yaml::BadValue),
            }
        }
        if let Some(child) = done.take() {
            match open.last_mut() {
                None => return Ok(child),
                Some(Lowering::Sequence { out, .. }) => out.push(child),
                Some(Lowering::Mapping {
                    value, key, out, ..
                }) => {
                    if value.is_some() {
                        *key = Some(key_of(child, strict)?);
                    } else if let Some(key) = key.take() {
                        insert(out, key, child);
                    }
                }
                Some(Lowering::Tagged(_)) => {
                    if let Some(Lowering::Tagged(tag)) = open.pop() {
                        done = Some(read_tagged(tag, child)?);
                    }
                    continue;
                }
            }
        }
        match open.last_mut() {
            Some(Lowering::Sequence { items, out }) => match items.next() {
                Some(item) => next = Some(item),
                None => done = Some(Yaml::Sequence(std::mem::take(out))),
            },
            Some(Lowering::Mapping {
                entries,
                value,
                out,
                ..
            }) => match value.take() {
                Some(value) => next = Some(value),
                None => match entries.next() {
                    Some((key, entry)) => {
                        *value = Some(entry);
                        next = Some(key);
                    }
                    None => done = Some(Yaml::Mapping(std::mem::take(out))),
                },
            },
            _ => {}
        }
        if done.is_some() {
            open.pop();
        }
    }
}

/// `key: value` into `out` as a Python dict takes it: a key equal by value to one already there
/// (`1`, `1.0` and `true` are one key) keeps the first key's spelling and takes the last value.
/// Measured on ansible-core 2.19.12: `{1: one, true: t}` is `{"1": "t"}`.
fn insert<'a>(out: &mut Mapping<'a>, key: Yaml<'a>, value: Yaml<'a>) {
    let same =
        key_number(&key).and_then(|n| out.keys().find(|k| key_number(k).as_ref() == Some(&n)));
    match same.cloned().and_then(|first| out.get_mut(&first)) {
        Some(kept) => *kept = value,
        None => {
            out.insert(key, value);
        }
    }
}

/// The number a key is to Python's equality, when it is one: a boolean is 0 or 1, and a float
/// with no fraction is the integer it equals. Held as text so that a key compares exactly.
pub(super) fn key_number(key: &Yaml<'_>) -> Option<String> {
    match key {
        Yaml::Value(Scalar::Integer(i)) => Some(i.to_string()),
        Yaml::Value(Scalar::Boolean(b)) => Some(u8::from(*b).to_string()),
        Yaml::Value(Scalar::FloatingPoint(f)) => {
            let f = f.into_inner();
            Some(if f.fract() == 0.0 && f.abs() < 9.0e15 {
                format!("{f:.0}")
            } else {
                format!("{f:?}")
            })
        }
        _ => None,
    }
}

/// A resolved key as Python's `repr` names it in a warning: `True`, `1`, `0.0`.
pub(super) fn key_repr(key: Option<&Yaml<'_>>) -> String {
    match key {
        Some(Yaml::Value(Scalar::Boolean(true))) => "True".to_string(),
        Some(Yaml::Value(Scalar::Boolean(false))) => "False".to_string(),
        Some(Yaml::Value(Scalar::Integer(i))) => i.to_string(),
        Some(Yaml::Value(scalar @ Scalar::FloatingPoint(_))) => {
            crate::playbook::python_repr(&scalar_json(scalar, false))
        }
        _ => "None".to_string(),
    }
}

/// A mapping key as the reference loads it: `!unsafe a` is the key `a`. Keys are never
/// rendered, so the tag has nothing left to say.
///
/// `strict` is PyYAML's own loader, which `from_yaml` uses: it has no constructor for the tag.
fn key_of(key: Yaml<'_>, strict: bool) -> anyhow::Result<Yaml<'_>> {
    match key {
        Yaml::Tagged(tag, _) if strict && tag.handle == "!" && tag.suffix == "unsafe" => {
            Err(anyhow!(scalar::no_constructor(&tag)))
        }
        Yaml::Tagged(tag, inner) if tag.handle == "!" && tag.suffix == "unsafe" => Ok(*inner),
        key => Ok(key),
    }
}

fn is_scalar(node: &Yaml<'_>) -> bool {
    matches!(node, Yaml::Value(_))
}

fn scalar_json(scalar: &Scalar<'_>, unsafe_text: bool) -> Value {
    match scalar {
        Scalar::Null => Value::Null,
        Scalar::Boolean(b) => Value::Bool(*b),
        Scalar::Integer(i) => Value::from(*i),
        Scalar::FloatingPoint(f) => serde_json::Number::from_f64(f.into_inner())
            .map_or_else(|| Value::String(f.to_string()), Value::Number),
        Scalar::String(s) if unsafe_text => Value::String(s.to_string()),
        Scalar::String(s) => Value::String(super::escapes::literal_escapes(s)),
    }
}

/// A sequence of scalars as JSON.
fn scalar_array(items: &[Yaml<'_>]) -> Value {
    let scalars = items.iter().filter_map(|n| match n {
        Yaml::Value(scalar) => Some(scalar_json(scalar, false)),
        _ => None,
    });
    Value::Array(scalars.collect())
}

/// A mapping key as a JSON object holds it: a string as written, and a number, a boolean or null
/// as the reference shows it, measured on ansible-core 2.19.12: `1:` is `"1"`, `2.5:` is
/// `"2.5"`, `true:` is `"true"` and `~:` is `"None"`. `template::keys` finds them by value.
pub(super) fn key_text(key: &Yaml<'_>) -> anyhow::Result<String> {
    Ok(match key {
        Yaml::Value(Scalar::Integer(i)) => i.to_string(),
        Yaml::Value(Scalar::FloatingPoint(f)) => {
            crate::playbook::python_repr(&scalar_json(&Scalar::FloatingPoint(*f), false))
        }
        Yaml::Value(Scalar::Boolean(b)) => b.to_string(),
        Yaml::Value(Scalar::Null) => "None".to_string(),
        other => other
            .as_str()
            .ok_or_else(|| anyhow!("mapping keys must be strings"))?
            .to_string(),
    })
}

/// What a template reads a key that is no string as: see `template::typed_keys`.
fn key_typed(key: &Yaml<'_>) -> Option<Typed> {
    Some(match key {
        Yaml::Value(Scalar::Integer(i)) => Typed::Int(*i),
        Yaml::Value(Scalar::FloatingPoint(f)) => Typed::Float(f.into_inner()),
        Yaml::Value(Scalar::Boolean(b)) => Typed::Bool(*b),
        Yaml::Value(Scalar::Null) => Typed::None,
        _ => return None,
    })
}

/// A mapping of scalars as JSON.
fn scalar_object(map: &Mapping<'_>) -> anyhow::Result<Value> {
    let mut out = Map::new();
    let mut typed = Vec::new();
    for (k, v) in map {
        let key = typed_text(k, &mut typed)?;
        if let Yaml::Value(scalar) = v {
            out.insert(key, scalar_json(scalar, false));
        }
    }
    Ok(finished(&mut out, &typed))
}

/// [`key_text`], noting the key in `typed` when it is no string.
fn typed_text(key: &Yaml<'_>, typed: &mut Vec<(String, Typed)>) -> anyhow::Result<String> {
    let text = key_text(key)?;
    typed.extend(key_typed(key).map(|kind| (text.clone(), kind)));
    Ok(text)
}

/// A converted mapping, its typed keys remembered for the templates that read it.
fn finished(out: &mut Map<String, Value>, typed: &[(String, Typed)]) -> Value {
    crate::template::typed_keys::remember(out, typed);
    Value::Object(std::mem::take(out))
}

/// A collection of the lowered tree whose children are being converted.
enum Converting<'t, 'a> {
    Array {
        items: std::slice::Iter<'t, Yaml<'a>>,
        out: Vec<Value>,
    },
    Object {
        entries: Box<dyn Iterator<Item = (&'t Yaml<'a>, &'t Yaml<'a>)> + 't>,
        key: Option<String>,
        out: Map<String, Value>,
        /// The keys that were no strings: see `template::typed_keys`.
        typed: Vec<(String, Typed)>,
    },
    /// A `!unsafe`, and whether it is the outermost: it wraps the strings below it, once.
    Unsafe { outermost: bool },
}

/// Converts a node to JSON. `!vault` and `!unsafe` load as data, see `vault_data` and
/// `unsafe_data`. A `!unsafe` inside a `!unsafe` wraps nothing more, as wrapping again at each
/// level doubled every string.
pub(super) fn to_json(root: &Yaml) -> anyhow::Result<Value> {
    let mut open: Vec<Converting<'_, '_>> = Vec::new();
    let mut unsafe_levels = 0usize;
    let mut next = Some(root);
    let mut done: Option<Value> = None;
    loop {
        if let Some(node) = next.take() {
            match node {
                Yaml::Value(scalar) => done = Some(scalar_json(scalar, unsafe_levels > 0)),
                // A collection of scalars, the usual one, is converted at once, with no frame.
                Yaml::Sequence(items) if unsafe_levels == 0 && items.iter().all(is_scalar) => {
                    done = Some(scalar_array(items));
                }
                Yaml::Mapping(map)
                    if unsafe_levels == 0
                        && map.iter().all(|(k, v)| is_scalar(k) && is_scalar(v)) =>
                {
                    done = Some(scalar_object(map)?);
                }
                Yaml::Sequence(items) => open.push(Converting::Array {
                    items: items.iter(),
                    out: Vec::with_capacity(items.len()),
                }),
                Yaml::Mapping(map) => open.push(Converting::Object {
                    entries: Box::new(map.iter()),
                    key: None,
                    out: Map::new(),
                    typed: Vec::new(),
                }),
                Yaml::Tagged(tag, inner) if tag.handle == "!" && is_vault(&tag.suffix) => {
                    match &**inner {
                        Yaml::Value(Scalar::String(s)) => {
                            done = Some(Value::String(vault_data(s)));
                        }
                        _ => bail!("!vault takes a string"),
                    }
                }
                Yaml::Tagged(tag, inner) if tag.handle == "!" && tag.suffix == "unsafe" => {
                    open.push(Converting::Unsafe {
                        outermost: unsafe_levels == 0,
                    });
                    unsafe_levels += 1;
                    next = Some(&**inner);
                    continue;
                }
                Yaml::Tagged(tag, inner) if scalar::wide_digits(tag, inner).is_some() => {
                    let digits = scalar::wide_digits(tag, inner).unwrap_or_default();
                    done = Some(Value::Number(digits.parse()?));
                }
                Yaml::Tagged(tag, _) => bail!(scalar::no_constructor(tag)),
                Yaml::Alias(_) => bail!("YAML aliases are not supported yet"),
                Yaml::Representation(..) => bail!("unresolved YAML scalar"),
                Yaml::BadValue => bail!("invalid YAML value"),
            }
        }
        if let Some(child) = done.take() {
            match open.last_mut() {
                None => return Ok(child),
                Some(Converting::Array { out, .. }) => out.push(child),
                Some(Converting::Object { key, out, .. }) => {
                    if let Some(key) = key.take() {
                        out.insert(key, child);
                    }
                }
                Some(Converting::Unsafe { .. }) => {
                    if let Some(Converting::Unsafe { outermost }) = open.pop() {
                        unsafe_levels -= 1;
                        done = Some(if outermost { unsafe_data(child) } else { child });
                    }
                    continue;
                }
            }
        }
        match open.last_mut() {
            Some(Converting::Array { items, out }) => match items.next() {
                Some(item) => next = Some(item),
                None => done = Some(Value::Array(std::mem::take(out))),
            },
            Some(Converting::Object {
                entries,
                key,
                out,
                typed,
            }) => match entries.next() {
                Some((k, v)) => {
                    *key = Some(typed_text(k, typed)?);
                    next = Some(v);
                }
                None => done = Some(finished(out, typed)),
            },
            _ => {}
        }
        if done.is_some() {
            open.pop();
        }
    }
}
