// SPDX-License-Identifier: GPL-3.0-or-later

use super::super::tests::{FakeRoot, probe};
use super::*;

fn uname(node: &str, machine: &str) -> Uname {
    Uname {
        system: "Linux".into(),
        node: node.into(),
        release: "6.8.0-138-generic".into(),
        version: "#138-Ubuntu SMP PREEMPT_DYNAMIC".into(),
        machine: machine.into(),
    }
}

fn fqdn_of(fake: &FakeRoot, node: &str) -> Result<String, String> {
    let (root, probe) = (fake.root(), probe());
    fqdn(&fake.host(&root, &probe), node)
}

/// The fqdn comes from `/etc/hosts` in Debian's and Ubuntu's layouts, or the task is handed back.
#[test]
fn the_fqdn_comes_from_etc_hosts_or_hands_back() {
    let fake = FakeRoot::new("fqdn");
    fake.write("/etc/nsswitch.conf", "hosts: files dns\n").write(
        "/etc/hosts",
        "127.0.0.1 localhost\n127.0.1.1 probe-hostname.example.com probe-hostname # here\n::1 ip6-localhost\n",
    );
    assert_eq!(
        fqdn_of(&fake, "probe-hostname").unwrap(),
        "probe-hostname.example.com"
    );
    assert_eq!(
        fqdn_of(&fake, "PROBE-HOSTNAME").unwrap(),
        "probe-hostname.example.com"
    );
    fake.write(
        "/etc/hosts",
        "127.0.0.1 localhost\n127.0.1.1 probe-hostname\n",
    );
    assert_eq!(fqdn_of(&fake, "probe-hostname").unwrap(), "probe-hostname");
    fake.write("/etc/hosts", "127.0.0.1 localhost probe-hostname\n");
    assert_eq!(
        fqdn_of(&fake, "probe-hostname").unwrap(),
        "localhost",
        "the address's first name"
    );
    fake.write(
        "/etc/hosts",
        "127.0.1.1 probe-hostname\n::1 probe-hostname\n",
    );
    assert!(fqdn_of(&fake, "probe-hostname").is_err(), "two addresses");
    fake.write("/etc/hosts", "127.0.0.1 localhost\n");
    assert!(
        fqdn_of(&fake, "probe-hostname").is_err(),
        "only DNS knows it"
    );
    fake.write("/etc/hosts", "127.0.1.1 probe-hostname\n")
        .write("/etc/nsswitch.conf", "hosts: resolve files\n");
    assert!(fqdn_of(&fake, "probe-hostname").is_err(), "files not first");
}

/// The architecture facts on x86_64 and on arm64, where the reference has no
/// `userspace_architecture`, and the machine id's first line.
#[test]
fn the_architecture_follows_the_machine() {
    let fake = FakeRoot::new("platform");
    fake.write("/etc/nsswitch.conf", "hosts: files dns\n")
        .write(
            "/etc/hosts",
            "127.0.1.1 probe-hostname.example.com probe-hostname\n",
        )
        .write("/etc/machine-id", "0123abcd\n");
    let (root, probe) = (fake.root(), probe());
    let host = fake.host(&root, &probe);
    let facts = collect_with(&host, &uname("probe-hostname", "x86_64")).unwrap();
    assert_eq!(facts["architecture"], "x86_64");
    assert_eq!(facts["userspace_architecture"], "x86_64");
    assert_eq!(facts["userspace_bits"], "64");
    assert_eq!(facts["hostname"], "probe-hostname");
    assert_eq!(facts["domain"], "example.com");
    assert_eq!(facts["machine_id"], "0123abcd");
    assert_eq!(facts["python_version"], "3.12.3");
    let facts = collect_with(&host, &uname("probe-hostname", "aarch64")).unwrap();
    assert_eq!(facts["architecture"], "aarch64");
    assert!(!facts.contains_key("userspace_architecture"));
}
