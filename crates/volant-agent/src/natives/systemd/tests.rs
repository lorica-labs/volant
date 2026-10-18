// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use serde_json::json;
use volant_protocol::TaskResult;

use super::*;
use crate::natives::NativeRun;
use crate::natives::common::golden::within;
use crate::natives::runtime::unbounded;
use crate::testing::{args, scratch};

/// `systemctl show cron` on an Ubuntu 24.04 host (systemd 255), with its `InvocationID`
/// replaced. 271 lines, none of them multi-line.
const SHOW_CRON: &str = include_str!("../systemd_show_cron.txt");

/// A directory holding a fake `systemctl` first on the module's `PATH`, which appends each
/// call's arguments to `calls` and runs `body` (`sh`), exiting 0 unless `body` exits itself,
/// and an `etc` without init scripts.
struct Fake(
    String,
    #[expect(dead_code, reason = "keeps the directory until the test ends")] tempfile::TempDir,
);

impl Fake {
    fn new(body: &str) -> Fake {
        let tmp = scratch();
        let dir = tmp.path().display().to_string();
        fs::create_dir_all(format!("{dir}/bin")).unwrap();
        fs::create_dir_all(format!("{dir}/etc/init.d")).unwrap();
        fs::write(format!("{dir}/show"), SHOW_CRON).unwrap();
        let script = format!("{dir}/bin/systemctl");
        fs::write(
            &script,
            format!("#!/bin/sh\necho \"$*\" >> {dir}/calls\ncd {dir}\n{body}\ntrue\n"),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        Fake(dir, tmp)
    }

    fn context(&self) -> Context {
        Context {
            environment: BTreeMap::from([(
                "PATH".to_string(),
                format!("{}/bin:/usr/bin:/bin", self.0),
            )]),
            ..Context::default()
        }
    }

    fn etc(&self) -> String {
        format!("{}/etc", self.0)
    }

    /// The native's answer, a failure from inside a step included, as `NATIVE.run` gives it.
    fn answer(&self, args: Value) -> Result<Map<String, Value>, Exit> {
        match answer(
            args.as_object().unwrap(),
            &self.context(),
            unbounded(),
            &self.etc(),
        ) {
            Err(Exit::Failed(result)) => Ok(result),
            other => other,
        }
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(format!("{}/calls", self.0))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

/// A `systemctl` that answers `show` with cron's output, `is-enabled` with `enabled`, and
/// succeeds at everything else.
const CRON: &str = r#"case "$1" in
  show) cat show ;;
  is-enabled) echo enabled ;;
esac"#;

/// Compares an answer with the golden recording of the same case, every key but `status`,
/// which is the host's own and is checked by each test.
fn assert_like_recording(recording: &str, answer: &Map<String, Value>) {
    let mut want: Map<String, Value> = serde_json::from_str(recording).unwrap();
    for key in ["action", "_ansible_no_log", "exception", "_after", "status"] {
        want.remove(key);
    }
    let mut ours = crate::modules::module_result(answer.clone()).0;
    ours.remove("status");
    assert_eq!(Value::Object(ours), Value::Object(want));
}

/// The module's split of a real `systemctl show`, and the two shapes of a brace left open.
#[test]
fn systemctl_show_splits_like_the_module() {
    let status = parse_systemctl_show(SHOW_CRON);
    assert_eq!(status.len(), 271);
    assert_eq!(status["ActiveState"], "active");
    assert_eq!(status["LoadState"], "loaded");
    assert_eq!(status["UnitFileState"], "enabled");
    assert_eq!(status["Id"], "cron.service");
    assert_eq!(
        status["ExecStart"],
        "{ path=/usr/sbin/cron ; argv[]=/usr/sbin/cron -f -P $EXTRA_OPTS ; \
             ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; \
             status=0/0 }"
    );
    assert_eq!(
        Value::Object(parse_systemctl_show(
            "Description={ open brace  \nExecStart={ path=/a ;\n  argv[]=/a }  \n\
                 no equals sign\nExecStop={ one line }\nExecReload={ never closed\nId=x\n"
        )),
        json!({
            "Description": "{ open brace",
            "ExecStart": "{ path=/a ;\n  argv[]=/a }",
            "ExecStop": "{ one line }",
        })
    );
}

/// A running enabled unit is left alone; `enabled`, daemon reload, a missing unit and restarts report like the module.
#[test]
fn a_unit_is_left_alone_started_or_reported_like_the_module() {
    {
        let fake = Fake::new(CRON);
        let answer = fake
            .answer(json!({"name": "cron", "state": "started", "enabled": true}))
            .unwrap();
        assert_like_recording(
            include_str!("../../../../volant/tests/golden/module/systemd-started-same.json"),
            &answer,
        );
        assert_eq!(
            Value::Object(answer["status"].as_object().unwrap().clone()),
            Value::Object(parse_systemctl_show(SHOW_CRON))
        );
        assert_eq!(fake.calls(), ["show cron", "is-enabled cron -l"]);
    }
    {
        let fake = Fake::new(CRON);
        let answer = fake
            .answer(json!({"name": "cron.service", "enabled": true}))
            .unwrap();
        assert_like_recording(
            include_str!("../../../../volant/tests/golden/module/systemd-enabled-only.json"),
            &answer,
        );
        assert_eq!(
            fake.calls(),
            ["show cron.service", "is-enabled cron.service -l"]
        );
    }
    {
        let fake = Fake::new("");
        let answer = fake.answer(json!({"daemon_reload": true})).unwrap();
        let mut want: Map<String, Value> = serde_json::from_str(include_str!(
            "../../../../volant/tests/golden/module/systemd-daemon-reload.json"
        ))
        .unwrap();
        want.remove("action");
        want.remove("_ansible_no_log");
        assert_eq!(Value::Object(answer), Value::Object(want));
        assert_eq!(fake.calls(), ["daemon-reload"]);
    }
    {
        let fake =
            Fake::new(r#"[ "$1" = show ] && printf 'LoadState=not-found\nActiveState=inactive\n'"#);
        let answer = fake
            .answer(json!({"name": "volant-no-such-unit", "state": "started"}))
            .unwrap();
        assert_like_recording(
            include_str!("../../../../volant/tests/golden/module/systemd-missing-unit.json"),
            &answer,
        );
        assert_eq!(fake.calls(), ["show volant-no-such-unit"]);
    }
    {
        let body = r#"case "$1" in
  show) printf 'LoadState=loaded\nActiveState=active\nMainPID=%s\n' "$(cat pid 2>/dev/null || echo 100)" ;;
  restart) echo 200 > pid ;;
esac"#;
        for (state, action) in [("restarted", "restart"), ("reloaded", "reload")] {
            let fake = Fake::new(body);
            let answer = fake
                .answer(json!({"name": "volant-golden", "state": state}))
                .unwrap();
            assert_eq!(answer["changed"], true, "{state}");
            assert_eq!(answer["state"], "started", "{state}");
            assert_eq!(answer["status"]["MainPID"], "100", "{state}");
            assert_eq!(
                fake.calls(),
                [
                    "show volant-golden".to_string(),
                    format!("{action} volant-golden")
                ]
            );
        }
    }
}

