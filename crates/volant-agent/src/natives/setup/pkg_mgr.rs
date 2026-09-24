// SPDX-License-Identifier: GPL-3.0-or-later
//! The `pkg_mgr` collector for the Debian family, where the reference answers `apt` whatever
//! else is installed, unless `/usr/bin/rpm` exists: then it asks rpm who owns `apt-get`, and the
//! native hands back.

use serde_json::{Map, Value};

use super::Host;

pub fn collect(host: &Host) -> Result<Map<String, Value>, String> {
    if host.root.exists("/usr/bin/rpm") {
        return Err("/usr/bin/rpm exists, and the reference asks it about apt-get".into());
    }
    let mut facts = Map::new();
    facts.insert("pkg_mgr".into(), Value::from("apt"));
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{FakeRoot, probe};
    use super::*;

    #[test]
    fn the_debian_family_uses_apt_unless_rpm_is_there() {
        let fake = FakeRoot::new("pkg-mgr");
        let (root, probe) = (fake.root(), probe());
        fake.write("/usr/bin/dnf", "");
        assert_eq!(
            collect(&fake.host(&root, &probe)).unwrap()["pkg_mgr"],
            "apt"
        );
        fake.write("/usr/bin/rpm", "");
        assert!(collect(&fake.host(&root, &probe)).is_err());
    }
}
