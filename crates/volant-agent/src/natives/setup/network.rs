// SPDX-License-Identifier: GPL-3.0-or-later
//! The `network` collector, restricted to `interfaces`, `default_ipv4`, `default_ipv6`,
//! `all_ipv4_addresses` and `all_ipv6_addresses`, from the same commands `LinuxNetwork` runs:
//! `ip -4 route get`, `ip -6 route get`, `ip addr show primary|secondary dev <name>` for every
//! directory of `/sys/class/net`, parsed line for line as it parses them. The per-interface dicts,
//! `locally_reachable_ips` and the `ethtool` features are left out: those keys are absent.
//!
//! Without an `ip` command the reference writes no network fact, and neither does the native.
//! When one of the reference's reads raises, its fact collector drops every network fact and the
//! module goes on; the native does the same.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use serde_json::{Map, Value};

use super::{Clock, Root, Stop, bin_path, py_int, py_split, run, splitlines, wait_polling_cancel};

/// The addresses the reference asks the kernel to route.
const PROBE_V4: &str = "8.8.8.8";
const PROBE_V6: &str = "2404:6800:400a:800::1012";

/// The facts, none where the reference writes none, or why to stop: `ip` found and not
/// startable fails the reference's module (hand back), and the task's deadline and cancel bound
/// every command.
pub fn collect(
    root: &Root,
    env: &BTreeMap<String, String>,
    clock: Clock,
) -> Result<Map<String, Value>, Stop> {
    let Some(ip) = bin_path(root, env, "ip") else {
        return Ok(Map::new());
    };
    let names = device_names(root);
    let mut commands: Vec<Vec<&str>> = vec![
        vec!["-4", "route", "get", PROBE_V4],
        vec!["-6", "route", "get", PROBE_V6],
    ];
    for name in &names {
        commands.push(vec!["addr", "show", "primary", "dev", name.as_str()]);
        commands.push(vec!["addr", "show", "secondary", "dev", name.as_str()]);
    }
    let mut ran = run_together(env, clock, &ip, &commands)?.into_iter();
    let started = || Stop::from("ip was found and could not be started");
    let mut next = || ran.next().flatten().ok_or_else(started);
    let mut state = State {
        names: Vec::new(),
        default_ipv4: default_route(&next()?.1, PROBE_V4),
        default_ipv6: default_route(&next()?.1, PROBE_V6),
        all_ipv4: Vec::new(),
        all_ipv6: Vec::new(),
        macaddress: None,
    };
    for name in &names {
        let Some(device) = state.device(root, name) else {
            return Ok(Map::new());
        };
        let (code, out) = next()?;
        let parsed = if code == 0 {
            state.parse(&device, &out)
        } else {
            // busybox `ip` knows no `primary`.
            match run(env, clock, &ip, &["addr", "show", "dev", name])?.ok_or_else(started)? {
                (0, out) => state.parse(&device, &out),
                _ => Some(()),
            }
        };
        let (code, out) = next()?;
        if parsed.is_none() || (code == 0 && state.parse(&device, &out).is_none()) {
            return Ok(Map::new());
        }
    }
    let mut interfaces: Vec<String> = Vec::new();
    for name in &state.names {
        insert(&mut interfaces, &name.replace(':', "_"));
    }
    let mut facts = Map::new();
    facts.insert("interfaces".into(), interfaces.into());
    facts.insert("default_ipv4".into(), Value::Object(state.default_ipv4));
    facts.insert("default_ipv6".into(), Value::Object(state.default_ipv6));
    facts.insert("all_ipv4_addresses".into(), state.all_ipv4.into());
    facts.insert("all_ipv6_addresses".into(), state.all_ipv6.into());
    Ok(facts)
}