/// Each state acts only when the unit differs; `enabled` follows `is-enabled`, init scripts included.
#[test]
fn each_state_acts_only_when_the_unit_differs_and_enabled_follows_is_enabled() {
    {
        for (active, state, action) in [
            ("inactive", "started", Some("start")),
            ("active", "stopped", Some("stop")),
            ("deactivating", "stopped", Some("stop")),
            ("inactive", "stopped", None),
            ("activating", "started", None),
            ("inactive", "restarted", Some("start")),
            ("failed", "reloaded", Some("start")),
        ] {
            let fake = Fake::new(&format!(
                r#"[ "$1" = show ] && printf 'LoadState=loaded\nActiveState={active}\n'"#
            ));
            let answer = fake.answer(json!({"name": "u", "state": state})).unwrap();
            assert_eq!(answer["changed"], action.is_some(), "{active} {state}");
            let mut calls = vec!["show u".to_string()];
            calls.extend(action.map(|action| format!("{action} u")));
            assert_eq!(fake.calls(), calls, "{active} {state}");
        }
    }
    {
        for (said, rc, wanted, action) in [
            ("enabled", 0, false, Some("disable")),
            ("static", 0, true, None),
            ("enabled-runtime", 0, true, Some("enable")),
            ("indirect", 0, true, Some("enable")),
            ("alias", 0, true, Some("enable")),
            ("disabled", 1, true, Some("enable")),
            ("disabled", 1, false, None),
        ] {
            let fake = Fake::new(&format!(
                r#"case "$1" in show) cat show ;; is-enabled) echo {said}; exit {rc} ;; esac"#
            ));
            let answer = fake
                .answer(json!({"name": "u", "enabled": wanted}))
                .unwrap();
            assert_eq!(answer["changed"], action.is_some(), "{said} {wanted}");
            assert_eq!(answer["enabled"], wanted, "{said} {wanted}");
            let mut calls = vec!["show u".to_string(), "is-enabled u -l".to_string()];
            calls.extend(action.map(|action| format!("{action} u")));
            assert_eq!(fake.calls(), calls, "{said} {wanted}");
        }
        let fake = Fake::new(
            r#"case "$1" in
  show) cat show ;;
  is-enabled) echo disabled; exit 1 ;;
  enable) echo out; echo err >&2; exit 1 ;;
