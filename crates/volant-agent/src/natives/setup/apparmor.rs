// SPDX-License-Identifier: GPL-3.0-or-later
//! The `apparmor` collector: whether the kernel exposes AppArmor's security filesystem.

use serde_json::{Map, Value, json};

use super::Host;

pub fn collect(host: &Host) -> Map<String, Value> {
    let status = if host.root.exists("/sys/kernel/security/apparmor") {
        "enabled"
    } else {
        "disabled"
    };
    let mut facts = Map::new();
    facts.insert("apparmor".into(), json!({"status": status}));
    facts
}

#[cfg(test)]
mod tests;
