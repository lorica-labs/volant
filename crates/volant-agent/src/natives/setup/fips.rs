// SPDX-License-Identifier: GPL-3.0-or-later
//! The `fips` collector: always present, true only when the kernel says `1`.

use serde_json::{Map, Value};

use super::Host;

pub fn collect(host: &Host) -> Map<String, Value> {
    let enabled = host
        .root
        .content("/proc/sys/crypto/fips_enabled")
        .as_deref()
        == Some("1");
    let mut facts = Map::new();
    facts.insert("fips".into(), Value::Bool(enabled));
    facts
}

#[cfg(test)]
mod tests {
    use super::super::tests::{FakeRoot, probe};
    use super::*;

    #[test]
    fn fips_is_true_only_for_a_one() {
        let fake = FakeRoot::new("fips");
        let (root, probe) = (fake.root(), probe());
        assert_eq!(collect(&fake.host(&root, &probe))["fips"], false, "no file");
        fake.write("/proc/sys/crypto/fips_enabled", "0\n");
        assert_eq!(collect(&fake.host(&root, &probe))["fips"], false);
        fake.write("/proc/sys/crypto/fips_enabled", " 1\n");
        assert_eq!(collect(&fake.host(&root, &probe))["fips"], true);
    }
}