esac"#,
        );
        let answer = fake.answer(json!({"name": "u", "enabled": true})).unwrap();
        assert_eq!(answer["failed"], true);
        assert_eq!(answer["msg"], "Unable to enable service u: out\nerr\n");
        assert!(answer.get("changed").is_none(), "fail_json has no changed");
    }
    {
        let fake = Fake::new(
            r#"case "$1" in show) cat show ;; is-enabled) echo masked-ish; exit 1 ;; esac"#,
        );
        let etc = fake.etc();
        fs::write(format!("{etc}/init.d/u"), "").unwrap();
        let answer = fake.answer(json!({"name": "u", "enabled": true})).unwrap();
        assert_eq!(answer["changed"], true, "no start link: enabled false");
        fs::create_dir_all(format!("{etc}/rc0.d")).unwrap();
        fs::create_dir_all(format!("{etc}/rc2.d")).unwrap();
        fs::write(format!("{etc}/rc2.d/S01u"), "").unwrap();
        let answer = fake.answer(json!({"name": "u", "enabled": true})).unwrap();
        assert_eq!(answer["changed"], false, "a start link: enabled");
        assert!(!sysv_is_enabled(&etc, "v"));
        fs::write(format!("{etc}/rc2.d/S1v"), "").unwrap();
        assert!(!sysv_is_enabled(&etc, "v"), "S? is not S??");
    }
}

