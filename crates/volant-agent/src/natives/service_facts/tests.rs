// SPDX-License-Identifier: GPL-3.0-or-later
//! The service_facts tests.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::imp::{answer, run};
use super::systemd::show;
use super::sysv::status_all;
use crate::modules::Context;
use crate::natives::NativeRun;
use crate::natives::runtime::{Clock, Root, Stop, unbounded};

/// The reason guard's cases (`natives::tests`): `systemctl` lines the native hands back
/// on, each naming a secret-looking unit, and a `PATH` holding one.
pub(in crate::natives) fn secret_probes() -> Vec<crate::natives::tests::Probe> {
    let fixture = |file: &str, content: &str| {
        let host = Host::new();
        host.write(file, content);
        host
    };
    let cases = [
        (
            "service_facts short unit line",
            "k3s0secret2a",
            fixture("/fixture/units.txt", "k3s0secret2a.service loaded active\n"),
            "/usr/bin:/bin",
        ),
        (
            "service_facts two failure states",
            "k3s0secret2b",
            fixture(
                "/fixture/units.txt",
                "k3s0secret2b.service not-found failed failed k3s0secret2b\n",
            ),
            "/usr/bin:/bin",
        ),
        (
            "service_facts short unit file line",
            "k3s0secret2c",
            fixture("/fixture/files.txt", "k3s0secret2c.service\n"),
            "/usr/bin:/bin",
        ),
        (
            "service_facts PATH",
            "K3S0SECRET2D",
            Host::new(),
            "K3S0SECRET2D:/usr/bin:/bin",
        ),
    ];
    cases
        .into_iter()
        .map(|(case, secret, host, path)| {
            let env = BTreeMap::from([("PATH".to_string(), path.to_string())]);
            let reason = match answer(&Map::new(), &Root::at(&host.0), &env, unbounded()) {
                Err(Stop::HandBack(reason)) => Some(reason),
                _ => None,
            };
            (case, secret, reason)
        })
        .collect()
}

/// `list-units` on an Ubuntu 24.04 host with systemd 255, cut down and sanitised (disk
/// ids zeroed), plus the lines whose description names a failure state: the reference
/// reads every word but the last, so `grub-initrd-fallback` and
/// `update-notifier-download` come out `failed`, as measured on that host, while
/// `demo-tail` (added here) ends on one and does not.
const UNITS: &str = r"UNIT                                                          LOAD      ACTIVE   SUB     DESCRIPTION
apparmor.service                                              loaded    active   exited  Load AppArmor profiles
apport-autoreport.service                                     loaded    inactive dead    Process error reports when automatic reporting is enabled
auditd.service                                                not-found inactive dead    auditd.service
cron.service                                                  loaded    active   running Regular background program processing daemon
fwupd-refresh.service                                         loaded    failed   failed  Refresh fwupd metadata and update motd
grub-initrd-fallback.service                                  loaded    inactive dead    GRUB failed boot detection
systemd-fsck@dev-disk-by\x2duuid-0000\x2d0000.service         loaded    active   exited  File System Check on /dev/disk/by-uuid/0000-0000
systemd-journald.service                                      loaded    active   running Journal Service
update-notifier-download.service                              loaded    inactive dead    Download data for packages that failed at package install time
demo-tail.service                                             loaded    inactive dead    Retry what failed
user@1000.service                                             loaded    active   running User Manager for UID 1000
zfs-mount.service                                             not-found inactive dead    zfs-mount.service

Legend: LOAD   → Reflects whether the unit definition was properly loaded.
ACTIVE → The high-level unit activation state, i.e. generalization of SUB.
SUB    → The low-level unit activation state, values depend on unit type.

12 loaded units listed.
To show all installed unit files use 'systemctl list-unit-files'.
";

/// `list-unit-files` on the same host, cut down: templates, aliases, a masked unit, a
/// `bad` one, `enabled-runtime`, and units `list-units` names as well.
const FILES: &str = r"UNIT FILE                                    STATE           PRESET
apparmor.service                             enabled         enabled
apport-autoreport.service                    static          -
autovt@.service                              alias           -
cron.service                                 enabled         enabled
cryptdisks.service                           masked          enabled
dbus-org.freedesktop.resolve1.service        alias           -
dbus-org.freedesktop.timesync1.service       bad             enabled
fwupd-refresh.service                        static          -
getty@.service                               enabled         enabled
grub-initrd-fallback.service                 enabled         enabled
lxd-agent.service                            static          -
systemd-journald.service                     static          -
systemd-remount-fs.service                   enabled-runtime enabled
update-notifier-download.service             static          -
user@.service                                static          -

