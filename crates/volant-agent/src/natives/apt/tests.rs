// SPDX-License-Identifier: GPL-3.0-or-later
//! The apt tests.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use super::status::utc_offset;
use super::{EXTENDED_STATES, LISTS, STAMP, STATUS, UPDATES, answer};
use crate::modules::Context;
use std::fs::{File, FileTimes};
use std::time::Duration;

use serde_json::json;

use super::super::setup::tests::FakeRoot;
use super::*;

/// The reason guard's cases (`natives::tests`): a package with a secret-looking name,
/// installed and marked automatically installed, removed, kept, asked for and misnamed.
pub(in crate::natives) fn secret_probes() -> Vec<crate::natives::tests::Probe> {
    let host = Host::new("secret");
    host.fake
        .write(
            STATUS,
            &format!(
                "{STATUS_TEXT}\nPackage: k3s0secret0r\nStatus: install ok installed\n\
                 Architecture: amd64\nVersion: 1\n"
            ),
        )
        .write(
            EXTENDED_STATES,
            "Package: k3s0secret0r\nArchitecture: amd64\nAuto-Installed: 1\n",
        );
    let reason = |args: Value| match host.answer(&args) {
        NativeRun::Fallback(reason) => Some(reason),
        _ => None,
    };
    vec![
        (
            "apt absent",
            "k3s0secret0r",
            reason(json!({"name": "k3s0secret0r", "state": "absent"})),
        ),
        (
            "apt auto-installed",
            "k3s0secret0r",
            reason(json!({"name": "k3s0secret0r"})),
        ),
        (
            "apt not installed",
            "k3s0secret0s",
            reason(json!({"name": "k3s0secret0s"})),
        ),
        (
            "apt inexact name",
            "k3s0secret0t",
            reason(json!({"name": ["k3s0secret0t!"]})),
        ),
    ]
}

/// Seconds of the stamp the tests set; its fraction is `.7`, which a rounding would push up.
const STAMP_SECONDS: u64 = 1_790_276_713;

const STATUS_TEXT: &str = "\
Package: bash
Essential: yes
Status: install ok installed
Priority: required
Architecture: amd64
Version: 5.2.21-2ubuntu4
Description: GNU Bourne Again SHell
 Bash is an sh-compatible command language interpreter.

Package: coreutils
Status: install ok installed
Architecture: amd64

Package: dpkg
Status: install ok installed
Architecture: amd64

Package: python3-apt
Status: install ok installed
Architecture: amd64

Package: tzdata
Status: install ok installed
Architecture: all

Package: foo
Status: install ok half-configured
Architecture: amd64

Package: bar
Status: deinstall ok installed
Architecture: amd64

Package: held
Status: hold ok installed
Architecture: amd64

Package: gone
Status: deinstall ok config-files
Architecture: amd64

Package: libtwo
Status: install ok installed
Architecture: amd64

Package: libtwo
Status: install ok installed
Architecture: i386

Package: foreign
Status: install ok installed
Architecture: i386

Package: libfoo
Status: install ok installed
Architecture: amd64
";

/// A host with the status above, a cache stamp at `STAMP_SECONDS.7`, an empty dpkg journal
/// and an `apt-mark` on `PATH` that logs its arguments and exits 0.
struct Host {
    fake: FakeRoot,
}

impl Host {
    fn new(name: &str) -> Host {
        let fake = FakeRoot::new(&format!("apt-{name}"));
        fake.write(STATUS, STATUS_TEXT)
            .write(
                EXTENDED_STATES,
                "Package: libfoo\nArchitecture: amd64\nAuto-Installed: 1\n",
            )
            .write(STAMP, "")
            .mkdir(UPDATES)
            .mkdir(LISTS)
            .write("/usr/lib/python3/dist-packages/apt/__init__.py", "")
            .write(
                "/usr/lib/python3/dist-packages/apt_pkg.cpython-312-x86_64-linux-gnu.so",
                "",
            )
            .write("/usr/bin/python3.12", "");
        std::os::unix::fs::symlink("python3.12", fake.0.join("usr/bin/python3")).unwrap();
        let host = Host { fake };
        host.stamp(STAMP_SECONDS, 700_000_000);
        host.apt_mark("exit 0");
        host
    }

