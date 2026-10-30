// SPDX-License-Identifier: GPL-3.0-or-later
//! Mapping keys that are a number, a boolean or null, which the variables hold as their text.
//!
//! The variables are JSON, whose object keys are strings, so `crate::yaml` holds `1:` as `"1"`,
//! `2.5:` as `"2.5"`, `true:` as `"true"` and `~:` as `"None"`: what ansible-core 2.19.12 prints
//! for them. A template reads the Python value: `d.keys()` gives `[1, 2.5, None]`, `d["1"]`
//! finds nothing, and `dict2items` gives the int. So the loader, and a template that builds such
//! a mapping, remember which keys of a mapping were typed, by the mapping's list of keys, and a
//! template that reads a mapping with that list gets those keys back typed.
//!
//! A mapping known only by its keys: another mapping with the same keys in the same order, built
//! with string keys (a module's JSON result, say), reads typed too.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};

use minijinja::Value;

/// The value a typed key had.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Typed {
    Int(i64),
    Float(f64),
    Bool(bool),
    None,
}

impl Typed {
    fn value(self) -> Value {
        match self {
            Self::Int(n) => Value::from(n),
            Self::Float(f) => Value::from(f),
            Self::Bool(b) => Value::from(b),
            Self::None => Value::from(()),
        }
    }
}

/// The places of a mapping's typed keys, and their values.
type Places = Vec<(usize, Typed)>;

/// The typed keys of each mapping, by its keys joined.
static KNOWN: LazyLock<Mutex<HashMap<String, Places>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn fingerprint<'a>(keys: impl Iterator<Item = &'a String>) -> String {
    let mut out = String::new();
    for key in keys {
        out.push_str(key);
        out.push('\0');
    }
    out
}

/// Remembers that the keys `typed` names, of the mapping `map`, are typed.
pub(crate) fn remember(
    map: &serde_json::Map<String, serde_json::Value>,
    typed: &[(String, Typed)],
) {
    if typed.is_empty() {
        return;
    }
    let places = typed
        .iter()
        .filter_map(|(text, value)| map.keys().position(|k| k == text).map(|at| (at, *value)))
        .collect();
    KNOWN
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(fingerprint(map.keys()), places);
}

/// Whether `text` is one a typed key is held under.
fn looks_typed(text: &str) -> bool {
    matches!(text, "None" | "true" | "false") || text.parse::<f64>().is_ok()
}

/// The keys of `map` as a template reads them: typed where the mapping was remembered so.
pub(crate) fn keys(map: &serde_json::Map<String, serde_json::Value>) -> Option<Vec<Value>> {
    if !map.keys().any(|k| looks_typed(k)) {
        return None;
    }
    let known = KNOWN.lock().unwrap_or_else(PoisonError::into_inner);
    let places = known.get(&fingerprint(map.keys()))?;
    let mut keys: Vec<Value> = map.keys().map(|k| Value::from(k.as_str())).collect();
    for (at, typed) in places {
        if let Some(key) = keys.get_mut(*at) {
            *key = typed.value();
        }
    }
    Some(keys)
}

/// The typed value of the template key `key`, and the text the variables hold it under, or
/// `None` for a string.
pub(crate) fn of_template(key: &Value) -> Option<(String, Typed)> {
    use minijinja::value::ValueKind;
    match key.kind() {
        ValueKind::None | ValueKind::Undefined => Some(("None".to_string(), Typed::None)),
        ValueKind::Bool => Some((key.is_true().to_string(), Typed::Bool(key.is_true()))),
        ValueKind::Number => match i64::try_from(key.clone()) {
            Ok(n) if key.as_i64().is_some() => Some((n.to_string(), Typed::Int(n))),
            _ => {
                let f = f64::try_from(key.clone()).ok()?;
                let text = crate::playbook::python_repr(&serde_json::Value::Number(
                    serde_json::Number::from_f64(f)?,
                ));
                Some((text, Typed::Float(f)))
            }
        },
        _ => None,
    }
}
