// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::json;

use super::super::tests::{FakeRoot, probe};
use super::*;

/// A systemd-resolved stub file, sanitised, plus the quirks: servers accumulate, a later
/// `search` replaces an earlier one, an option with an empty value is `true`.
#[test]
fn resolv_conf_is_read_like_the_reference() {
    let fake = FakeRoot::new("dns");
    let (root, probe) = (fake.root(), probe());
    assert_eq!(collect(&fake.host(&root, &probe))["dns"], json!({}));
    fake.write(
        "/etc/resolv.conf",
        "# comment\n; other\nnameserver 127.0.0.53\nnameserver 10.0.0.1 10.0.0.2\n\
             options edns0 trust-ad ndots:2 timeout:\nsearch first\nsearch example home\n\
             domain\ndomain example\n",
    );
    assert_eq!(
        collect(&fake.host(&root, &probe))["dns"],
        json!({
            "nameservers": ["127.0.0.53", "10.0.0.1", "10.0.0.2"],
            "options": {"edns0": true, "trust-ad": true, "ndots": "2", "timeout": true},
            "search": ["example", "home"],
            "domain": "example",
        })
    );
}