/// Every command at once, each on its own thread: they only read the kernel's tables. Each is
/// bounded by the task's deadline; the task's cancel, which only this thread may ask, is polled
/// while they run (`wait_polling_cancel`) and stops them all. Each exit code and output, `None` for one that could not
/// be started.
fn run_together(
    env: &BTreeMap<String, String>,
    clock: Clock,
    program: &Path,
    commands: &[Vec<&str>],
) -> Result<Vec<Option<(i32, String)>>, Stop> {
    let halt = AtomicBool::new(false);
    let halted = || halt.load(Ordering::Relaxed);
    let deadline = clock.deadline;
    let (sender, receiver) = mpsc::channel();
    let mut results: Vec<Option<(i32, String)>> = vec![None; commands.len()];
    let mut stop = None;
    std::thread::scope(|scope| {
        for (at, args) in commands.iter().enumerate() {
            let sender = sender.clone();
            scope.spawn(move || {
                let clock = Clock {
                    deadline,
                    cancelled: &halted,
                };
                let _ = sender.send((at, run(env, clock, program, args)));
            });
        }
        drop(sender);
        loop {
            match wait_polling_cancel(&receiver, clock, &halt) {
                Ok(Some((at, Ok(ran)))) => results[at] = ran,
                Ok(Some((_, Err(error)))) => {
                    stop.get_or_insert(error);
                    halt.store(true, Ordering::Relaxed);
                }
                Ok(None) => break,
                Err(cancelled) => {
                    // The halted commands end on their own; the scope waits for them.
                    stop.get_or_insert(cancelled);
                    break;
                }
            }
        }
    });
    match stop {
        Some(stop) => Err(stop),
        None => Ok(results),
    }
}

/// `glob('/sys/class/net/*')` keeping directories: directory order, no hidden name.
fn device_names(root: &Root) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root.path("/sys/class/net")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .filter(|name| root.path(&format!("/sys/class/net/{name}")).is_dir())
        .collect()
}

/// `get_default_interfaces` for one family: the words after `dev`, `src` and `via` on the first
/// line, when that line starts with the address asked about.
fn default_route(out: &str, probe: &str) -> Map<String, Value> {
    let mut found = Map::new();
    let words: Vec<&str> = splitlines(out)
        .first()
        .map(|line| py_split(line).collect())
        .unwrap_or_default();
    if words.first() != Some(&probe) {
        return found;
    }
    for pair in words.windows(2) {
        let key = match pair[0] {
            "dev" => "interface",
            "src" => "address",
            "via" if pair[1] != probe => "gateway",
            _ => continue,
        };
        found.insert(key.into(), pair[1].into());
    }
    found
}

fn insert(names: &mut Vec<String>, name: &str) {
    if !names.iter().any(|known| known == name) {
        names.push(name.to_string());
    }
}

/// What the reference keeps of one device for the default routes.
struct Device {
    name: String,
    mtu: Option<i64>,
    kind: Option<String>,
}

/// The reference's working state across devices.
struct State {
    /// The keys of its `interfaces` dict, in insertion order.
    names: Vec<String>,
    default_ipv4: Map<String, Value>,
    default_ipv6: Map<String, Value>,
    all_ipv4: Vec<String>,
    all_ipv6: Vec<String>,
    /// Its `macaddress` local: set by a device with an `address` file and kept by the next one
    /// without; unset, reading it raises.
    macaddress: Option<String>,
}

