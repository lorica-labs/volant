// SPDX-License-Identifier: GPL-3.0-or-later
//! `ansible._protomatter.debug`, the action ansible-core's own integration targets evaluate
//! expressions with. Not a public interface there, and not one here.

use serde_json::{Map, Value, json};

use crate::executor::prepare::Item;
use crate::playbook::PlayTask;
use crate::template::Templar;

/// The action's name, the only spelling it answers to.
pub(in crate::executor) const DEBUG: &str = "ansible._protomatter.debug";

/// Evaluates each `expression` as written, into `expression_result`. One that fails, an undefined
/// one included, gives `<< error N - message >>` in its place and a warning, as ansible-core
/// 2.19.12's `ReplacingMarkerBehavior` does.
pub(super) fn debug(task: &PlayTask, item: &Item, templar: &Templar) -> Map<String, Value> {
    let expressions = match task.args.get("expression") {
        Some(Value::String(one)) => vec![one.clone()],
        Some(Value::Array(many)) => many
            .iter()
            .map(|e| e.as_str().map_or_else(|| e.to_string(), str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    let mut warnings = Vec::new();
    let results: Vec<Value> = expressions
        .iter()
        .map(|expr| match templar.evaluate(expr, &item.vars) {
            Ok(value) => value,
            Err(err) => {
                let number = warnings.len() + 1;
                warnings.push(format!(
                    "Encountered 1 template error.\nerror {number} - {}",
                    err.0
                ));
                json!(format!("<< error {number} - {} >>", err.0))
            }
        })
        .collect();
    let mut r = Map::new();
    r.insert("_ansible_verbose_always".into(), json!(true));
    r.insert("expression_result".into(), Value::Array(results));
    if !warnings.is_empty() {
        r.insert("warnings".into(), json!(warnings));
    }
    r.insert("changed".into(), json!(false));
    r
}