/// A load error, a failed command and a systemctl that cannot start fail like the module.
#[test]
fn a_failure_is_named_and_fails_like_the_module() {
    {
        let fake = Fake::new(
            r#"printf 'LoadState=bad-setting\nLoadError=org.freedesktop.systemd1.BadUnitSetting "Unit u.service has a bad unit file setting."\n'"#,
        );
        let answer = fake
            .answer(json!({"name": "u", "state": "started"}))
            .unwrap();
        assert_eq!(
            answer["msg"],
            "Error loading unit file 'u': org.freedesktop.systemd1.BadUnitSetting \"Unit \
             u.service has a bad unit file setting.\""
        );
        assert_eq!(fake.calls(), ["show u"]);
    }
    {
        assert_eq!(
            clean_args(&["/usr/bin/systemctl", "list-unit-files", "u@*"]),
            "/usr/bin/systemctl list-unit-files 'u@*'"
        );
        assert_eq!(
            clean_args(&["systemctl", "is-enabled", "passwd.service", "-l"]),
            "systemctl is-enabled passwd.service '********'"
        );
        assert_eq!(
            clean_args(&["--password=x", "a b", "it's", ""]),
            "'--password=********' 'a b' 'it'\"'\"'s' ''"
        );
        assert_eq!(
            clean_args(&["/opt/\u{e9}/systemctl"]),
            "'/opt/\u{e9}/systemctl'"
        );
    }
    {
        let fake = Fake::new(r#"[ "$1" = daemon-reload ] && rm -f "$0""#);
        let task = json!({"name": "u", "state": "started", "daemon_reload": true});
        let mut answer = fake.answer(task.clone()).unwrap();
        let systemctl = format!("{}/bin/systemctl", fake.0);
        // `exception` as the module sends it (measured, tracebacks off). The chained `OSError`
        // text comes from this libc, so it is checked for its errno and file only.
        let mut exception = answer
            .remove("exception")
            .expect("the module sends an exception");
        let cause = exception["event"]["chain"]["event"]["msg"].take();
        assert_eq!(
            cause,
            format!("[Errno 2] No such file or directory: b'{systemctl}'")
        );
        assert_eq!(
            exception,
            json!({"__ansible_type": "ErrorSummary", "event": {
            "__ansible_type": "Event", "msg": "Error executing command.",
            "chain": {"__ansible_type": "EventChain", "msg_reason": "<<< caused by >>>",
                "traceback_reason":
                    "The above exception was the direct cause of the following error:",
                "follow": true, "event": {"__ansible_type": "Event", "msg": null}}}})
        );
        assert_eq!(
            Value::Object(answer),
            json!({
                "failed": true, "msg": "Error executing command.", "rc": 2,
                "cmd": format!("{systemctl} show u"), "stdout": "", "stderr": "",
                "invocation": invocation(SPEC, &args(task)),
            })
        );
        assert_eq!(fake.calls(), ["daemon-reload"]);
    }
}

/// What the native leaves to the module is handed back first; after a reload the module's branches are followed.
#[test]
#[expect(clippy::too_many_lines)]
fn what_is_left_to_the_module_is_handed_back_and_followed_after_a_reload() {
    {
        for refused in [
            json!({"name": "u", "masked": true}),
            json!({"name": "u", "masked": false, "state": "started"}),
            json!({"name": "u", "state": "started", "scope": "user"}),
            json!({"name": "u", "state": "started", "scope": "nope"}),
            json!({"daemon_reexec": true}),
            json!({"daemon-reexec": true, "daemon_reload": true}),
            json!({"name": "u", "state": "started", "force": true}),
            json!({"name": "u", "state": "started", "no_block": true}),
            json!({"name": "u*", "state": "started"}),
            json!({"name": "u?", "state": "started"}),
            json!({"name": "u[1]", "state": "started"}),
            json!({"name": "u'x", "state": "started"}),
            json!({"name": "$HOME", "state": "started"}),
            json!({"name": "~u", "state": "started"}),
            json!({"name": "/etc/init.d/u", "state": "started"}),
            json!({"name": "", "state": "started"}),
            json!({"name": "u", "state": "running"}),
            json!({"name": "u", "enabled": "yes"}),
            json!({"name": "u", "daemon_reload": "yes"}),
            json!({"name": 1, "state": "started"}),
            json!({"name": "u", "service": "u", "state": "started"}),
            json!({"name": "u", "state": "started", "other": 1}),
            json!({"state": "started"}),
        ] {
            let fake = Fake::new(CRON);
            assert!(
                matches!(
                    fake.answer(refused.clone()),
                    Err(Exit::Stop(Stop::HandBack(_)))
                ),
                "{refused}"
            );
            assert!(fake.calls().is_empty(), "{refused}");
        }
        for (body, init_script) in [
            ("exit 1", false),
            ("echo 'Failed to parse bus message' >&2; exit 1", false),
            (
                r#"[ "$1" = show ] && printf 'LoadState=not-found\nActiveState=inactive\n'"#,
                true,
            ),
        ] {
            let fake = Fake::new(body);
            if init_script {
                fs::write(format!("{}/init.d/u", fake.etc()), "").unwrap();
            }
            assert!(
                matches!(
                    fake.answer(json!({"name": "u", "state": "started"})),
                    Err(Exit::Stop(Stop::HandBack(_)))
                ),
                "{body}"
            );
            assert_eq!(fake.calls(), ["show u"], "{body}");
        }
    }
    {
        let fake = Fake::new(
            r#"case "$1" in
  show) echo 'Id=u@1.service'; echo 'Failed to parse bus message' >&2; exit 1 ;;
  list-unit-files) echo 'u@.service enabled' ;;
  is-active) echo inactive ;;
esac"#,
        );
        let answer = fake
            .answer(json!({"name": "u@1", "state": "started", "daemon_reload": true}))
            .unwrap();
        assert_eq!(
            answer["status"],
            json!({"Id": "u@1.service", "ActiveState": "inactive"})
        );
        assert_eq!(answer["changed"], true);
        assert_eq!(
            fake.calls(),
            [
                "daemon-reload",
                "show u@1",
                "list-unit-files u@*",
                "is-active u@1",
                "start u@1"
            ]
        );

        let fake = Fake::new(
            r#"case "$1" in
  show) exit 1 ;;
  is-enabled) echo bogus ;;
  list-unit-files) exit 1 ;;
  '') echo listed; printf 'broken\034 \n' >&2; exit 3 ;;
