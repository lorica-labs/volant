// SPDX-License-Identifier: GPL-3.0-or-later
//! The `selinux` collector. Its answer rests on loading libselinux, which the probe does in the
//! module's interpreter. With SELinux enabled the reference reads the policy through the library,
//! and the native hands back.

use serde_json::{Map, Value, json};

use super::Host;

pub fn collect(host: &Host) -> Result<Map<String, Value>, String> {
    let mut facts = Map::new();
    match host.probe.selinux {
        None => {
            facts.insert(
                "selinux".into(),
                json!({"status": "Missing selinux Python library"}),
            );
            facts.insert("selinux_python_present".into(), Value::Bool(false));
        }
        Some(false) => {
            facts.insert("selinux_python_present".into(), Value::Bool(true));
            facts.insert("selinux".into(), json!({"status": "disabled"}));
        }
        Some(true) => return Err("SELinux is enabled".into()),
    }
    Ok(facts)
}

#[cfg(test)]
mod tests;
