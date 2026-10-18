// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::json;

use super::super::tests::{FakeRoot, probe};
use super::*;

fn lsb(fake: &FakeRoot) -> Result<Value, String> {
    let (root, probe) = (fake.root(), probe());
    let host = fake.host(&root, &probe);
    let run = LsbRelease::run(host.root, &host.env, crate::natives::runtime::unbounded()).unwrap();
    collect(&host, &run).map(|facts| facts["lsb"].clone())
}

/// `lsb_release -a` as Ubuntu 24.04 prints it, the file when the command is missing or fails,
/// quotes stripped after `major_release` is cut.
#[test]
fn lsb_comes_from_the_command_then_the_file() {
    let fake = FakeRoot::new("lsb");
    assert_eq!(lsb(&fake).unwrap(), json!({}), "neither");
    fake.write(
        "/etc/lsb-release",
        "DISTRIB_ID=Ubuntu\nDISTRIB_RELEASE=24.04\nDISTRIB_CODENAME=noble\nDISTRIB_DESCRIPTION=\"Ubuntu 24.04.4 LTS\"\n",
    );
    let from_file = json!({"id": "Ubuntu", "release": "24.04", "codename": "noble",
                           "description": "Ubuntu 24.04.4 LTS", "major_release": "24"});
    assert_eq!(lsb(&fake).unwrap(), from_file);
    fake.command("/usr/bin/lsb_release", "Distributor ID:\tUbuntu", 1);
    assert_eq!(
        lsb(&fake).unwrap(),
        from_file,
        "a failing command reads the file"
    );
    fake.command(
        "/usr/bin/lsb_release",
        "Distributor ID:\tUbuntu\nDescription:\tUbuntu 24.04.4 LTS\nRelease:\t24.04\nCodename:\tnoble",
        0,
    );
    assert_eq!(
        lsb(&fake).unwrap(),
        json!({"id": "Ubuntu", "description": "Ubuntu 24.04.4 LTS", "release": "24.04",
               "codename": "noble", "major_release": "24"})
    );
    fake.command("/usr/bin/lsb_release", "", 0)
        .write("/etc/lsb-release", "DISTRIB_ID=Ubuntu\nbroken\n");
    assert!(lsb(&fake).is_err());
}
