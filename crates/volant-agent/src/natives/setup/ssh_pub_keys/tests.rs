// SPDX-License-Identifier: GPL-3.0-or-later

use super::super::tests::{FakeRoot, probe};
use super::*;

/// Keys in `/etc/ssh` win, a key missing there is looked for in the next directory until the
/// loop meets an algorithm it already has, and a file of one word hands back.
#[test]
fn host_keys_follow_the_reference_s_loop() {
    let fake = FakeRoot::new("ssh");
    let (root, probe) = (fake.root(), probe());
    assert!(collect(&fake.host(&root, &probe)).unwrap().is_empty());
    fake.write(
        "/etc/ssh/ssh_host_rsa_key.pub",
        "ssh-rsa AAAArsa root@probe-hostname\n",
    )
    .write("/etc/openssh/ssh_host_dsa_key.pub", "ssh-dss AAAAdsa\n")
    .write(
        "/etc/openssh/ssh_host_ed25519_key.pub",
        "ssh-ed25519 AAAAopen\n",
    )
    .write(
        "/etc/ssh_host_ecdsa_key.pub",
        "ecdsa-sha2-nistp256 AAAAetc\n",
    );
    let facts = collect(&fake.host(&root, &probe)).unwrap();
    assert_eq!(facts["ssh_host_key_rsa_public"], "AAAArsa");
    assert_eq!(facts["ssh_host_key_rsa_public_keytype"], "ssh-rsa");
    assert_eq!(
        facts["ssh_host_key_dsa_public"], "AAAAdsa",
        "dsa comes before rsa in the next directory"
    );
    assert_eq!(
        facts.len(),
        4,
        "the loop stops at rsa in /etc/openssh: {facts:?}"
    );
    fake.write("/etc/ssh/ssh_host_ecdsa_key.pub", "broken\n");
    assert!(collect(&fake.host(&root, &probe)).is_err());
}