impl State {
    /// The `/sys/class/net/<name>` reads, `None` where one raises.
    fn device(&mut self, root: &Root, name: &str) -> Option<Device> {
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
    fn parse(&mut self, device: &Device, output: &str) -> Option<()> {
        for line in splitlines(output) {
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
fn two(text: &str) -> Option<(&str, &str)> {
    let (a, b) = text.split_once('/')?;
    (!b.contains('/')).then_some((a, b))
}

#[cfg(test)]
pub mod tests {
    use std::os::unix::fs::PermissionsExt;

    use serde_json::json;

    use super::super::tests::FakeRoot;
    use super::super::unbounded;
    use super::*;

    /// An `ip` at `/usr/sbin/ip` answering the reference's commands with what a guest with one
    /// ethernet interface, a secondary address and IPv6 printed (addresses from the
    /// documentation ranges).
    pub const IP: &str = r#"#!/bin/sh
case "$*" in
"-4 route get 8.8.8.8")
  echo "8.8.8.8 via 192.0.2.1 dev eth0 src 192.0.2.10 uid 1000"
  echo "    cache" ;;
"-6 route get 2404:6800:400a:800::1012")
  echo "2404:6800:400a:800::1012 from :: via fe80::1 dev eth0 proto ra src 2001:db8::10 metric 100 pref medium" ;;
"addr show primary dev lo")
  cat <<'EOF'
1: lo: <LOOPBACK,UP,LOWER_UP> mtu 65536 qdisc noqueue state UNKNOWN group default qlen 1000
    link/loopback 00:00:00:00:00:00 brd 00:00:00:00:00:00
    inet 127.0.0.1/8 scope host lo
       valid_lft forever preferred_lft forever
    inet6 ::1/128 scope host noprefixroute
       valid_lft forever preferred_lft forever
EOF
  ;;
"addr show primary dev eth0")
  cat <<'EOF'
2: eth0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc fq_codel state UP group default qlen 1000
    link/ether 52:54:00:12:34:56 brd ff:ff:ff:ff:ff:ff
    altname enp6s18
    inet 192.0.2.10/24 metric 100 brd 192.0.2.255 scope global dynamic eth0
       valid_lft 86000sec preferred_lft 86000sec
    inet6 2001:db8::10/64 scope global dynamic mngtmpaddr noprefixroute
       valid_lft 86000sec preferred_lft 14000sec
    inet6 fe80::5054:ff:fe12:3456/64 scope link
       valid_lft forever preferred_lft forever
EOF
  ;;
"addr show secondary dev eth0")
  cat <<'EOF'
2: eth0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc fq_codel state UP group default qlen 1000
    inet 192.0.2.11/24 brd 192.0.2.255 scope global secondary eth0:1
       valid_lft forever preferred_lft forever
EOF
  ;;
