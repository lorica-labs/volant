// SPDX-License-Identifier: GPL-3.0-or-later

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
