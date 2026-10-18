// SPDX-License-Identifier: GPL-3.0-or-later

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
