// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::json;
#[cfg(target_os = "linux")]
use volant_protocol::facts::NATIVE_FACT_KEYS;

use std::os::unix::fs::PermissionsExt;
#[cfg(target_os = "linux")]
use std::time::Instant;

use super::collect::filtered;
use super::request::gather_subset;
use super::*;
use crate::natives::runtime::{py_int, py_split, py_strip, splitlines};

/// A directory standing in for `/`, removed when dropped.
pub struct FakeRoot(pub PathBuf);

impl FakeRoot {
    pub fn new(name: &str) -> FakeRoot {
        let dir = std::env::temp_dir().join(format!(
            "volant-setup-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        FakeRoot(dir)
    }

    pub fn root(&self) -> Root {
        Root::at(&self.0)
    }

    pub fn write(&self, path: &str, content: &str) -> &Self {
        let full = self.0.join(path.trim_start_matches('/'));
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
        self
    }

    /// An executable shell script at `path` printing `output` and exiting with `code`.
    pub fn command(&self, path: &str, output: &str, code: i32) -> &Self {
        self.write(
            path,
            &format!("#!/bin/sh\ncat <<'EOF'\n{output}\nEOF\nexit {code}\n"),
        );
        let full = self.0.join(path.trim_start_matches('/'));
        std::fs::set_permissions(full, std::fs::Permissions::from_mode(0o755)).unwrap();
        self
    }

    /// An executable shell script at `path` running `body`.
    pub fn script(&self, path: &str, body: &str) -> &Self {
        self.write(path, &format!("#!/bin/sh\n{body}\n"));
        let full = self.0.join(path.trim_start_matches('/'));
        std::fs::set_permissions(full, std::fs::Permissions::from_mode(0o755)).unwrap();
        self
    }

    pub fn mkdir(&self, path: &str) -> &Self {
        std::fs::create_dir_all(self.0.join(path.trim_start_matches('/'))).unwrap();
        self
    }

    /// A host the probe does not have to run for: the interpreter's answer given, and `PATH`
    /// pointing into the fake root's own `usr/bin`.
    pub fn host<'a>(&self, root: &'a Root, probe: &'a python::Probe) -> Host<'a> {
        let mut env = probe.env.clone();
        env.insert("PATH".into(), "/usr/bin".into());
        Host {
            root,
            env,
            probe,
            clock: unbounded(),
        }
    }
}

impl Drop for FakeRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The interpreter's answer on an Ubuntu 24.04 host, as the probe reads it there.
pub fn probe() -> python::Probe {
    python::Probe {
        python: json!({
            "version": {"major": 3, "minor": 12, "micro": 3, "releaselevel": "final", "serial": 0},
            "version_info": [3, 12, 3, "final", 0],
            "executable": "/usr/bin/python3",
            "has_sslcontext": true,
            "type": "cpython",
        }),
        python_version: "3.12.3".into(),
        bits: "64bit".into(),
        env: BTreeMap::from([
            ("HOME".to_string(), "/home/user".to_string()),
            ("LOGNAME".to_string(), "user".to_string()),
        ]),
        lc_time: Some("C.UTF-8".into()),
        // Measured on target (UTC), the instant's fields sanitised to one fixed date.
        date_time: json!({
            "year": "2026", "month": "09", "weekday": "Thursday", "weekday_number": "4",
            "weeknumber": "38", "day": "24", "hour": "15", "minute": "47", "second": "42",
            "epoch": "1790264862", "epoch_int": "1790264862", "date": "2026-09-24",
            "time": "15:47:42", "iso8601_micro": "2026-09-24T15:47:42.158470Z",
            "iso8601": "2026-09-24T15:47:42Z", "iso8601_basic": "20260924T154742158470",
            "iso8601_basic_short": "20260924T154742", "tz": "UTC", "tz_dst": "UTC",
            "tz_offset": "+0000",
        }),
        selinux: Some(false),
        distro: None,
    }
}

pub const UBUNTU_OS_RELEASE: &str = r#"PRETTY_NAME="Ubuntu 24.04.4 LTS"
NAME="Ubuntu"
VERSION_ID="24.04"
VERSION="24.04.4 LTS (Noble Numbat)"
VERSION_CODENAME=noble
ID=ubuntu
ID_LIKE=debian
HOME_URL="https://www.ubuntu.com/"
SUPPORT_URL="https://help.ubuntu.com/"
BUG_REPORT_URL="https://bugs.launchpad.net/ubuntu/"
PRIVACY_POLICY_URL="https://www.ubuntu.com/legal/terms-and-policies/privacy-policy"
UBUNTU_CODENAME=noble
LOGO=ubuntu-logo
"#;

pub const DEBIAN_OS_RELEASE: &str = r#"PRETTY_NAME="Debian GNU/Linux 12 (bookworm)"
NAME="Debian GNU/Linux"
VERSION_ID="12"
VERSION="12 (bookworm)"
VERSION_CODENAME=bookworm
ID=debian
HOME_URL="https://www.debian.org/"
SUPPORT_URL="https://www.debian.org/support"
BUG_REPORT_URL="https://bugs.debian.org/"
"#;

/// Every source a Debian 12 host can give the collectors, the DSA host key included, so that
/// every key the native can produce is produced.
#[cfg(target_os = "linux")]
pub fn debian_root(name: &str) -> FakeRoot {
    let fake = FakeRoot::new(name);
    fake.write("/etc/os-release", DEBIAN_OS_RELEASE)
        .write("/etc/debian_version", "12.7\n")
        .command(
            "/usr/bin/lsb_release",
            "Distributor ID:\tDebian\nDescription:\tDebian GNU/Linux 12 (bookworm)\nRelease:\t12\nCodename:\tbookworm",
            0,
        )
        .command("/usr/sbin/capsh", "Current: =ep\nBounding set =cap_chown", 0)
        .write("/etc/nsswitch.conf", "passwd: files systemd\nhosts: files dns\n")
        .write("/etc/passwd", "root:x:0:0:root:/root:/bin/bash\nuser:x:1000:1000:A User,,,:/home/user:/bin/bash\n")
        .write("/proc/1/comm", "systemd\n")
        .write("/proc/cmdline", "BOOT_IMAGE=/vmlinuz ro quiet\n")
        .write("/proc/sys/crypto/fips_enabled", "0\n")
        .write("/etc/resolv.conf", "nameserver 127.0.0.53\noptions edns0 trust-ad\nsearch example\n")
        .write("/etc/machine-id", "0123456789abcdef0123456789abcdef\n")
        .write("/etc/hosts", "127.0.0.1 localhost\n127.0.1.1 probe-hostname\n")
        .write(
            "/proc/cpuinfo",
            "processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: Intel(R) Xeon(R)\n\
                 physical id\t: 0\nsiblings\t: 1\ncore id\t\t: 0\ncpu cores\t: 1\n\
                 flags\t\t: fpu vme\n",
        )
        .write(
            "/proc/meminfo",
            "MemTotal: 2048 kB\nMemFree: 1024 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
        );
    network::tests::sysfs(&fake);
    network::tests::install_ip(&fake, network::tests::IP);
    for algo in ["dsa", "rsa", "ecdsa", "ed25519"] {
        fake.write(
            &format!("/etc/ssh/ssh_host_{algo}_key.pub"),
            &format!("ssh-{algo} AAAA{algo} root@probe-hostname\n"),
        );
    }
    fake
}

/// A host with every source gets exactly the listed keys and the reference's invocation.
#[test]
#[cfg(target_os = "linux")]
#[expect(clippy::too_many_lines)]
fn a_host_with_every_source_is_answered_in_full() {
    {
        let fake = debian_root("keys");
        let root = fake.root();
        let probe = probe();
        let host = fake.host(&root, &probe);
        let lsb = LsbRelease::run(host.root, &host.env, unbounded()).unwrap();
        let collected = [
            distribution::collect(&host, &lsb).unwrap(),
            apparmor::collect(&host),
            caps::collect(&host).unwrap(),
            cmdline::collect(&host),
            date_time::collect(&host).unwrap(),
            dns::collect(&host),
            env::collect(&host),
            fips::collect(&host),
            local::collect(&host, Some(DEFAULT_FACT_PATH)).unwrap(),
            lsb::collect(&host, &lsb).unwrap(),
            pkg_mgr::collect(&host).unwrap(),
            platform::collect_with(
                &host,
                &platform::Uname {
                    system: "Linux".into(),
                    node: "probe-hostname".into(),
                    release: "6.1.0-18-amd64".into(),
                    version: "#1 SMP PREEMPT_DYNAMIC Debian 6.1.76-1".into(),
                    machine: "x86_64".into(),
                },
            )
            .unwrap(),
            python::collect(&host),
            selinux::collect(&host).unwrap(),
            service_mgr::collect(&host).unwrap(),
            ssh_pub_keys::collect(&host).unwrap(),
            user::collect(&host).unwrap(),
            hardware::collect(host.root, "x86_64"),
            network::collect(host.root, &host.env, unbounded()).unwrap(),
        ];
        let mut keys: Vec<String> = collected
            .into_iter()
            .flatten()
            .map(|(name, _)| format!("ansible_{name}"))
            .chain(["gather_subset".to_string(), "module_setup".to_string()])
            .collect();
        keys.sort();
        assert_eq!(keys, NATIVE_FACT_KEYS);
    }
    {
        let fake = debian_root("answer");
        fake.write(
            "/etc/hosts",
            &format!("127.0.1.1 {}\n", platform::uname().node),
        )
        .write(
            "/usr/bin/lsb_release",
            "#!/bin/sh\necho \"Distributor ID:\t$LOGNAME\"\n",
        );
        std::fs::set_permissions(
            fake.0.join("usr/bin/lsb_release"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let agent = BTreeMap::from([("LOGNAME".to_string(), "the-agent-s-own".to_string())]);
        let root = fake.root();
        let interpreter = python::tests::fake_interpreter(&fake, &probe());
        let context = Context {
            interpreter: Some(interpreter),
            environment: BTreeMap::from([("PATH".to_string(), "/usr/bin".to_string())]),
            ..Context::default()
        };
        let args = json!({"gather_subset": "!all"})
            .as_object()
            .unwrap()
            .clone();
        let result = answer(&args, &root, &context, Some(agent.clone()), unbounded()).unwrap();
        assert_eq!(
            result["invocation"],
            json!({"module_args": {
                "gather_subset": ["!all"],
                "gather_timeout": 10,
                "filter": [],
                "fact_path": "/etc/ansible/facts.d",
            }})
        );
        assert_eq!(result["ansible_facts"]["gather_subset"], json!(["!all"]));
        assert_eq!(result["ansible_facts"]["module_setup"], json!(true));
        assert_eq!(
            result["ansible_facts"]["ansible_distribution"],
            json!("Debian")
        );
        assert_eq!(
            result["ansible_facts"]["ansible_lsb"]["id"],
            json!("user"),
            "the module's LOGNAME, not the agent's"
        );
        for key in ["ansible_processor", "ansible_interfaces"] {
            assert!(
                !result["ansible_facts"]
                    .as_object()
                    .unwrap()
                    .contains_key(key),
                "{key} collected for !all"
            );
        }

        let args = json!({"gather_subset": ["!all", "hardware", "network"]})
            .as_object()
            .unwrap()
            .clone();
        let facts =
            &answer(&args, &root, &context, Some(agent), unbounded()).unwrap()["ansible_facts"];
        assert_eq!(facts["ansible_processor_vcpus"], json!(1));
        assert_eq!(facts["ansible_default_ipv4"]["interface"], json!("eth0"));

        // The `package` plugin's own gather, and the processor count read through a filter
        // that leaves out the architecture it is counted by.
        for (args, facts) in [
            (
                json!({"gather_subset": "!all", "filter": "ansible_pkg_mgr"}),
                json!({"ansible_pkg_mgr": "apt"}),
            ),
            (
                json!({"gather_subset": ["!all"], "filter": ["service_mgr", "gather_subset"]}),
                json!({"ansible_service_mgr": "systemd", "gather_subset": ["!all"]}),
            ),
            (
                json!({"gather_subset": ["!all", "hardware"], "filter": ["processor_vcpus"]}),
                json!({"ansible_processor_vcpus": 1}),
            ),
        ] {
            let result = answer(
                args.as_object().unwrap(),
                &root,
                &context,
                agent_env(),
                unbounded(),
            )
            .unwrap();
            assert_eq!(result["ansible_facts"], facts, "{args}");
            let converted = match &args["filter"] {
                Value::String(one) => json!([one]),
                list => list.clone(),
            };
            assert_eq!(result["invocation"]["module_args"]["filter"], converted);
        }
    }
}

/// Only a subset is answered; arguments, locale, time zone or another distribution hand back.
#[test]
#[expect(clippy::too_many_lines)]
fn the_native_answers_a_subset_and_hands_back_the_rest() {
    {
        let runs = |value: Value| {
            let (_, collectors) = gather_subset(Some(&value)).unwrap();
            (
                collectors.contains("hardware"),
                collectors.contains("network"),
            )
        };
        assert_eq!(runs(json!(["all"])), (true, true));
        assert_eq!(runs(json!(["min"])), (false, false));
        assert_eq!(runs(json!(["!all", "network"])), (false, true));
        assert_eq!(runs(json!(["all", "!hardware"])), (false, true));
        assert_eq!(runs(json!(["!hardware"])), (false, false), "min only");
        assert_eq!(runs(json!("!all,processor_count")), (true, false));
        assert_eq!(
            runs(json!(["all", "!network", "default_ipv4"])),
            (true, true)
        );
        assert_eq!(runs(json!(["all", "!virtual", "!ohai"])), (true, true));
        assert_eq!(
            gather_subset(None).unwrap().0,
            vec!["all"],
            "absent is all, as the fact and invocation say"
        );
        for not_answered in [
            json!(["virtual"]),
            json!(["min", "ohai"]),
            json!(["mounts"]),
            json!(["!all", "devices"]),
            json!(["dmi"]),
            json!(["all", "is_chroot"]),
            json!(["!all", "!min", "network"]),
        ] {
            assert!(
                gather_subset(Some(&not_answered)).is_err(),
                "{not_answered}"
            );
        }
        for min in [
            json!(["min"]),
            json!(["!all"]),
            json!(["!hardware"]),
            json!(["!platform"]),
            json!(["!distribution"]),
            json!(["!all", "dns"]),
            json!(["!dns", "dns"]),
            json!(["!all", "!nothing"]),
            json!(["!machine_id"]),
            json!("!all,min"),
        ] {
            assert!(gather_subset(Some(&min)).is_ok(), "{min}");
        }
        assert_eq!(
            gather_subset(Some(&json!("!all,min"))).unwrap().0,
            vec!["!all", "min"]
        );
        for not_min in [
            json!(["!all", "!min"]),
            json!(["!min", "min"]),
            json!(["!service_mgr", "!platform"]),
            json!(["!dns"]),
            json!(["!min", "dns"]),
            json!("min, !hardware"),
            json!([1]),
            json!(null),
            json!([]),
        ] {
            assert!(gather_subset(Some(&not_min)).is_err(), "{not_min}");
        }
    }
    {
        use crate::testing::args;
        let ok = request(&args(
            json!({"gather_subset": ["min"], "filter": [], "gather_timeout": 5}),
        ))
        .unwrap();
        assert_eq!(ok.fact_path.as_deref(), Some(DEFAULT_FACT_PATH));
        assert_eq!(
            request(&args(json!({"gather_subset": ["min"], "fact_path": null})))
                .unwrap()
                .fact_path,
            None
        );
        assert_eq!(
            request(&args(json!({"gather_subset": ["min"], "filter": "a,b"})))
                .unwrap()
                .filter,
            ["a", "b"],
            "split on commas, as type='list' splits a string"
        );
        for refused in [
            json!({"gather_subset": ["min"], "filter": [1]}),
            json!({"gather_subset": ["min"], "filter": 5}),
            json!({"gather_subset": ["min"], "filter": {"a": 1}}),
            json!({"gather_subset": ["min"], "gather_timeout": "10"}),
            json!({"gather_subset": ["min"], "fact_path": "~/facts"}),
            json!({"gather_subset": ["min"], "fact_path": "facts"}),
            json!({"gather_subset": ["min"], "fact_path": "/etc/$X"}),
            json!({"gather_subset": ["min"], "no_such": 1}),
            json!({"gather_subset": ["min"], "_ansible_check_mode": true}),
        ] {
            assert!(request(&args(refused.clone())).is_err(), "{refused}");
        }
    }
    {
        let names = [
            "ansible_pkg_mgr",
            "ansible_service_mgr",
            "ansible_memtotal_mb",
            "ansible_eth0",
            "ansible_eth1",
            "ansible_eth3",
            "ansible_eth-",
            "gather_subset",
            "module_setup",
            "ansible_distribution_version",
            "facter_x",
            "ansible_]x",
            "ansible_[x",
            "ansible_!x",
            "Ansible_pkg_mgr",
        ];
        let group: Map<String, Value> = names
            .iter()
            .enumerate()
            .map(|(i, name)| ((*name).to_string(), json!(i)))
            .collect();
        let kept = |filter: &[&str]| -> Vec<String> {
            let filter: Vec<String> = filter.iter().map(|p| (*p).to_string()).collect();
            let mut facts = Map::new();
            filtered(group.clone(), &filter, &mut facts);
            facts.keys().cloned().collect()
        };
        let cases: &[(&str, &[&str])] = &[
            ("ansible_pkg_mgr", &["ansible_pkg_mgr"]),
            ("pkg_mgr", &["ansible_pkg_mgr"]),
            ("*_mb", &["ansible_memtotal_mb"]),
            ("ansible_eth[0-2]", &["ansible_eth0", "ansible_eth1"]),
            ("eth[!0]", &["ansible_eth1", "ansible_eth3", "ansible_eth-"]),
            ("[a]nsible_pkg*", &["ansible_pkg_mgr"]),
            ("gather_subset", &["gather_subset"]),
            ("distribution?version", &["ansible_distribution_version"]),
            ("[z-a]*", &[]),
            ("ansible_[!]x", &[]),
            ("ansible_[]]x", &["ansible_]x"]),
            ("ansible_[", &[]),
            ("*[x", &["ansible_[x"]),
            ("facter*", &["facter_x"]),
            ("ohai*", &[]),
            ("ansible_eth[1-]", &["ansible_eth1", "ansible_eth-"]),
            ("ansible_eth[-1]", &["ansible_eth1", "ansible_eth-"]),
            ("ansible_eth[!]", &[]),
            ("ansible_eth[a-c-e3]", &["ansible_eth3", "ansible_eth-"]),
            ("ANSIBLE_*", &[]),
            ("*[0-9]", &["ansible_eth0", "ansible_eth1", "ansible_eth3"]),
        ];
        for (pattern, expected) in cases {
            assert_eq!(kept(&[pattern]), *expected, "{pattern}");
        }
        for everything in [&[""][..], &["*"], &[]] {
            assert_eq!(kept(everything), names, "{everything:?}");
        }
        assert_eq!(
            kept(&["*_mb", "pkg_mgr", "ansible_pkg*"]),
            ["ansible_memtotal_mb", "ansible_pkg_mgr"]
        );
    }
    {
        let fake = FakeRoot::new("task-env");
        for key in ["LANG", "LC_ALL", "LC_TIME", "LANGUAGE", "TZ"] {
            let context = Context {
                environment: BTreeMap::from([(key.to_string(), "C".to_string())]),
                ..Context::default()
            };
            let request = Request {
                gather_subset: vec!["min".into()],
                hardware: false,
                network: false,
                fact_path: None,
                filter: Vec::new(),
            };
            let Err(Stop::HandBack(reason)) =
                collect(&request, &fake.root(), &context, agent_env(), unbounded())
            else {
                panic!("{key} is answered");
            };
            assert!(reason.contains(key), "{reason}");
        }
    }
    {
        let fake = FakeRoot::new("gate-first");
        let mark = fake.0.join("probed");
        fake.write("/etc/os-release", "ID=fedora\nVERSION_ID=40\n")
            .script("/usr/bin/fake-python", &format!("touch {}", mark.display()));
        let context = Context {
            interpreter: Some(fake.0.join("usr/bin/fake-python").display().to_string()),
            environment: BTreeMap::from([("PATH".to_string(), "/usr/bin".to_string())]),
            ..Context::default()
        };
        let request = Request {
            gather_subset: vec!["min".into()],
            hardware: false,
            network: false,
            fact_path: None,
            filter: Vec::new(),
        };
        let Err(Stop::HandBack(reason)) =
            collect(&request, &fake.root(), &context, agent_env(), unbounded())
        else {
            panic!("fedora is answered");
        };
        assert!(reason.contains("fedora"), "{reason}");
        assert!(!mark.exists(), "the interpreter ran");
    }
}

/// A hung command, `lsb_release` or `ip` ends at the timeout, the cancel or a failed probe.
#[test]
#[cfg(target_os = "linux")]
#[expect(clippy::too_many_lines)]
fn a_hung_command_ends_at_the_timeout_the_cancel_or_a_failed_probe() {
    {
        let fake = debian_root("hung");
        fake.script("/usr/bin/fake-python", "sleep 30")
            .script("/usr/bin/lsb_release", "sleep 30");
        let context = Context {
            interpreter: Some(fake.0.join("usr/bin/fake-python").display().to_string()),
            environment: BTreeMap::from([("PATH".to_string(), "/usr/bin".to_string())]),
            timeout: Some(std::time::Duration::from_secs(1)),
            ..Context::default()
        };
        let request = Request {
            gather_subset: vec!["min".into()],
            hardware: false,
            network: false,
            fact_path: None,
            filter: Vec::new(),
        };
        let started = Instant::now();
        let clock = Clock {
            deadline: Some(started + std::time::Duration::from_secs(1)),
            cancelled: &|| false,
        };
        let stop = collect(&request, &fake.root(), &context, agent_env(), clock).unwrap_err();
        assert!(matches!(stop, Stop::TimedOut), "{stop:?}");
        assert!(started.elapsed().as_secs() < 10, "{:?}", started.elapsed());

        let asked = std::cell::Cell::new(0);
        let cancelled = || {
            asked.set(asked.get() + 1);
            asked.get() > 5
        };
        let started = Instant::now();
        let clock = Clock {
            deadline: None,
            cancelled: &cancelled,
        };
        let stop = collect(&request, &fake.root(), &context, agent_env(), clock).unwrap_err();
        assert!(matches!(stop, Stop::Cancelled), "{stop:?}");
        assert!(started.elapsed().as_secs() < 10, "{:?}", started.elapsed());
    }
    {
        let fake = debian_root("hung-lsb");
        let interpreter = python::tests::fake_interpreter(&fake, &probe());
        fake.script("/usr/bin/lsb_release", "sleep 30");
        let context = Context {
            interpreter: Some(interpreter),
            environment: BTreeMap::from([("PATH".to_string(), "/usr/bin".to_string())]),
            ..Context::default()
        };
        let request = Request {
            gather_subset: vec!["min".into()],
            hardware: false,
            network: false,
            fact_path: None,
            filter: Vec::new(),
        };
        let asked = std::cell::Cell::new(0);
        let cancelled = || {
            asked.set(asked.get() + 1);
            asked.get() > 5
        };
        let started = Instant::now();
        let clock = Clock {
            deadline: None,
            cancelled: &cancelled,
        };
        let stop = collect(&request, &fake.root(), &context, agent_env(), clock).unwrap_err();
        assert!(matches!(stop, Stop::Cancelled), "{stop:?}");
        assert!(started.elapsed().as_secs() < 10, "{:?}", started.elapsed());

        let started = Instant::now();
        let clock = Clock {
            deadline: Some(started + std::time::Duration::from_secs(1)),
            cancelled: &|| false,
        };
        let stop = collect(&request, &fake.root(), &context, agent_env(), clock).unwrap_err();
        assert!(matches!(stop, Stop::TimedOut), "{stop:?}");
        assert!(started.elapsed().as_secs() < 10, "{:?}", started.elapsed());
    }
    {
        let fake = debian_root("hung-ip");
        let interpreter = python::tests::fake_interpreter(&fake, &probe());
        network::tests::install_ip(&fake, "#!/bin/sh\nsleep 30\n");
        let mut context = Context {
            interpreter: Some(interpreter),
            environment: BTreeMap::from([("PATH".to_string(), "/usr/bin".to_string())]),
            ..Context::default()
        };
        let request = Request {
            gather_subset: vec!["min".into()],
            hardware: false,
            network: true,
            fact_path: None,
            filter: Vec::new(),
        };
        let started = Instant::now();
        // Answers only once the probe and `lsb_release` are long done, so that the cancel
        // reaches the wait for `ip` and nothing before it.
        let cancelled = || started.elapsed() > std::time::Duration::from_secs(1);
        let clock = Clock {
            deadline: None,
            cancelled: &cancelled,
        };
        let stop = collect(&request, &fake.root(), &context, agent_env(), clock).unwrap_err();
        assert!(matches!(stop, Stop::Cancelled), "{stop:?}");
        assert!(started.elapsed().as_secs() < 10, "{:?}", started.elapsed());

        context.interpreter = Some("/nonexistent/python3".into());
        let started = Instant::now();
        let stop = collect(&request, &fake.root(), &context, agent_env(), unbounded()).unwrap_err();
        assert!(matches!(stop, Stop::HandBack(_)), "{stop:?}");
        assert!(started.elapsed().as_secs() < 10, "{:?}", started.elapsed());
    }
}

/// A `PYTHONPATH` in the task's `environment` that brings an old `distro` makes the native hand
/// back, as the module started under that environment would import it.
#[test]
#[cfg(target_os = "linux")]
fn a_task_pythonpath_with_an_old_distro_hands_back() {
    let fake = debian_root("task-distro");
    fake.write("/site/distro.py", "__version__ = \"1.5.0\"\n");
    let context = Context {
        interpreter: Some("python3".into()),
        environment: BTreeMap::from([(
            "PYTHONPATH".to_string(),
            fake.0.join("site").display().to_string(),
        )]),
        ..Context::default()
    };
    let request = Request {
        gather_subset: vec!["min".into()],
        hardware: false,
        network: false,
        fact_path: None,
        filter: Vec::new(),
    };
    let Err(Stop::HandBack(reason)) =
        collect(&request, &fake.root(), &context, agent_env(), unbounded())
    else {
        panic!("the old distro is not seen");
    };
    assert!(reason.contains("1.5.0"), "{reason}");
}

#[test]
fn python_string_helpers_split_like_python() {
    assert_eq!(
        splitlines("a\r\nb\rc\n\nd\x1ce"),
        vec!["a", "b", "c", "", "d", "e"]
    );
    assert_eq!(splitlines("a\n"), vec!["a"]);
    assert_eq!(py_strip("\x1f a \u{3000}"), "a");
    assert_eq!(py_int(" -1_000\n", 10), Some(-1000));
    assert_eq!(py_int("0x1003", 16), Some(0x1003));
    assert_eq!(py_int("0x_ff", 16), Some(255));
    for raises in ["", "1__0", "_1", "1_", "-+1", "0x10", "1.5"] {
        assert_eq!(py_int(raises, 10), None, "{raises}");
    }
    assert_eq!(
        py_split(" a\x1cb  c ").collect::<Vec<_>>(),
        vec!["a", "b", "c"]
    );
}
