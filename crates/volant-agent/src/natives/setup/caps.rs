// SPDX-License-Identifier: GPL-3.0-or-later
//! The `caps` collector: `capsh --print`, its `Current:` line read the way the reference reads it,
//! and `N/A` for both facts when `capsh` is missing or fails.

use serde_json::{Map, Value};

use super::{Host, splitlines};

pub fn collect(host: &Host) -> Result<Map<String, Value>, String> {
    let mut enforced = Value::from("N/A");
    let mut capabilities = Value::from("N/A");
    if let Some(capsh) = host.bin_path("capsh") {
        // A capsh that cannot be started is a warning in the module's result.
        let (code, out) = host
            .run(&capsh, &["--print"])
            .ok_or("capsh was found and could not be started")?;
        if code == 0 {
            let (value, caps) = parse(&out);
            enforced = Value::from(value);
            capabilities = Value::from(caps);
        }
    }
    let mut facts = Map::new();
    facts.insert("system_capabilities_enforced".into(), enforced);
    facts.insert("system_capabilities".into(), capabilities);
    Ok(facts)
}

/// `line.split(':')[1].strip() == '=ep'`, else the names between the first and second `=`, split
/// on commas.
fn parse(out: &str) -> (&'static str, Vec<String>) {
    let mut enforced = "NA";
    let mut caps = Vec::new();
    for line in splitlines(out) {
        if !line.starts_with("Current:") {
            continue;
        }
        if super::py_strip(line.split(':').nth(1).unwrap_or_default()) == "=ep" {
            enforced = "False";
        } else {
            enforced = "True";
            caps = line
                .split('=')
                .nth(1)
                .unwrap_or_default()
                .split(',')
                .map(|cap| super::py_strip(cap).to_string())
                .collect();
        }
    }
    (enforced, caps)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::tests::{FakeRoot, probe};
    use super::*;

    /// Root with every capability, an unprivileged user (measured: `Current: =` gives `['']`),
    /// a capability list, and `capsh` missing or failing.
    ///
    /// What would make this red: the empty list read as `[]` for an unprivileged user, or the
    /// facts left out when `capsh` is missing, where ansible-core 2.19 says `N/A`.
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
}
