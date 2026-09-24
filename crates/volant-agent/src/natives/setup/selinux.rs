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
mod tests {
    use super::super::tests::{FakeRoot, probe};
    use super::*;

    /// What would make this red: the library's absence read as SELinux disabled, which the
    /// reference never says without the library.
    #[test]
    fn selinux_follows_the_library_and_hands_back_when_enabled() {
        let fake = FakeRoot::new("selinux");
        let root = fake.root();
        let mut probe = probe();
        let facts = collect(&fake.host(&root, &probe)).unwrap();
        assert_eq!(facts["selinux"], json!({"status": "disabled"}));
        assert_eq!(facts["selinux_python_present"], true);
        probe.selinux = None;
        let facts = collect(&fake.host(&root, &probe)).unwrap();
        assert_eq!(
            facts["selinux"],
            json!({"status": "Missing selinux Python library"})
        );
        assert_eq!(facts["selinux_python_present"], false);
        probe.selinux = Some(true);
        assert!(collect(&fake.host(&root, &probe)).is_err());
    }
}
