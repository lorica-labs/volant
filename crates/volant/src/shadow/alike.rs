// SPDX-License-Identifier: GPL-3.0-or-later
//! Whether the arguments Volant sends are the ones a module ran with, allowing for what the
//! module itself converts.

use serde_json::{Map, Value, json};

/// The keys Volant sent whose value differs from the one the module ran with, and the reason of
/// each argument not compared because it is unknown. Keys only in the recording are the module's
/// defaults, Volant's own keys start with `_volant`, and a value the module hid is not compared.
pub(super) fn different_args(
    recorded: &Map<String, Value>,
    sent: &Map<String, Value>,
) -> (Map<String, Value>, Map<String, Value>, Vec<String>) {
    let (mut ansible, mut volant, mut unknown) = (Map::new(), Map::new(), Vec::new());
    for (key, value) in sent {
        let Some(theirs) = recorded.get(key) else {
            continue;
        };
        if let Some(reason) = crate::template::unknown::reason_in_json(value) {
            unknown.push(reason);
            continue;
        }
        if key.starts_with("_volant")
            || theirs == "VALUE_SPECIFIED_IN_NO_LOG_PARAMETER"
            || alike(theirs, value)
        {
            continue;
        }
        ansible.insert(key.clone(), theirs.clone());
        volant.insert(key.clone(), value.clone());
    }
    (ansible, volant, unknown)
}

/// Equal once the module's own conversion of what it was given is allowed for: `yes` for
/// `true`, `"8"` for `8`, `"a,b"` for a list.
fn alike(theirs: &Value, ours: &Value) -> bool {
    match (theirs, ours) {
        (Value::Bool(b), Value::String(s)) => {
            let s = s.to_ascii_lowercase();
            let words: &[&str] = if *b {
                &["yes", "on", "true", "1", "y", "t"]
            } else {
                &["no", "off", "false", "0", "n", "f"]
            };
            words.contains(&s.as_str())
        }
        (Value::Number(n), Value::String(s)) | (Value::String(s), Value::Number(n)) => {
            s.trim() == n.to_string()
        }
        (Value::String(s), Value::Bool(b)) => s == &b.to_string(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| alike(a, b))
        }
        (Value::Array(x), Value::String(s)) => {
            let parts: Vec<&str> = s.split(',').map(str::trim).collect();
            x.len() == parts.len() && x.iter().zip(parts).all(|(a, b)| alike(a, &json!(b)))
        }
        (Value::Object(x), Value::Object(y)) => {
            y.iter().all(|(k, b)| x.get(k).is_none_or(|a| alike(a, b)))
        }
        _ => theirs == ours,
    }
}

/// `args` with the recording's directory, `from`, read as the replay's, `to`, in every string.
pub(super) fn moved(
    args: &Map<String, Value>,
    (from, to): &(String, String),
) -> Map<String, Value> {
    fn walk(value: &Value, from: &str, to: &str) -> Value {
        match value {
            Value::String(s) => Value::String(s.replace(from, to)),
            Value::Array(items) => Value::Array(items.iter().map(|v| walk(v, from, to)).collect()),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), walk(v, from, to)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    if from.is_empty() || from == to {
        return args.clone();
    }
    args.iter()
        .map(|(k, v)| (k.clone(), walk(v, from, to)))
        .collect()
}

#[cfg(test)]
#[path = "alike_tests.rs"]
mod tests;
