// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

/// Root with every capability, an unprivileged user (measured: `Current: =` gives `['']`), a
/// capability list, and `capsh` missing or failing.
#[test]
fn capabilities_are_read_like_the_reference() {
    assert_eq!(
        parse("Current: =ep\nBounding set =cap_chown"),
        ("False", vec![])
    );
    assert_eq!(parse("Current: =\n"), ("True", vec![String::new()]));
    // The text between the first and second `=`: libcap 2's layout gives the flags, the older
    // `= caps+ep` layout gives the names.
    assert_eq!(
        parse("Current: cap_chown,cap_kill=ep\n"),
        ("True", vec!["ep".to_string()])
    );
    assert_eq!(
        parse("Current: = cap_chown,cap_kill+ep\n"),
        (
            "True",
            vec!["cap_chown".to_string(), "cap_kill+ep".to_string()]
        )
    );
    assert_eq!(parse("nothing"), ("NA", vec![]));
}

/// `capsh` missing, failing, then answering, under a fake root. Linux only: the native runs
/// nowhere else, and `capsh` is a Linux tool.
#[test]
#[cfg(target_os = "linux")]
fn capsh_missing_failing_or_answering() {
    use serde_json::json;

    use super::super::tests::{FakeRoot, probe};

    let fake = FakeRoot::new("caps");
    let (root, probe) = (fake.root(), probe());
    let facts = collect(&fake.host(&root, &probe)).unwrap();
    assert_eq!(facts["system_capabilities"], "N/A");
    assert_eq!(facts["system_capabilities_enforced"], "N/A");
    fake.command("/usr/sbin/capsh", "Current: =ep", 1);
    assert_eq!(
        collect(&fake.host(&root, &probe)).unwrap()["system_capabilities"],
        "N/A"
    );
    fake.command("/usr/sbin/capsh", "Current: =ep", 0);
    let facts = collect(&fake.host(&root, &probe)).unwrap();
    assert_eq!(facts["system_capabilities"], json!([]));
    assert_eq!(facts["system_capabilities_enforced"], "False");
}
