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
mod tests {
    use super::super::tests::{FakeRoot, probe};
    use super::*;

    #[test]
    fn apparmor_is_enabled_when_its_filesystem_is_there() {
        let fake = FakeRoot::new("apparmor");
        let (root, probe) = (fake.root(), probe());
        assert_eq!(
            collect(&fake.host(&root, &probe))["apparmor"],
            json!({"status": "disabled"})
        );
        fake.mkdir("/sys/kernel/security/apparmor");
        assert_eq!(
            collect(&fake.host(&root, &probe))["apparmor"],
            json!({"status": "enabled"})
        );
    }
}
