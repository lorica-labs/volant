// SPDX-License-Identifier: GPL-3.0-or-later

//! The reference's working state across devices, and the `ip addr` lines it reads.

use std::net::Ipv4Addr;

use serde_json::{Map, Value};

use crate::natives::runtime::{self, Root, py_int, py_split};

pub(super) fn insert(names: &mut Vec<String>, name: &str) {
    if !names.iter().any(|known| known == name) {
        names.push(name.to_string());
    }
}

/// What the reference keeps of one device for the default routes.
pub(super) struct Device {
    name: String,
    mtu: Option<i64>,
    pub(super) kind: Option<String>,
}

/// The reference's working state across devices.
pub(super) struct State {
    /// The keys of its `interfaces` dict, in insertion order.
    pub(super) names: Vec<String>,
    pub(super) default_ipv4: Map<String, Value>,
    pub(super) default_ipv6: Map<String, Value>,
    pub(super) all_ipv4: Vec<String>,
    pub(super) all_ipv6: Vec<String>,
    /// Its `macaddress` local: set by a device with an `address` file and kept by the next one
    /// without; unset, reading it raises.
    pub(super) macaddress: Option<String>,
}

impl State {
    /// The `/sys/class/net/<name>` reads, `None` where one raises.
    pub(super) fn device(&mut self, root: &Root, name: &str) -> Option<Device> {
        insert(&mut self.names, name);
        let mut dir = format!("/sys/class/net/{name}");
        let exists = |dir: &str, file: &str| root.exists(&format!("{dir}/{file}"));
        let content = |dir: &str, file: &str| root.content(&format!("{dir}/{file}"));
        if exists(&dir, "address") {
            self.macaddress = Some(content(&dir, "address").unwrap_or_default());
        }
        let mtu = if exists(&dir, "mtu") {
            Some(py_int(&content(&dir, "mtu")?, 10)?)
        } else {
            None
        };
        let mut kind = exists(&dir, "type").then(|| {
            match content(&dir, "type").as_deref() {
                Some("1") => "ether",
                Some("32") => "infiniband",
                Some("512") => "ppp",
                Some("772") => "loopback",
                Some("65534") => "tunnel",
                _ => "unknown",
            }
            .to_string()
        });
        if exists(&dir, "bridge") {
            kind = Some("bridge".into());
        }
        if exists(&dir, "bonding") {
            kind = Some("bonding".into());
            for field in ["mode", "miimon", "lacp_rate"] {
                py_split(&content(&dir, &format!("bonding/{field}")).unwrap_or_default()).next()?;
            }
            if content(&dir, "bonding/primary").is_some() {
                // The reference reuses its path variable here, and reads the rest under it.
                dir = format!("{dir}/bonding/all_slaves_active");
            }
        }
        if exists(&dir, "device") {
            std::fs::read_link(root.path(&format!("{dir}/device"))).ok()?;
        }
        if exists(&dir, "speed")
            && let Some(speed) = content(&dir, "speed")
        {
            py_int(&speed, 10)?;
        }
        if exists(&dir, "flags") {
            py_int(&content(&dir, "flags")?, 16)?;
        }
        Some(Device {
            name: name.to_string(),
            mtu,
            kind,
        })
    }

    /// `parse_ip_output`, `None` where it raises. A secondary address changes only the
    /// per-interface dicts, which the native leaves out, so both outputs parse alike here.
    pub(super) fn parse(&mut self, device: &Device, output: &str) -> Option<()> {
        for line in runtime::splitlines(output) {
            if line.is_empty() {
                continue;
            }
            let words: Vec<&str> = py_split(line).collect();
            match *words.first()? {
                "inet" => self.inet(device, &words)?,
                "inet6" => self.inet6(device, &words)?,
                _ => {}
            }
        }
        Some(())
    }

    fn inet(&mut self, device: &Device, words: &[&str]) -> Option<()> {
        let spec = *words.get(1)?;
        let mut broadcast = "";
        let (address, prefix) = if spec.contains('/') {
            if words.len() > 3 && words[2] == "brd" {
                broadcast = words[3];
            }
            two(spec)?
        } else {
            // A point-to-point address has no prefix.
            (spec, "32")
        };
        let bits = u32::from(address.parse::<Ipv4Addr>().ok()?);
        let length = u32::try_from(py_int(prefix, 10)?)
            .ok()
            .filter(|n| *n <= 32)?;
        let netmask = u32::try_from((1_u64 << 32) - ((1_u64 << 32) >> length)).ok()?;
        let iface = *words.last()?;
        if iface != device.name {
            insert(&mut self.names, iface);
        }
        if self.default_ipv4.get("address").and_then(Value::as_str) == Some(address) {
            let mut put = |key: &str, value: Value| {
                self.default_ipv4.insert(key.into(), value);
            };
            put("broadcast", broadcast.into());
            put("netmask", Ipv4Addr::from(netmask).to_string().into());
            put("network", Ipv4Addr::from(bits & netmask).to_string().into());
            put("prefix", prefix.into());
            put("macaddress", self.macaddress.clone()?.into());
            put("mtu", device.mtu?.into());
            put("type", device.kind.as_deref().unwrap_or("unknown").into());
            put("alias", iface.into());
        }
        if !address.starts_with("127.") {
            self.all_ipv4.push(address.to_string());
        }
        Some(())
    }

    fn inet6(&mut self, device: &Device, words: &[&str]) -> Option<()> {
        let (address, prefix, scope) = if *words.get(2)? == "peer" {
            (words[1], two(words.get(3)?)?.1, *words.get(5)?)
        } else {
            let (address, prefix) = two(words.get(1)?)?;
            (address, prefix, *words.get(3)?)
        };
        if self.default_ipv6.get("address").and_then(Value::as_str) == Some(address) {
            let mut put = |key: &str, value: Value| {
                self.default_ipv6.insert(key.into(), value);
            };
            put("prefix", prefix.into());
            put("scope", scope.into());
            put("macaddress", self.macaddress.clone()?.into());
            put("mtu", device.mtu?.into());
            put("type", device.kind.as_deref().unwrap_or("unknown").into());
        }
        if address != "::1" {
            self.all_ipv6.push(address.to_string());
        }
        Some(())
    }
}

/// `a, b = text.split('/')`, which raises unless there are exactly two parts.
pub(super) fn two(text: &str) -> Option<(&str, &str)> {
    let (a, b) = text.split_once('/')?;
    (!b.contains('/')).then_some((a, b))
}