esac"#,
        );
        let answer = fake
            .answer(json!({"name": "u", "state": "started", "daemon_reload": true}))
            .unwrap();
        let systemctl = format!("{}/bin/systemctl", fake.0);
        assert_eq!(
            Value::Object(answer),
            json!({
                // `bytes.rstrip()` keeps the `\x1c` `str.strip()` would take.
                "failed": true, "msg": "broken\u{1c}", "cmd": systemctl, "rc": 3,
                "stdout": "listed\n", "stderr": "broken\u{1c} \n",
                "invocation": invocation(SPEC, &args(
                    json!({"name": "u", "state": "started", "daemon_reload": true})
                )),
            })
        );

        let fake =
            Fake::new(r#"[ "$1" = show ] && printf 'LoadState=not-found\nActiveState=inactive\n'"#);
        fs::write(format!("{}/init.d/u", fake.etc()), "").unwrap();
        let answer = fake
            .answer(json!({"name": "u", "state": "started", "daemon_reload": true}))
            .unwrap();
        assert_eq!(
            answer["warnings"],
            json!([
                "The service (u) is actually an init script but the system is managed by systemd"
            ])
        );
        assert_eq!(fake.calls(), ["daemon-reload", "show u", "start u"]);

        let fake = Fake::new(r#"[ "$1" = show ] && echo LoadState=loaded"#);
        let answer = fake
            .answer(json!({"name": "u", "state": "started", "daemon_reload": true}))
            .unwrap();
        assert_eq!(answer["msg"], "Service is in unknown state");
        assert_eq!(answer["status"], json!({"LoadState": "loaded"}));

        let fake = Fake::new("echo nope >&2; exit 4");
        let answer = fake.answer(json!({"daemon_reload": true})).unwrap();
        assert_eq!(answer["msg"], "failure 4 during daemon-reload: nope\n");
    }
}

