// SPDX-License-Identifier: GPL-3.0-or-later
//! The `env` collector: the module's own environment, which is the interpreter's at start plus
//! the task's `environment`.

use serde_json::{Map, Value};

use super::Host;

pub fn collect(host: &Host) -> Map<String, Value> {
    let env = host
        .env
        .iter()
        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
        .collect();
    let mut facts = Map::new();
    facts.insert("env".into(), Value::Object(env));
    facts
}