15 unit files listed.
";

const SYSV: &str = " [ + ]  apparmor\n [ - ]  console-setup.sh\n [ + ]  cron\n [ - ]  cryptdisks\n [ + ]  procps\n [ - ]  uuidd\n";

/// What `systemctl show <unit> --property=ActiveState` gave there: nothing and exit 1
/// for a template, the target's state for an alias.
const SHOWS: [(&str, &str, i32); 8] = [
    ("autovt@.service", "", 1),
    ("getty@.service", "", 1),
    ("user@.service", "", 1),
    ("cryptdisks.service", "ActiveState=inactive\n", 0),
    ("lxd-agent.service", "ActiveState=inactive\n", 0),
    ("systemd-remount-fs.service", "ActiveState=inactive\n", 0),
    (
        "dbus-org.freedesktop.resolve1.service",
        "ActiveState=active\n",
        0,
    ),
    (
        "dbus-org.freedesktop.timesync1.service",
        "ActiveState=inactive\n",
        0,
    ),
];

/// ansible-core 2.19.12's own `ServiceScanService` and `SystemctlScanService`, run on
/// the outputs above, merged as `main`
/// merges them. No warning.
const REFERENCE: &str = r#"{"apparmor": {"name": "apparmor", "source": "sysv", "state": "running"}, "apparmor.service": {"name": "apparmor.service", "source": "systemd", "state": "stopped", "status": "enabled"}, "apport-autoreport.service": {"name": "apport-autoreport.service", "source": "systemd", "state": "stopped", "status": "static"}, "auditd.service": {"name": "auditd.service", "source": "systemd", "state": "stopped", "status": "not-found"}, "autovt@.service": {"name": "autovt@.service", "source": "systemd", "state": "unknown", "status": "alias"}, "console-setup.sh": {"name": "console-setup.sh", "source": "sysv", "state": "stopped"}, "cron": {"name": "cron", "source": "sysv", "state": "running"}, "cron.service": {"name": "cron.service", "source": "systemd", "state": "running", "status": "enabled"}, "cryptdisks": {"name": "cryptdisks", "source": "sysv", "state": "stopped"}, "demo-tail.service": {"name": "demo-tail.service", "source": "systemd", "state": "stopped", "status": "inactive"}, "cryptdisks.service": {"name": "cryptdisks.service", "source": "systemd", "state": "inactive", "status": "masked"}, "dbus-org.freedesktop.resolve1.service": {"name": "dbus-org.freedesktop.resolve1.service", "source": "systemd", "state": "active", "status": "alias"}, "dbus-org.freedesktop.timesync1.service": {"name": "dbus-org.freedesktop.timesync1.service", "source": "systemd", "state": "inactive", "status": "bad"}, "fwupd-refresh.service": {"name": "fwupd-refresh.service", "source": "systemd", "state": "stopped", "status": "failed"}, "getty@.service": {"name": "getty@.service", "source": "systemd", "state": "unknown", "status": "enabled"}, "grub-initrd-fallback.service": {"name": "grub-initrd-fallback.service", "source": "systemd", "state": "stopped", "status": "failed"}, "lxd-agent.service": {"name": "lxd-agent.service", "source": "systemd", "state": "inactive", "status": "static"}, "procps": {"name": "procps", "source": "sysv", "state": "running"}, "systemd-fsck@dev-disk-by\\x2duuid-0000\\x2d0000.service": {"name": "systemd-fsck@dev-disk-by\\x2duuid-0000\\x2d0000.service", "source": "systemd", "state": "stopped", "status": "active"}, "systemd-journald.service": {"name": "systemd-journald.service", "source": "systemd", "state": "running", "status": "static"}, "systemd-remount-fs.service": {"name": "systemd-remount-fs.service", "source": "systemd", "state": "inactive", "status": "enabled-runtime"}, "update-notifier-download.service": {"name": "update-notifier-download.service", "source": "systemd", "state": "stopped", "status": "failed"}, "user@.service": {"name": "user@.service", "source": "systemd", "state": "unknown", "status": "static"}, "user@1000.service": {"name": "user@1000.service", "source": "systemd", "state": "running", "status": "active"}, "uuidd": {"name": "uuidd", "source": "sysv", "state": "stopped"}, "zfs-mount.service": {"name": "zfs-mount.service", "source": "systemd", "state": "stopped", "status": "not-found"}}"#;

