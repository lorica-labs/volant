// SPDX-License-Identifier: GPL-3.0-or-later

use super::super::tests::{FakeRoot, probe};
use super::*;

/// `date_time` is the interpreter's, and a locale the module cannot set hands back.
#[test]
fn the_date_is_the_interpreter_s() {
    let fake = FakeRoot::new("date-time");
    let root = fake.root();
    let mut probe = probe();
    probe.date_time["tz_dst"] = "XDT".into();
    let facts = collect(&fake.host(&root, &probe)).unwrap();
    assert_eq!(facts["date_time"], probe.date_time);
    probe.lc_time = None;
    assert!(collect(&fake.host(&root, &probe)).is_err());
}