    fn stamp(&self, seconds: u64, nanos: u32) {
        let file = File::options()
            .write(true)
            .open(self.fake.0.join(STAMP.trim_start_matches('/')))
            .unwrap();
        let when = UNIX_EPOCH + Duration::new(seconds, nanos);
        file.set_times(FileTimes::new().set_modified(when)).unwrap();
    }

    /// `apt-mark` on the fake `PATH`, logging `$*` then running `body`.
    fn apt_mark(&self, body: &str) {
        let log = self.log_path();
        self.fake.script(
            "/bin/apt-mark",
            &format!("echo \"$*\" >> '{}'\n{body}", log.display()),
        );
    }

    fn log_path(&self) -> PathBuf {
        self.fake.0.join("apt-mark.log")
    }

    /// What `apt-mark` was called with, one line per call.
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.log_path())
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn context(&self) -> Context {
        Context {
            environment: BTreeMap::from([(
                "PATH".to_string(),
                self.fake.0.join("bin").to_string_lossy().into_owned(),
            )]),
            ..Context::default()
        }
    }

    fn answer_at(&self, args: &Value, context: &Context, now: SystemTime) -> NativeRun {
        answer(args.as_object().unwrap(), &self.fake.root(), context, now)
    }

    /// The answer a minute after the stamp.
    fn answer(&self, args: &Value) -> NativeRun {
        self.answer_at(args, &self.context(), soon())
    }

    fn done(&self, args: &Value) -> Map<String, Value> {
        match self.answer(args) {
            NativeRun::Done(result) => result.0,
            NativeRun::Fallback(reason) => panic!("{args}: handed back, {reason}"),
            NativeRun::Cancelled => panic!("{args}: cancelled"),
        }
    }

    /// Hands back, and the host is as it was: no `apt-mark`, the status file untouched.
    fn hands_back(&self, args: &Value) -> String {
        let reason = match self.answer(args) {
            NativeRun::Fallback(reason) => reason,
            NativeRun::Done(result) => panic!("{args}: answered {:?}", result.0),
            NativeRun::Cancelled => panic!("{args}: cancelled"),
        };
        assert_eq!(self.calls(), Vec::<String>::new(), "{args}");
        reason
    }
}

fn soon() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(STAMP_SECONDS + 60)
}

/// The recording's answer less what the controller adds, and less `cache_update_time`, the
/// generator's own cache date.
fn recorded(name: &str) -> Map<String, Value> {
    let text = match name {
        "apt-update-fresh" => {
            include_str!("../../../../volant/tests/golden/module/apt-update-fresh.json")
        }
        "apt-present-installed" => {
            include_str!("../../../../volant/tests/golden/module/apt-present-installed.json")
        }
        "apt-present-list" => {
            include_str!("../../../../volant/tests/golden/module/apt-present-list.json")
        }
        "apt-present-update-fresh" => {
            include_str!("../../../../volant/tests/golden/module/apt-present-update-fresh.json")
        }
        "apt-absent-missing" => {
            include_str!("../../../../volant/tests/golden/module/apt-absent-missing.json")
        }
        _ => unreachable!("{name}"),
    };
    let mut recording: Map<String, Value> = serde_json::from_str(text).unwrap();
    for key in ["_ansible_no_log", "action", "cache_update_time"] {
        recording.remove(key);
    }
    recording
}

