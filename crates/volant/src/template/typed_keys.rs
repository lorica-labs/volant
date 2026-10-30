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

/// Whether `key` of `map` was `None`, which Python's `json.dumps` writes `null`.
pub(crate) fn is_none(map: &serde_json::Map<String, serde_json::Value>, key: &str) -> bool {
    key == "None"
        && keys(map).is_some_and(|keys| {
            map.keys()
                .zip(keys)
                .any(|(text, typed)| text == key && typed.is_none())
        })
}

/// The typed value of the template key `key`, and the text the variables hold it under, or
/// `None` for a string.
pub(crate) fn of_template(key: &Value) -> Option<(String, Typed)> {
    use minijinja::value::ValueKind;
    match key.kind() {
        ValueKind::None | ValueKind::Undefined => Some(("None".to_string(), Typed::None)),
        ValueKind::Bool => Some((key.is_true().to_string(), Typed::Bool(key.is_true()))),
        // `as_i64` takes a float with no fraction too: the serialized number tells them apart.
        ValueKind::Number => match serde_json::to_value(key).ok()? {
            serde_json::Value::Number(n) if !n.is_f64() => {
                let n = n.as_i64()?;
                Some((n.to_string(), Typed::Int(n)))
            }
            serde_json::Value::Number(n) => {
                let text = crate::playbook::python_repr(&serde_json::Value::Number(n.clone()));
                Some((text, Typed::Float(n.as_f64()?)))
            }
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use minijinja::Value;

    /// A YAML key read by a template keeps its type, and goes back to JSON as its text.
    #[test]
    fn a_yaml_key_keeps_its_type_through_the_variables() {
        let docs = crate::yaml::load("1: one\n2.5: f\n~: n\nx: s\n", "t.yml").unwrap();
        let json = crate::yaml::to_json(&docs[0]).unwrap();
        let read = crate::template::bigint::to_template(&json);
        let keys: Vec<Value> = read.try_iter().unwrap().collect();
        let want = [
            Value::from(1),
            Value::from(2.5),
            Value::from(()),
            Value::from("x"),
        ];
        assert_eq!(keys, want);
        assert!(read.get_item(&Value::from("1")).unwrap().is_undefined());
        let back = crate::template::bigint::to_json(&read).unwrap();
        assert_eq!(back, json);
        // A mapping a template builds keeps its keys typed the same way.
        let built = Value::from_pairs([(Value::from(7), Value::from("seven"))]);
        let stored = crate::template::bigint::to_json(&built).unwrap();
        let again: Vec<Value> = crate::template::bigint::to_template(&stored)
            .try_iter()
            .unwrap()
            .collect();
        assert_eq!(again, [Value::from(7)]);
    }
}
