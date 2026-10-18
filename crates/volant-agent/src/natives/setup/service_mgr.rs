// SPDX-License-Identifier: GPL-3.0-or-later
//! The `service_mgr` collector: the name of process 1 from `/proc/1/comm`, and for an `init` or a
//! shell the reference's Linux probes in their order. Without `/proc/1/comm` the reference asks
//! `ps`, and the native hands back.

use serde_json::{Map, Value};

use super::Host;
use crate::natives::runtime::py_strip;

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
mod tests;
