// SPDX-License-Identifier: GPL-3.0-or-later
//! The `cmdline` collector: `/proc/cmdline` split by `shlex.split(data, posix=False)`, once with
//! the last value of a repeated key winning (`cmdline`) and once collecting them (`proc_cmdline`).

use serde_json::{Map, Value};

use super::Host;

pub fn collect(host: &Host) -> Map<String, Value> {
    let mut facts = Map::new();
    let Some(data) = host.root.content("/proc/cmdline") else {
        return facts;
    };
    let pieces = split_non_posix(&data).unwrap_or_default();
    let mut last = Map::new();
    let mut all = Map::new();
    for piece in &pieces {
        let (key, value) = match piece.split_once('=') {
            Some((key, value)) => (key, Value::from(value)),
            None => (piece.as_str(), Value::Bool(true)),
        };
        last.insert(key.to_string(), value.clone());
        match (all.get_mut(key), value) {
            (_, Value::Bool(true)) => {
                all.insert(key.to_string(), Value::Bool(true));
            }
            (Some(Value::Array(values)), value) => values.push(value),
            (Some(previous), value) => *previous = Value::Array(vec![previous.clone(), value]),
            (None, value) => {
                all.insert(key.to_string(), value);
            }
        }
    }
    facts.insert("cmdline".into(), Value::Object(last));
    facts.insert("proc_cmdline".into(), Value::Object(all));
    facts
}

/// `shlex.split(data, posix=False)`: words split on whitespace; a word that starts with a quote
/// runs to the matching quote, keeps both, and ends there. `None` for an unclosed quote, where
/// the reference's `ValueError` leaves both facts empty.
fn split_non_posix(data: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut chars = data.chars();
    let mut word = String::new();
    while let Some(c) = chars.next() {
        if matches!(c, ' ' | '\t' | '\r' | '\n') {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else if word.is_empty() && matches!(c, '"' | '\'') {
            word.push(c);
            loop {
                let next = chars.next()?;
                word.push(next);
                if next == c {
                    break;
                }
            }
            words.push(std::mem::take(&mut word));
        } else {
            word.push(c);
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    Some(words)
}

#[cfg(test)]
mod tests;
