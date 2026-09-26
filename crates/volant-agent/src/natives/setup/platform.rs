// SPDX-License-Identifier: GPL-3.0-or-later
//! The `platform` collector: `uname(2)`, the interpreter's own answers, `socket.getfqdn()` and the
//! machine id.
//!
//! `getfqdn` resolves the node name and then its address back through the name service. The
//! native answers it from `/etc/hosts` when the name service reads that file first and the file
//! gives one address for the name; anything else (a name only DNS knows, several addresses the
//! C library would order) hands back rather than guessing.

use std::ffi::CStr;
use std::net::IpAddr;

use serde_json::{Map, Value};

use super::{Host, py_split, py_strip};

/// The fields of `uname(2)`.
pub struct Uname {
    pub system: String,
    pub node: String,
    pub release: String,
    pub version: String,
    pub machine: String,
}

pub fn uname() -> Uname {
    let mut raw: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe { libc::uname(&raw mut raw) };
    let field = |chars: &[libc::c_char]| {
        unsafe { CStr::from_ptr(chars.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    };
    Uname {
        system: field(&raw.sysname),
        node: field(&raw.nodename),
        release: field(&raw.release),
        version: field(&raw.version),
        machine: field(&raw.machine),
    }
}

pub fn collect(host: &Host) -> Result<Map<String, Value>, String> {
    collect_with(host, &uname())
}

pub fn collect_with(host: &Host, uname: &Uname) -> Result<Map<String, Value>, String> {
    let fqdn = fqdn(host, &uname.node)?;
    let bits = host.probe.bits.replace("bit", "");
    let mut facts = Map::new();
    let mut put = |key: &str, value: &str| {
        facts.insert(key.to_string(), Value::from(value));
    };
    put("system", &uname.system);
    put("kernel", &uname.release);
    put("kernel_version", &uname.version);
    put("machine", &uname.machine);
    put("python_version", &host.probe.python_version);
    put("fqdn", &fqdn);
    put("hostname", uname.node.split('.').next().unwrap_or_default());
    put("nodename", &uname.node);
    put(
        "domain",
        fqdn.split_once('.').map_or("", |(_, domain)| domain),
    );
    put("userspace_bits", &bits);
    let userspace = match bits.as_str() {
        "64" => Some("x86_64"),
        "32" => Some("i386"),
        _ => None,
    };
    if uname.machine == "x86_64" {
        put("architecture", "x86_64");
    } else if ["i386", "i486", "i586", "i686", "i86pc"]
        .iter()
        .any(|i86| uname.machine.contains(i86))
    {
        put("architecture", "i386");
    } else {
        put("architecture", &uname.machine);
    }
    if let Some(userspace) = userspace
        && (uname.machine == "x86_64" || facts["architecture"] == "i386")
    {
        facts.insert("userspace_architecture".into(), Value::from(userspace));
    }
    let machine_id = host
        .root
        .content("/var/lib/dbus/machine-id")
        .or_else(|| host.root.content("/etc/machine-id"));
    if let Some(machine_id) = machine_id {
        let first = super::splitlines(&machine_id)[0].to_string();
        facts.insert("machine_id".into(), Value::from(first));
    }
    Ok(facts)
}

/// `socket.getfqdn()` for `node`, from `/etc/hosts`: the one address the file gives the name,
/// then the first line holding that address, whose first name with a dot wins over its
/// canonical name.
fn fqdn(host: &Host, node: &str) -> Result<String, String> {
    let name = py_strip(node);
    if name.is_empty() || name.parse::<IpAddr>().is_ok() {
        return Err(format!("the node name '{name}' is not a host name"));
    }
    let first = host
        .root
        .content("/etc/nsswitch.conf")
        .and_then(|conf| {
            conf.lines()
                .find_map(|line| line.trim_start().strip_prefix("hosts:").map(str::to_string))
        })
        .and_then(|sources| py_split(&sources).next().map(str::to_string));
    if first.as_deref() != Some("files") {
        return Err("host names are not resolved from /etc/hosts first".into());
    }
    let hosts = std::fs::read_to_string(host.root.path("/etc/hosts")).unwrap_or_default();
    let mut lines: Vec<(IpAddr, Vec<&str>)> = Vec::new();
    for line in hosts.lines() {
        let line = line.split('#').next().unwrap_or_default();
        let mut words = line.split_ascii_whitespace();
        let Some(address) = words.next() else {
            continue;
        };
        let address = address.parse::<IpAddr>().map_err(|_| {
            format!("/etc/hosts has an address the native does not read: {address}")
        })?;
        lines.push((address, words.collect()));
    }
    let mut addresses = lines
        .iter()
        .filter(|(_, names)| names.iter().any(|n| n.eq_ignore_ascii_case(name)))
        .map(|(address, _)| *address);
    let address = addresses
        .next()
        .ok_or_else(|| format!("{name} is not in /etc/hosts"))?;
    if addresses.any(|other| other != address) {
        return Err(format!("{name} has several addresses in /etc/hosts"));
    }
    let names = lines
        .iter()
        .find(|(line_address, _)| *line_address == address)
        .map(|(_, names)| names)
        .filter(|names| !names.is_empty())
        .ok_or("no name for the address")?;
    Ok(names
        .iter()
        .find(|name| name.contains('.'))
        .unwrap_or(&names[0])
        .to_string())
}

#[cfg(test)]
mod tests {
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

    /// Debian's layout (`127.0.1.1 <fqdn> <short>`), Ubuntu's (`127.0.1.1 <short>`), and the
    /// cases the C library would decide: resolution not reading the file first, the name absent
    /// from it, or given two addresses.
    ///
    /// What would make this red: the node name returned as is (Debian's layout gives the long
    /// name), the canonical name kept when an alias has a dot, or an answer where the name
    /// service would ask DNS.
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
}
