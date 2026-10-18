// SPDX-License-Identifier: GPL-3.0-or-later

use super::super::tests::{FakeRoot, probe};
use super::*;

/// SELinux follows the library: its absence is never read as disabled, and enabled hands back.
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
