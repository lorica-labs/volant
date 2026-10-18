// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::json;

use super::super::tests::{FakeRoot, probe};
use super::*;

#[test]
fn local_facts_are_empty_or_handed_back() {
    let fake = FakeRoot::new("local");
    let (root, probe) = (fake.root(), probe());
    let host = fake.host(&root, &probe);
    let dir = "/etc/ansible/facts.d";
    assert_eq!(
        collect(&host, Some(dir)).unwrap()["local"],
        json!({}),
        "no directory"
    );
    fake.write("/etc/ansible/facts.d/.hidden.fact", "{}")
        .write("/etc/ansible/facts.d/notes.txt", "");
    assert_eq!(collect(&host, Some(dir)).unwrap()["local"], json!({}));
    assert_eq!(collect(&host, None).unwrap()["local"], json!({}));
    fake.write("/etc/ansible/facts.d/site.fact", "{}");
    assert!(collect(&host, Some(dir)).is_err());
    assert!(collect(&host, None).is_ok(), "no fact path, nothing read");
}