/// The fast path answers a wanted installed package like the reference and leaves the rest to the module.
#[test]
fn the_fast_path_answers_installed_packages_like_the_reference() {
    {
        let host = Host::new("answers");
        for (name, args, marks) in [
            (
                "apt-update-fresh",
                json!({"cache_valid_time": 86400, "update_cache": true}),
                None,
            ),
            (
                "apt-present-installed",
                json!({"name": "bash", "state": "present"}),
                None,
            ),
            (
                "apt-present-list",
                json!({"name": ["bash", "coreutils"], "state": "present"}),
                None,
            ),
            (
                "apt-present-update-fresh",
                json!({"cache_valid_time": 86400, "name": "bash", "update_cache": true}),
                None,
            ),
            (
                "apt-absent-missing",
                json!({"name": "volant-no-such-package", "state": "absent"}),
                None,
            ),
        ] {
            let _ = std::fs::remove_file(host.log_path());
            let mut ours = host.done(&args);
            let expected = recorded(name);
            if expected.contains_key("cache_updated") {
                assert_eq!(
                    ours.remove("cache_update_time"),
                    Some(json!(STAMP_SECONDS)),
                    "{name}"
                );
            }
            assert_eq!(Value::Object(ours), Value::Object(expected), "{name}");
            assert_eq!(
                host.calls(),
                marks.map(str::to_string).into_iter().collect::<Vec<_>>(),
                "{name}"
            );
        }
    }
    {
        let host = Host::new("half");
        let reason = host.hands_back(&json!({"name": "foo", "state": "present"}));
        assert_eq!(reason, "a package is not installed");
        host.hands_back(&json!({"name": ["bash", "foo"]}));
        // Removing it is not "nothing to do" either.
        host.hands_back(&json!({"name": "foo", "state": "absent"}));
    }
    {
        let host = Host::new("deinstall");
        for name in ["bar", "held"] {
            host.hands_back(&json!({"name": name}));
            host.hands_back(&json!({"name": name, "state": "absent"}));
        }
    }
}

/// A cache the module would update is left to it, and the cache time is the module's second.
#[test]
fn the_cache_check_is_read_like_the_module_reads_it() {
    {
        let host = Host::new("stale");
        assert_eq!(
            host.hands_back(&json!({"update_cache": true})),
            "update_cache without cache_valid_time"
        );
        for args in [
            json!({"update_cache": true, "name": "bash"}),
            json!({"update_cache": true, "cache_valid_time": 30, "name": "bash"}),
        ] {
            host.hands_back(&args);
        }
        let a_day_later = UNIX_EPOCH + Duration::from_secs(STAMP_SECONDS + 86_401);
        for args in [
            json!({"update_cache": true, "cache_valid_time": 86400}),
            json!({"cache_valid_time": 86400, "name": "volant-none", "state": "absent"}),
        ] {
            assert!(
                matches!(
                    host.answer_at(&args, &host.context(), a_day_later),
                    NativeRun::Fallback(_)
                ),
                "{args}"
            );
        }
        // At the edge, the module's `>=` still says fresh.
        let edge = UNIX_EPOCH + Duration::new(STAMP_SECONDS + 86_400, 700_000_000);
        assert!(matches!(
            host.answer_at(
                &json!({"update_cache": true, "cache_valid_time": 86400}),
                &host.context(),
                edge
            ),
            NativeRun::Done(_)
        ));
    }
    {
        let host = Host::new("floor");
        let time = |host: &Host| {
            host.done(&json!({"update_cache": true, "cache_valid_time": 86400}))
            ["cache_update_time"]
            .clone()
        };
        assert_eq!(time(&host), json!(STAMP_SECONDS));
        host.stamp(STAMP_SECONDS, 999_999_600);
        assert_eq!(time(&host), json!(STAMP_SECONDS + 1));
        host.stamp(STAMP_SECONDS, 999_999_400);
        assert_eq!(time(&host), json!(STAMP_SECONDS));
        // No stamp: the lists directory's time.
        std::fs::remove_file(host.fake.0.join(STAMP.trim_start_matches('/'))).unwrap();
        let lists = File::open(host.fake.0.join(LISTS.trim_start_matches('/'))).unwrap();
        lists
            .set_times(
                FileTimes::new().set_modified(UNIX_EPOCH + Duration::new(STAMP_SECONDS - 5, 1)),
            )
            .unwrap();
        assert_eq!(time(&host), json!(STAMP_SECONDS - 5));
    }
    {
        // The marker, not `TZ` itself, tells the child it is one: a child that did not get `TZ`
        // fails below rather than starting another.
        if std::env::var_os("VOLANT_TZ_CHILD").is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "natives::apt::tests::only_a_cache_check_needs_the_same_offset_now",
                ])
                .env("TZ", "Europe/Paris")
                .env("VOLANT_TZ_CHILD", "1")
                .status()
                .unwrap();
            assert!(status.success(), "the test failed under TZ=Europe/Paris");
            return;
        }
        let summer = 1_790_276_713_i64; // 2026-09-24, CEST
        let winter = summer + 45 * 86_400; // 2026-11-08, CET
        assert_ne!(
            utc_offset(summer),
            utc_offset(winter),
            "this test needs the Europe/Paris zone in the system's tz database"
        );
        let host = Host::new("offset");
        let later = UNIX_EPOCH + Duration::from_secs(winter.unsigned_abs());
        let NativeRun::Done(result) =
            host.answer_at(&json!({"name": "bash"}), &host.context(), later)
        else {
            panic!("a task without a cache check handed back");
        };
        assert_eq!(result.0["cache_update_time"], json!(STAMP_SECONDS));
        assert!(matches!(
            host.answer_at(
                &json!({"name": "bash", "cache_valid_time": 100_000_000}),
                &host.context(),
                later
            ),
            NativeRun::Fallback(_)
        ));
    }
}