/// A host of fake commands under a directory of its own, removed at the end: `systemctl`
/// and `service` print the outputs above, `locale -a` lists `C.utf8`.
struct Host(PathBuf);

impl Drop for Host {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const SYSTEMCTL: &str = r#"#!/bin/sh
d="$(dirname "$0")/../../fixture"
case "$1" in
list-units) cat "$d/units.txt"; exit "$(cat "$d/units.rc" 2>/dev/null || echo 0)" ;;
list-unit-files) cat "$d/files.txt" ;;
show) [ -f "$d/show/$2" ] || { echo "ActiveState=$2"; exit 0; }; cat "$d/show/$2"; exit "$(cat "$d/show/$2.rc")" ;;
esac
"#;

impl Host {
    fn new() -> Host {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = PathBuf::from(format!(
            "/tmp/volant-service-facts-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("run/systemd/system")).unwrap();
        std::fs::create_dir_all(dir.join("fixture/show")).unwrap();
        let host = Host(dir);
        host.command("/usr/bin/systemctl", SYSTEMCTL);
        host.command(
            "/usr/sbin/service",
            "#!/bin/sh\ncat \"$(dirname \"$0\")/../../fixture/sysv.txt\"\n",
        );
        host.command(
            "/usr/bin/locale",
            "#!/bin/sh\nprintf 'C\\nC.utf8\\nPOSIX\\n'\n",
        );
        host.write("/fixture/units.txt", UNITS);
        host.write("/fixture/files.txt", FILES);
        host.write("/fixture/sysv.txt", SYSV);
        for (unit, stdout, rc) in SHOWS {
            host.write(&format!("/fixture/show/{unit}"), stdout);
            host.write(&format!("/fixture/show/{unit}.rc"), &rc.to_string());
        }
        host
    }

    fn write(&self, path: &str, content: &str) {
        let full = self.0.join(path.trim_start_matches('/'));
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }

    fn command(&self, path: &str, script: &str) {
        self.write(path, script);
        let full = self.0.join(path.trim_start_matches('/'));
        std::fs::set_permissions(full, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn answer_with(&self, clock: Clock) -> Result<Map<String, Value>, Stop> {
        let env = BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        answer(&Map::new(), &Root::at(&self.0), &env, clock)
    }

    fn answer(&self) -> Result<Map<String, Value>, Stop> {
        self.answer_with(unbounded())
    }

    fn hands_back(&self) -> String {
        match self.answer() {
            Err(Stop::HandBack(reason)) => reason,
            other => panic!("answered where the native should hand back: {other:?}"),
        }
    }
}

/// A host is answered, a tool without init scripts lists nothing, sysv and the edges are read like the reference.
#[test]
#[expect(clippy::too_many_lines)]
fn a_listing_is_read_as_the_reference_reads_it() {
    {
        let host = Host::new();
        let result = host.answer().unwrap();
        let reference: Value = serde_json::from_str(REFERENCE).unwrap();
        assert_eq!(result["ansible_facts"]["services"], reference);
        assert_eq!(result["invocation"], json!({"module_args": {}}));
    }
    {
        let host = Host::new();
        host.command(
            "/usr/sbin/service",
            "#!/bin/sh\necho ' [ + ]  ghost'\nexit 4\n",
        );
        let result = host.answer().unwrap();
        let services = result["ansible_facts"]["services"].as_object().unwrap();
        assert!(
            services
                .values()
                .all(|service| service["source"] == "systemd")
        );
        assert!(services.contains_key("cron.service"));
    }
    {
        let cases: [(&str, &[(&str, bool)]); 11] = [
            (
                " [ + ]  apparmor\n [ - ]  cron\n",
                &[("apparmor", true), ("cron", false)],
            ),
            (
                " [ + ]  a\n\n  [ - ]\tb c \n [ ? ]  d\n",
                &[("a", true), ("b c ", false)],
            ),
            (" [ + ]  \n", &[(" ", true)]),
            (" [ + ]  \n [ - ]  x\n", &[("[ - ]  x", true)]),
            ("[ + ] x", &[("x", true)]),
            (" [ + ]x\n", &[]),
            (" [ +]  y\n", &[]),
            ("junk [ + ]  z\n [ - ]  w", &[("w", false)]),
            (" [ + ]  \x1c v\n", &[("v", true)]),
            (" [ + ]  \n  \n", &[(" ", true)]),
            (" [ - ] \r\n", &[("\r", false)]),
        ];
        for (text, expected) in cases {
            let expected: Vec<(String, bool)> = expected
                .iter()
                .map(|(name, running)| ((*name).to_string(), *running))
                .collect();
            assert_eq!(status_all(text), expected, "{text:?}");
        }
    }
    {
        let host = Host::new();
        std::fs::remove_file(host.0.join("usr/bin/locale")).unwrap();
        host.answer().unwrap();
        let host = Host::new();
        host.command("/usr/bin/locale", "#!/bin/sh\nexit 1\n");
        host.answer().unwrap();

        let host = Host::new();
        std::fs::remove_file(host.0.join("usr/sbin/service")).unwrap();
        let services = host.answer().unwrap()["ansible_facts"]["services"].clone();
        let services = services.as_object().unwrap();
        assert!(
            services
                .values()
                .all(|service| service["source"] == "systemd")
        );

        let host = Host::new();
        host.write(
            "/fixture/units.txt",
            &format!("{UNITS}cron.service loaded inactive dead Regular background program\n"),
        );
        let result = host.answer().unwrap();
        let services = result["ansible_facts"]["services"].as_object().unwrap();
        assert_eq!(services["cron.service"]["state"], "stopped");

        let host = Host::new();
        host.command(
            "/usr/bin/systemctl",
            &SYSTEMCTL.replacen(
                "#!/bin/sh\n",
                "#!/bin/sh\n[ \"$1\" = list-unit-files ] && exit 1\n",
                1,
            ),
        );
        assert!(host.hands_back().contains("list-unit-files exited 1"));

        let host = Host::new();
        for fixture in ["units.txt", "files.txt", "sysv.txt"] {
            host.write(&format!("/fixture/{fixture}"), "");
        }
        assert!(host.hands_back().contains("no service was found"));

        let host = Host::new();
        let env = BTreeMap::from([("PATH".to_string(), "bin:/usr/bin".to_string())]);
        match answer(&Map::new(), &Root::at(&host.0), &env, unbounded()) {
            Err(Stop::HandBack(reason)) => {
                assert!(
                    reason.contains("PATH holds a relative directory"),
                    "{reason}"
                );
            }
            other => panic!("a relative PATH answered: {other:?}"),
        }

        let host = Host::new();
        host.command("/opt/my tools/systemctl", SYSTEMCTL);
        std::fs::remove_file(host.0.join("usr/bin/systemctl")).unwrap();
        let env = BTreeMap::from([(
            "PATH".to_string(),
            "/opt/my tools:/usr/bin:/bin".to_string(),
        )]);
        match answer(&Map::new(), &Root::at(&host.0), &env, unbounded()) {
            Err(Stop::HandBack(reason)) => {
                assert!(reason.contains("is not a plain path"), "{reason}");
            }
            other => panic!("a path with a space answered: {other:?}"),
        }
    }
}

/// Two failure states on a line, a host outside the subset and the entry point's hand-back all hand back.
#[test]
fn what_the_native_does_not_read_hands_back_whole() {
    {
        let host = Host::new();
        host.write(
            "/fixture/units.txt",
            "gone.service not-found failed failed gone.service\n",
        );
        assert!(host.hands_back().contains("names two failure states"));
    }
    {
        let host = Host::new();
        host.write("/fixture/units.txt", "short.service loaded active\n");
        assert!(host.hands_back().contains("fewer than four fields"));

        let host = Host::new();
        host.write("/fixture/files.txt", "lonely.service\n");
        assert!(host.hands_back().contains("fewer than two words"));

        let host = Host::new();
        host.write("/fixture/units.rc", "1");
        assert!(host.hands_back().contains("list-units exited 1"));

        let host = Host::new();
        host.command("/usr/sbin/chkconfig", "#!/bin/sh\n");
        assert!(host.hands_back().contains("chkconfig is found"));

        let host = Host::new();
        std::fs::remove_dir_all(host.0.join("run/systemd")).unwrap();
        assert_eq!(host.hands_back(), "the host is not run by systemd");

        let host = Host::new();
        std::fs::write(
            host.0.join("fixture/units.txt"),
            b"bad\xff.service a b c d\n",
        )
        .unwrap();
        assert!(host.hands_back().contains("is not UTF-8"));

        let host = Host::new();
        host.command("/usr/sbin/service", "#!/bin/sh\nexit 3\n");
        assert!(host.hands_back().contains("--status-all exited 3"));

        let host = Host::new();
        let args = json!({"x": 1});
        let reason = match answer(
            args.as_object().unwrap(),
            &Root::at(&host.0),
            &BTreeMap::new(),
            unbounded(),
        ) {
            Err(Stop::HandBack(reason)) => reason,
            other => panic!("an argument was accepted: {other:?}"),
        };
        assert!(reason.contains("the argument x"), "{reason}");
    }
    {
        let context = Context {
            timeout: None,
            environment: BTreeMap::new(),
            interpreter: None,
            remote_tmp: "/tmp".into(),
            check_mode: false,
        };
        let args = json!({"x": 1});
        match run(args.as_object().unwrap(), &context, &|| false) {
            NativeRun::Fallback(reason) => {
                assert_eq!(reason, "the argument x is outside the native service_facts");
            }
            _ => panic!("an argument was answered"),
        }
    }
}

/// A hung `locale`, `service`, `list-units` or `show` ends at the task's deadline with
/// `TimedOut`, and at a cancel with `Cancelled`, on whichever thread runs it.
#[test]
fn a_hung_command_ends_at_the_deadline_or_the_cancel() {
    let hang = |sub: &str| {
        SYSTEMCTL.replacen(
            "#!/bin/sh\n",
            &format!("#!/bin/sh\n[ \"$1\" = {sub} ] && exec sleep 60\n"),
            1,
        )
    };
    let hung: [(&str, String); 4] = [
        ("/usr/bin/locale", "#!/bin/sh\nexec sleep 60\n".into()),
        ("/usr/sbin/service", "#!/bin/sh\nexec sleep 60\n".into()),
        ("/usr/bin/systemctl", hang("list-units")),
        ("/usr/bin/systemctl", hang("show")),
    ];
    for (path, script) in hung {
        let host = Host::new();
        host.command(path, &script);
        let started = Instant::now();
        let never = || false;
        let clock = Clock {
            deadline: Some(Instant::now() + Duration::from_millis(100)),
            cancelled: &never,
        };
        let answer = host.answer_with(clock);
        assert!(matches!(answer, Err(Stop::TimedOut)), "{path}: {answer:?}");
        let calls = AtomicUsize::new(0);
        let soon = || calls.fetch_add(1, Ordering::Relaxed) > 3;
        let clock = Clock {
            deadline: None,
            cancelled: &soon,
        };
        let answer = host.answer_with(clock);
        assert!(matches!(answer, Err(Stop::Cancelled)), "{path}: {answer:?}");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{path}: the hung command was waited for"
        );
    }
}

/// A SysV hand-back does not win over systemd's deadline or cancel: the task ends with the
/// stop, not with a hand-back that would run the module past its deadline.
#[test]
fn a_stop_wins_over_a_hand_back_from_the_other_listing() {
    let script = SYSTEMCTL.replacen(
        "#!/bin/sh\n",
        "#!/bin/sh\n[ \"$1\" = list-unit-files ] && exec sleep 60\n",
        1,
    );
    let host = Host::new();
    host.command("/usr/sbin/service", "#!/bin/sh\nexit 3\n");
    host.command("/usr/bin/systemctl", &script);
    let started = Instant::now();
    let never = || false;
    let clock = Clock {
        deadline: Some(Instant::now() + Duration::from_millis(500)),
        cancelled: &never,
    };
    let answer = host.answer_with(clock);
    assert!(matches!(answer, Err(Stop::TimedOut)), "{answer:?}");
    let calls = AtomicUsize::new(0);
    let soon = || calls.fetch_add(1, Ordering::Relaxed) > 3;
    let clock = Clock {
        deadline: None,
        cancelled: &soon,
    };
    let answer = host.answer_with(clock);
    assert!(matches!(answer, Err(Stop::Cancelled)), "{answer:?}");
    assert!(started.elapsed() < Duration::from_secs(30));
}

/// A unit name a shell rewrites is sent through the shell, as the reference sends it:
/// `\x2d` reaches systemctl as `x2d`.
#[test]
fn a_name_the_shell_rewrites_goes_through_the_shell() {
    let host = Host::new();
    let systemctl = host.0.join("usr/bin/systemctl");
    let env = BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
    let (rc, stdout) = show(&systemctl, &env, unbounded(), r"a\x2db.service").unwrap();
    assert_eq!((rc, stdout.as_str()), (0, "ActiveState=ax2db.service\n"));
    let (_, stdout) = show(&systemctl, &env, unbounded(), "a-b.service").unwrap();
    assert_eq!(stdout, "ActiveState=a-b.service\n");
}