/// The environment is the native's, a status-only task reads the unit, aliases are kept in the invocation.
#[test]
fn the_environment_status_and_aliases_follow_the_module() {
    {
        let fake = Fake::new(r#"echo "xdg=$XDG_RUNTIME_DIR" >> calls; cat show"#);
        for (key, value) in [("SYSTEMD_OFFLINE", "1"), ("debian_chroot", "x")] {
            let mut context = fake.context();
            context.environment.insert(key.into(), value.into());
            assert!(matches!(
                answer(
                    &args(json!({"name": "u", "state": "started"})),
                    &context,
                    unbounded(),
                    &fake.etc()
                ),
                Err(Exit::Stop(Stop::HandBack(_)))
            ));
        }
        assert!(fake.calls().is_empty());
        let mut context = fake.context();
        context
            .environment
            .insert("XDG_RUNTIME_DIR".into(), "/run/user/4242".into());
        answer(
            &args(json!({"name": "u", "state": "started"})),
            &context,
            unbounded(),
            &fake.etc(),
        )
        .unwrap();
        assert_eq!(fake.calls(), ["show u", "xdg=/run/user/4242"]);
    }
    {
        assert_eq!(xdg_default(|_| None, 42), Some("/run/user/42".into()));
        assert_eq!(xdg_default(|_| Some(String::new()), 42), None);
        assert_eq!(xdg_default(|_| Some("/run/user/7".into()), 42), None);
    }
    {
        let fake = Fake::new(CRON);
        let answer = fake.answer(json!({"name": "cron"})).unwrap();
        assert_eq!(answer["changed"], false);
        assert_eq!(answer["name"], "cron");
        assert!(answer.get("state").is_none() && answer.get("enabled").is_none());
        assert_eq!(
            answer["status"],
            Value::Object(parse_systemctl_show(SHOW_CRON))
        );
        assert_eq!(fake.calls(), ["show cron"]);

        let fake = Fake::new(CRON);
        let answer = fake.answer(json!({})).unwrap();
        assert_eq!(
            Value::Object(answer),
            json!({"changed": false, "name": null, "status": {},
               "invocation": invocation(SPEC, &Map::new())})
        );
        assert!(fake.calls().is_empty());
    }
    {
        for alias in ["unit", "service"] {
            let fake = Fake::new(CRON);
            let task = format!(r#"{{"{alias}": "cron", "enabled": true}}"#);
            let answer = fake.answer(serde_json::from_str(&task).unwrap()).unwrap();
            let module_args = &answer["invocation"]["module_args"];
            assert_eq!(module_args[alias], "cron");
            assert_eq!(module_args["name"], "cron");
            assert_eq!(answer["name"], "cron");
            assert_eq!(fake.calls(), ["show cron", "is-enabled cron -l"]);
        }
        let fake = Fake::new("");
        let answer = fake.answer(json!({"daemon-reload": true})).unwrap();
        let module_args = &answer["invocation"]["module_args"];
        assert_eq!(module_args["daemon-reload"], true);
        assert_eq!(module_args["daemon_reload"], true);
        assert_eq!(fake.calls(), ["daemon-reload"]);
    }
}

/// A `systemctl` that hangs (a D-Bus that does not answer) ends at the task's `timeout`, and
/// at the controller's cancel, and is killed either way.
#[test]
fn a_hung_systemctl_ends_with_the_timeout_and_the_cancel() {
    let fake = Fake::new("exec sleep 600");
    let mut context = fake.context();
    context.timeout = Some(Duration::from_secs(1));
    let task = args(json!({"name": "u", "state": "started"}));
    let (task1, context1) = (task.clone(), context.clone());
    let run = within(30, move || (NATIVE.run)(&task1, &context1, &|| false));
    let NativeRun::Done(result) = run else {
        panic!("a timeout is an answer")
    };
    assert_eq!(result, TaskResult::timed_out(1));
    context.timeout = None;
    let run = within(30, move || (NATIVE.run)(&task, &context, &|| true));
    assert!(matches!(run, NativeRun::Cancelled));
}