/// Everything the fast path does not answer is handed back before any change.
#[test]
#[expect(clippy::too_many_lines)]
fn everything_else_goes_back_to_the_module_before_any_change() {
    {
        let host = Host::new("outside");
        for args in [
            json!({"name": "bash=5.2.21-2ubuntu4"}),
            json!({"name": "bash>=5"}),
            json!({"name": "bas*"}),
            json!({"name": "bash:amd64"}),
            json!({"name": " bash"}),
            json!({"name": "Bash"}),
            json!({"name": []}),
            json!({"name": [1]}),
            json!({"name": "bash", "state": "latest"}),
            json!({"name": "bash", "state": "fixed"}),
            json!({"name": "bash", "state": "bogus"}),
            json!({"deb": "/tmp/x.deb"}),
            json!({"name": "bash", "default_release": "noble"}),
            json!({"name": "gone", "state": "absent", "purge": true}),
            json!({"name": "bash", "autoremove": true}),
            json!({"name": "bash", "autoclean": true}),
            json!({"upgrade": "dist"}),
            json!({"name": "bash", "upgrade": "no"}),
            json!({"name": "bash", "only_upgrade": true}),
            json!({"name": "bash", "clean": true}),
            json!({"name": "bash", "force": "maybe"}),
            json!({"name": "bash", "lock_timeout": "soon"}),
            json!({"name": "bash", "lock_timeout": 1.5}),
            json!({"name": "bash", "dpkg_options": 3}),
            json!({"name": "bash", "purge": null}),
            json!({"name": "bash", "no_such": 1}),
            json!({"name": "bash", "_ansible_check_mode": true}),
            json!({"name": "bash", "pkg": "bash"}),
            json!({"update_cache": true, "cache_valid_time": -1}),
            json!({"update_cache": true, "cache_valid_time": "1_000"}),
            // Not in the status file: unknown, or virtual.
            json!({"name": "volant-no-such-package"}),
            json!({"name": "mail-transport-agent"}),
            // Two architectures installed, or only a foreign one.
            json!({"name": "libtwo"}),
            json!({"name": "foreign"}),
        ] {
            host.hands_back(&args);
        }
        let mut context = host.context();
        context
            .environment
            .insert("TZ".into(), "Europe/Paris".into());
        assert!(matches!(
            host.answer_at(&json!({"name": "bash"}), &context, soon()),
            NativeRun::Fallback(_)
        ));
    }
    {
        let host = Host::new("journal");
        host.fake
            .write("/var/lib/dpkg/updates/0001", "Package: bash\n");
        assert_eq!(
            host.hands_back(&json!({"name": "bash"})),
            "dpkg has pending updates in its journal"
        );

        let host = Host::new("no-python-apt");
        host.fake.write(
            STATUS,
            &STATUS_TEXT.replace("Package: python3-apt\n", "Package: python3-apx\n"),
        );
        let reason = host.hands_back(&json!({"name": "bash"}));
        assert!(reason.starts_with("python3-apt"), "{reason}");

        // dpkg has python3-apt, built for 3.12, and the system python is now a 3.13.
        let host = Host::new("other-python");
        host.fake.write("/usr/bin/python3.13", "");
        std::fs::remove_file(host.fake.0.join("usr/bin/python3")).unwrap();
        std::os::unix::fs::symlink("python3.13", host.fake.0.join("usr/bin/python3")).unwrap();
        let reason = host.hands_back(&json!({"name": "bash"}));
        assert!(reason.starts_with("python3-apt"), "{reason}");
        // The reference tries `/usr/bin/python` next.
        std::os::unix::fs::symlink("python3.12", host.fake.0.join("usr/bin/python")).unwrap();
        host.done(&json!({"name": "bash"}));

        // The extension without the package around it.
        let host = Host::new("no-apt-package");
        std::fs::remove_file(
            host.fake
                .0
                .join("usr/lib/python3/dist-packages/apt/__init__.py"),
        )
        .unwrap();
        let reason = host.hands_back(&json!({"name": "bash"}));
        assert!(reason.starts_with("python3-apt"), "{reason}");

        let host = Host::new("no-apt-mark");
        let mut context = host.context();
        context.environment.insert(
            "PATH".into(),
            host.fake.0.join("nowhere").display().to_string(),
        );
        assert!(matches!(
            host.answer_at(&json!({"name": "bash"}), &context, soon()),
            NativeRun::Fallback(_)
        ));
    }
    {
        let host = Host::new("manual");
        let reason = host.hands_back(&json!({"name": ["bash", "libfoo"]}));
        assert!(
            reason.starts_with("a package is marked automatically"),
            "{reason}"
        );
        host.done(&json!({"name": ["bash", "coreutils", "tzdata"]}));
        assert_eq!(host.calls(), Vec::<String>::new());
    }
}

