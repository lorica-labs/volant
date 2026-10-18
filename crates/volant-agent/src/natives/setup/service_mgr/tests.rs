// SPDX-License-Identifier: GPL-3.0-or-later

use super::super::tests::{FakeRoot, probe};
use super::*;

fn service_mgr(fake: &FakeRoot) -> Result<Value, String> {
    let (root, probe) = (fake.root(), probe());
    collect(&fake.host(&root, &probe)).map(|facts| facts["service_mgr"].clone())
}

/// `systemd` on a booted host, and `python3` in the container of the reference measurement,
/// whose process 1 is the interpreter: both read off `/proc/1/comm` alone.
#[test]
fn process_one_names_the_service_manager() {
    let fake = FakeRoot::new("service-mgr");
    fake.mkdir("/run/systemd/system")
        .command("/usr/bin/systemctl", "", 0);
    assert!(service_mgr(&fake).is_err(), "no /proc/1/comm");
    fake.write("/proc/1/comm", "systemd\n");
    assert_eq!(service_mgr(&fake).unwrap(), "systemd");
    fake.write("/proc/1/comm", "python3\n");
    assert_eq!(service_mgr(&fake).unwrap(), "python3");
    fake.write("/proc/1/comm", "runit-init\n");
    assert_eq!(service_mgr(&fake).unwrap(), "runit");
}

/// An `init` or a shell as process 1 falls to the probes, in the reference's order.
#[test]
fn an_anonymous_process_one_falls_to_the_probes() {
    let fake = FakeRoot::new("service-mgr-probes");
    fake.write("/proc/1/comm", "bash\n");
    assert_eq!(service_mgr(&fake).unwrap(), "service");
    fake.mkdir("/etc/dinit.d");
    assert_eq!(service_mgr(&fake).unwrap(), "dinit");
    fake.mkdir("/etc/init.d");
    assert_eq!(service_mgr(&fake).unwrap(), "sysvinit");
    fake.command("/usr/bin/systemctl", "", 0).mkdir("/sbin");
    std::os::unix::fs::symlink("../lib/systemd/systemd", fake.0.join("sbin/init")).unwrap();
    assert_eq!(
        service_mgr(&fake).unwrap(),
        "systemd",
        "offline: /sbin/init links to systemd"
    );
    fake.write("/sbin/openrc", "");
    assert_eq!(service_mgr(&fake).unwrap(), "openrc");
    fake.write("/proc/1/comm", "init\n")
        .mkdir("/run/systemd/system");
    assert_eq!(service_mgr(&fake).unwrap(), "systemd");
}
