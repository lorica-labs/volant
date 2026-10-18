// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::json;

use super::super::tests::{FakeRoot, probe};
use super::*;

/// The name comes from `LOGNAME` before the uid, as `getpass.getuser()` has it, and its entry
/// from `/etc/passwd`.
#[test]
fn the_user_is_the_environment_s_then_the_uid_s() {
    let fake = FakeRoot::new("user");
    fake.write(
        "/etc/nsswitch.conf",
        "# comment\npasswd:         files systemd\n",
    )
    .write(
        "/etc/passwd",
        "root:x:0:0:root:/root:/bin/bash\nuser:x:1000:1001:A User,,,:/home/user:/bin/sh\n",
    );
    let root = fake.root();
    let mut probe = probe();
    let facts = collect_as(&fake.host(&root, &probe), 0, 0, 5).unwrap();
    assert_eq!(
        Value::Object(facts),
        json!({"user_id": "user", "user_uid": 1000, "user_gid": 1001, "user_gecos": "A User,,,",
               "user_dir": "/home/user", "user_shell": "/bin/sh", "real_user_id": 0,
               "effective_user_id": 0, "real_group_id": 5, "effective_group_id": 5})
    );
    probe.env.remove("LOGNAME");
    let facts = collect_as(&fake.host(&root, &probe), 0, 0, 0).unwrap();
    assert_eq!(facts["user_id"], "root", "no variable, the uid's name");
    probe.env.insert("USER".into(), "nobody-here".into());
    assert!(
        collect_as(&fake.host(&root, &probe), 0, 0, 0).is_err(),
        "a user /etc/passwd lacks"
    );
    fake.write("/etc/nsswitch.conf", "passwd: sss files\n");
    probe.env.insert("USER".into(), "root".into());
    assert!(
        collect_as(&fake.host(&root, &probe), 0, 0, 0).is_err(),
        "sssd first"
    );
}