/// String arguments are converted like the module converts them; a task with no package is answered like it.
#[test]
fn arguments_are_read_like_the_module_reads_them() {
    {
        let host = Host::new("strings");
        let result = host.done(&json!({"update-cache": "Yes ", "cache_valid_time": " 86400"}));
        let args = &result["invocation"]["module_args"];
        assert_eq!(args["update_cache"], json!(true));
        assert_eq!(args["update-cache"], json!("Yes "));
        assert_eq!(args["cache_valid_time"], json!(86400));
        let result = host.done(&json!({
            "pkg": "bash,coreutils",
            "state": "present",
            "install_recommends": "no",
            "lock_timeout": "30",
            "default_release": "",
            "force": 0,
        }));
        let args = &result["invocation"]["module_args"];
        assert_eq!(args["package"], json!(["bash", "coreutils"]));
        assert_eq!(args["pkg"], json!("bash,coreutils"));
        assert_eq!(args["install_recommends"], json!(false));
        assert_eq!(args["lock_timeout"], json!(30));
        assert_eq!(args["force"], json!(false));
        assert_eq!(args["default_release"], json!(""));
        assert_eq!(args["upgrade"], Value::Null);
    }
    {
        let host = Host::new("no-package");
        let three = |result: &Map<String, Value>| {
            (
                result.get("changed").cloned(),
                result.get("cache_updated").cloned(),
                result.get("cache_update_time").cloned(),
            )
        };
        let expected = (
            Some(json!(false)),
            Some(json!(false)),
            Some(json!(STAMP_SECONDS)),
        );
        for args in [
            json!({"cache_valid_time": 86400}),
            json!({"cache_valid_time": "86400", "update_cache": false}),
            json!({"update_cache": false}),
            json!({}),
            json!({"cache_valid_time": 86400, "state": "absent"}),
        ] {
            assert_eq!(three(&host.done(&args)), expected, "{args}");
        }
        let result = host.done(&json!({"state": "absent"}));
        assert_eq!(three(&result), (Some(json!(false)), None, None));
        assert_eq!(host.calls(), Vec::<String>::new());
        let a_day_later = UNIX_EPOCH + Duration::from_secs(STAMP_SECONDS + 86_401);
        assert!(matches!(
            host.answer_at(
                &json!({"cache_valid_time": 86400}),
                &host.context(),
                a_day_later
            ),
            NativeRun::Fallback(_)
        ));
    }
}
