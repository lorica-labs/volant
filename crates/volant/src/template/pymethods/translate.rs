// SPDX-License-Identifier: GPL-3.0-or-later
//! `str.translate` and `str.maketrans`.

use std::collections::HashMap;

use minijinja::value::ValueKind;
use minijinja::{Error, Value};

use super::args::{Call, raise, type_of};
use super::limits::check;

/// `translate(table)`: each character is looked up by its code point in a mapping or list.
pub(super) fn translate(s: &str, table: &Value) -> Result<Value, Error> {
    if !matches!(table.kind(), ValueKind::Map | ValueKind::Seq) {
        return Err(raise(format!(
            "'{}' object is not subscriptable",
            type_of(table)
        )));
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let hit = table
            .get_item(&Value::from(u32::from(c)))
            .ok()
            .filter(|v| !v.is_undefined());
        match hit {
            None => out.push(c),
            Some(v) if v.is_none() => {}
            Some(v) if v.as_str().is_some() => {
                out.push_str(v.as_str().unwrap_or_default());
                // A table can map one character to a long text.
                check(out.len() as u128)?;
            }
            Some(v) if v.kind() == ValueKind::Number && v.is_integer() => {
                let mapped = v
                    .as_i64()
                    .and_then(|n| u32::try_from(n).ok())
                    .and_then(char::from_u32)
                    .ok_or_else(|| raise("character mapping must be in range(0x110000)"))?;
                out.push(mapped);
            }
            Some(_) => return Err(raise("character mapping must return integer, None or str")),
        }
    }
    Ok(Value::from(out))
}

/// `maketrans(x[, y[, z]])`: a table from code points to code points or text.
pub(super) fn maketrans(call: &Call<'_>) -> Result<Value, Error> {
    let args = call.between("maketrans", 1, 3)?;
    let mut table: Vec<(Value, Value)> = Vec::new();
    if args.len() == 1 {
        let map = &args[0];
        if map.kind() != ValueKind::Map {
            return Err(raise(
                "if you give only one argument to maketrans it must be a dict",
            ));
        }
        for key in map.try_iter()? {
            let code = match key.as_str() {
                Some(t) => {
                    let mut chars = t.chars();
                    match (chars.next(), chars.next()) {
                        (Some(c), None) => Value::from(u32::from(c)),
                        _ => {
                            return Err(raise(
                                "string keys in translate table must be of length 1",
                            ));
                        }
                    }
                }
                None if key.kind() == ValueKind::Number && key.is_integer() => key.clone(),
                None => {
                    return Err(raise("keys in translate table must be strings or integers"));
                }
            };
            table.push((code, map.get_item(&key)?));
        }
        return Ok(Value::from_pairs(table));
    }
    let from = args[0].as_str().ok_or_else(|| {
        raise(format!(
            "maketrans() argument 1 must be str, not {}",
            type_of(&args[0])
        ))
    })?;
    let to = args[1].as_str().ok_or_else(|| {
        raise(format!(
            "maketrans() argument 2 must be str, not {}",
            type_of(&args[1])
        ))
    })?;
    if from.chars().count() != to.chars().count() {
        return Err(raise(
            "the first two maketrans arguments must have equal length",
        ));
    }
    // A code point keeps the place of its first entry when a later one sets it again, as in a
    // Python dict.
    let mut place: HashMap<u32, usize> = HashMap::new();
    let mut set = |table: &mut Vec<(Value, Value)>, code: char, to: Value| {
        let code = u32::from(code);
        if let Some(&at) = place.get(&code) {
            table[at].1 = to;
        } else {
            place.insert(code, table.len());
            table.push((Value::from(code), to));
        }
    };
    for (a, b) in from.chars().zip(to.chars()) {
        set(&mut table, a, Value::from(u32::from(b)));
    }
    if let Some(drop) = args.get(2) {
        let drop = drop.as_str().ok_or_else(|| {
            raise(format!(
                "maketrans() argument 3 must be str, not {}",
                type_of(drop)
            ))
        })?;
        for c in drop.chars() {
            set(&mut table, c, Value::from(()));
        }
    }
    Ok(Value::from_pairs(table))
}