"addr show secondary dev lo") ;;
*) exit 1 ;;
esac
"#;

    /// `/sys/class/net` for that guest: `lo` and `eth0`.
    pub fn sysfs(fake: &FakeRoot) {
        for (name, address, mtu, kind) in [
            ("lo", "00:00:00:00:00:00", "65536", "772"),
            ("eth0", "52:54:00:12:34:56", "1500", "1"),
        ] {
            let dir = format!("/sys/class/net/{name}");
            fake.write(&format!("{dir}/address"), &format!("{address}\n"))
                .write(&format!("{dir}/mtu"), &format!("{mtu}\n"))
                .write(&format!("{dir}/type"), &format!("{kind}\n"))
                .write(&format!("{dir}/flags"), "0x1003\n")
                .write(&format!("{dir}/operstate"), "up\n");
        }
    }

    pub fn install_ip(fake: &FakeRoot, script: &str) {
        fake.write("/usr/sbin/ip", script);
        std::fs::set_permissions(
            fake.0.join("usr/sbin/ip"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }

    fn env() -> BTreeMap<String, String> {
        BTreeMap::from([("PATH".to_string(), "/usr/bin".to_string())])
    }

    /// The guest above, field by field as the reference builds them.
    ///
    /// What would make this red: the gateway, interface or source read from other words of the
    /// route; the broadcast, netmask, network, prefix, MAC, MTU or type of the default address
    /// taken from anywhere but its device; the alias not the label; a loopback address listed;
    /// the secondary's label missing from `interfaces` or its colon kept.
    #[test]
    fn the_default_routes_and_addresses_are_the_reference_s() {
        let fake = FakeRoot::new("network");
        sysfs(&fake);
        install_ip(&fake, IP);
        let facts = collect(&fake.root(), &env(), unbounded()).unwrap();
        let mut names = facts["interfaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|name| name.as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, ["eth0", "eth0_1", "lo"]);
        assert_eq!(
            facts["default_ipv4"],
            json!({
                "gateway": "192.0.2.1",
                "interface": "eth0",
                "address": "192.0.2.10",
                "broadcast": "",
                "netmask": "255.255.255.0",
                "network": "192.0.2.0",
                "prefix": "24",
                "macaddress": "52:54:00:12:34:56",
                "mtu": 1500,
                "type": "ether",
                "alias": "eth0",
            })
        );
        assert_eq!(
            facts["default_ipv6"],
            json!({
                "gateway": "fe80::1",
                "interface": "eth0",
                "address": "2001:db8::10",
                "prefix": "64",
                "scope": "global",
                "macaddress": "52:54:00:12:34:56",
                "mtu": 1500,
                "type": "ether",
            })
        );
        assert_eq!(
            facts["all_ipv4_addresses"],
            json!(["192.0.2.10", "192.0.2.11"])
        );
        assert_eq!(
            facts["all_ipv6_addresses"],
            json!(["2001:db8::10", "fe80::5054:ff:fe12:3456"])
        );
    }

    /// The broadcast is read only when `brd` directly follows the address, as the reference
    /// reads it; `ip` prints `metric` first on a DHCP address, so the reference's broadcast is
    /// empty there, and here too. Without `metric` it is read.
    #[test]
    fn the_broadcast_is_read_where_the_reference_reads_it() {
        let fake = FakeRoot::new("network-brd");
        sysfs(&fake);
        install_ip(&fake, &IP.replace(" metric 100 brd", " brd"));
        let facts = collect(&fake.root(), &env(), unbounded()).unwrap();
        assert_eq!(facts["default_ipv4"]["broadcast"], "192.0.2.255");
    }

    /// No `ip` along `PATH` or in the `sbin` directories: no network key at all, as the
    /// reference leaves them, even though `/sys/class/net` is there to read.
    ///
    /// What would make this red: the native falling back on `/proc/net/route` or `/sys` to
    /// answer what the reference does not.
    #[test]
    fn without_ip_there_is_no_network_fact() {
        let fake = FakeRoot::new("network-no-ip");
        sysfs(&fake);
        fake.mkdir("/sbin").mkdir("/usr/sbin");
        assert_eq!(
            collect(&fake.root(), &env(), unbounded()).unwrap(),
            Map::new()
        );
    }

    /// No route: `ip` prints nothing on stdout and the reference keeps an empty dict. A route
    /// without a source address leaves the default address unenriched.
    #[test]
    fn a_missing_route_is_an_empty_dict() {
        let fake = FakeRoot::new("network-no-route");
        sysfs(&fake);
        let script = IP
            .replace(
                r#"echo "2404:6800:400a:800::1012 from :: via fe80::1 dev eth0 proto ra src 2001:db8::10 metric 100 pref medium""#,
                "echo 'RTNETLINK answers: Network is unreachable' >&2; exit 2",
            )
            .replace(" src 192.0.2.10 uid 1000", " uid 1000");
        install_ip(&fake, &script);
        let facts = collect(&fake.root(), &env(), unbounded()).unwrap();
        assert_eq!(facts["default_ipv6"], json!({}));
        assert_eq!(
            facts["default_ipv4"],
            json!({"gateway": "192.0.2.1", "interface": "eth0"})
        );
    }

    /// What the reference's reads raise on, the whole collector dropped: an MTU that is not a
    /// number, a default address on a device before any MAC address was read. A command found
    /// and not startable fails the module, so the native hands back.
    #[test]
    fn a_read_the_reference_raises_on_drops_the_collector() {
        let fake = FakeRoot::new("network-raise");
        sysfs(&fake);
        install_ip(&fake, IP);
        // On `lo`, which carries no default address: the read alone raises.
        fake.write("/sys/class/net/lo/mtu", "jumbo
");
        assert_eq!(
            collect(&fake.root(), &env(), unbounded()).unwrap(),
            Map::new()
        );

        let fake = FakeRoot::new("network-no-mac");
        sysfs(&fake);
        install_ip(&fake, IP);
        std::fs::remove_file(fake.0.join("sys/class/net/eth0/address")).unwrap();
        std::fs::remove_file(fake.0.join("sys/class/net/lo/address")).unwrap();
        assert_eq!(
            collect(&fake.root(), &env(), unbounded()).unwrap(),
            Map::new()
        );

        let fake = FakeRoot::new("network-no-exec");
        sysfs(&fake);
        install_ip(&fake, "#!/nonexistent/sh\n");
        assert!(collect(&fake.root(), &env(), unbounded()).is_err());
    }

    /// A hung `ip` ends at the task's deadline, or at its cancel, which only the calling thread
    /// asks while the commands run on theirs.
    ///
    /// What would make this red: the commands started outside the executor, or the cancel never
    /// polled while they run, which leaves the task waiting thirty seconds.
    #[test]
    fn a_hung_ip_ends_at_the_timeout_or_the_cancel() {
        let fake = FakeRoot::new("network-hung");
        sysfs(&fake);
        install_ip(&fake, "#!/bin/sh\nsleep 30\n");
        let started = std::time::Instant::now();
        let clock = Clock {
            deadline: Some(started + std::time::Duration::from_secs(1)),
            cancelled: &|| false,
        };
        let stop = collect(&fake.root(), &env(), clock).unwrap_err();
        assert!(matches!(stop, Stop::TimedOut), "{stop:?}");
        assert!(started.elapsed().as_secs() < 10, "{:?}", started.elapsed());

        let asked = std::cell::Cell::new(0);
        let cancelled = || {
            asked.set(asked.get() + 1);
            asked.get() > 5
        };
        let started = std::time::Instant::now();
        let clock = Clock {
            deadline: None,
            cancelled: &cancelled,
        };
        let stop = collect(&fake.root(), &env(), clock).unwrap_err();
        assert!(matches!(stop, Stop::Cancelled), "{stop:?}");
        assert!(started.elapsed().as_secs() < 10, "{:?}", started.elapsed());
    }

    /// A busybox `ip` refuses `primary`: the reference asks again without it.
    #[test]
    fn busybox_ip_is_asked_without_primary() {
        let fake = FakeRoot::new("network-busybox");
        sysfs(&fake);
        let script = IP
            .replace("\"addr show primary dev eth0\")", "\"addr show dev eth0\")")
            .replace("\"addr show primary dev lo\")", "\"addr show dev lo\")");
        install_ip(&fake, &script);
        let facts = collect(&fake.root(), &env(), unbounded()).unwrap();
        assert_eq!(facts["default_ipv4"]["mtu"], 1500);
    }

    #[test]
    fn device_types_follow_the_reference_s_table() {
        let fake = FakeRoot::new("network-types");
        fake.write("/sys/class/net/br0/type", "1\n")
            .mkdir("/sys/class/net/br0/bridge")
            .write("/sys/class/net/wg0/type", "65534\n")
            .write("/sys/class/net/x0/type", "7\n")
            .mkdir("/sys/class/net/none");
        let mut state = State {
            names: Vec::new(),
            default_ipv4: Map::new(),
            default_ipv6: Map::new(),
            all_ipv4: Vec::new(),
            all_ipv6: Vec::new(),
            macaddress: None,
        };
        let kind = |state: &mut State, name| state.device(&fake.root(), name).unwrap().kind;
        assert_eq!(kind(&mut state, "br0").as_deref(), Some("bridge"));
        assert_eq!(kind(&mut state, "wg0").as_deref(), Some("tunnel"));
        assert_eq!(kind(&mut state, "x0").as_deref(), Some("unknown"));
        assert_eq!(kind(&mut state, "none"), None);
        fake.mkdir("/sys/class/net/bond0/bonding");
        assert!(state.device(&fake.root(), "bond0").is_none(), "no mode");
    }

    #[test]
    fn python_split_on_a_slash_needs_exactly_two_parts() {
        assert_eq!(two("10.0.0.1/8"), Some(("10.0.0.1", "8")));
        assert_eq!(two("10.0.0.1"), None);
        assert_eq!(two("a/b/c"), None);
    }
}
