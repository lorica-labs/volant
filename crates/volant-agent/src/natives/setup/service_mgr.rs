// SPDX-License-Identifier: GPL-3.0-or-later
//! The `service_mgr` collector: the name of process 1 from `/proc/1/comm`, and for an `init` or a
//! shell the reference's Linux probes in their order. Without `/proc/1/comm` the reference asks
//! `ps`, and the native hands back.

use serde_json::{Map, Value};

use super::{Host, py_strip};

pub fn collect(host: &Host) -> Result<Map<String, Value>, String> {
    let comm = host
        .root
        .content("/proc/1/comm")
        .ok_or("/proc/1/comm is unreadable, and the reference asks ps")?;
    let proc_1 = py_strip(basename(&comm));
    let name = if proc_1 == "init" || proc_1.ends_with("sh") {
        linux(host)
    } else {
        match proc_1 {
            "procd" => "openwrt_init",
            "runit-init" => "runit",
            "svscan" => "svc",
            "openrc-init" => "openrc",
            other => other,
        }
    };
    let mut facts = Map::new();
    facts.insert("service_mgr".into(), Value::from(name));
    Ok(facts)
}

/// The probes for a process 1 that says nothing: an `init` or a shell in a container.
fn linux(host: &Host) -> &'static str {
    let root = host.root;
    let systemctl = host.bin_path("systemctl").is_some();
    if systemctl
        && [
            "/run/systemd/system/",
            "/dev/.run/systemd/",
            "/dev/.systemd/",
        ]
        .iter()
        .any(|canary| root.exists(canary))
    {
        "systemd"
    } else if host.bin_path("initctl").is_some() && root.exists("/etc/init/") {
        "upstart"
    } else if root.exists("/sbin/openrc") {
        "openrc"
    } else if systemctl
        && std::fs::read_link(root.path("/sbin/init"))
            .is_ok_and(|target| target.file_name().is_some_and(|name| name == "systemd"))
    {
        "systemd"
    } else if root.exists("/etc/init.d/") {
        "sysvinit"
    } else if root.exists("/etc/dinit.d/") {
        "dinit"
    } else {
        "service"
    }
}

/// `os.path.basename`.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{FakeRoot, probe};
    use super::*;

    fn service_mgr(fake: &FakeRoot) -> Result<Value, String> {
        let (root, probe) = (fake.root(), probe());
        collect(&fake.host(&root, &probe)).map(|facts| facts["service_mgr"].clone())
    }

    /// `systemd` on a booted host, and `python3` in the container of the reference measurement,
    /// whose process 1 is the interpreter: both read off `/proc/1/comm` alone.
    ///
    /// What would make this red: `service_mgr` read anywhere but `/proc/1/comm` (the systemd
    /// canary below is present in the container case, and would answer `systemd`).
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
}
