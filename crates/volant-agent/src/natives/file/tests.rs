// SPDX-License-Identifier: GPL-3.0-or-later

use std::os::unix::fs::PermissionsExt;

use serde_json::{Value, json};

use super::super::common::golden::{Scratch, differences, within};
use super::*;

fn ask(args: &Value) -> NativeRun {
    run(args.as_object().unwrap(), &Context::default(), &|| false)
}

fn chmod(path: &str, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn mkdir(path: &str, mode: u32) {
    fs::create_dir_all(path).unwrap();
    chmod(path, mode);
}

/// What the golden test reads back after a case: `{exists, mode, type, content}`.
fn after(path: &str) -> Value {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return json!({"exists": false});
    };
    let kind = if meta.file_type().is_symlink() {
        "link"
    } else if meta.is_dir() {
        "directory"
    } else {
        "file"
    };
    let mode = format!("{:04o}", meta.mode() & 0o7777);
    let mut after = json!({"exists": true, "mode": mode, "type": kind});
    if kind == "file" {
        after["content"] = fs::read_to_string(path).unwrap().into();
    }
    after
}

/// Every recorded `file` case the native answers, each on the state the golden play left before it.
#[test]
fn file_answers_every_recorded_case_like_the_reference() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped as root: file-chown-denied needs a chown to be refused");
        return;
    }
    type Setup = fn(&Scratch);
    let cases: [(&str, &str, Value, Setup); 12] = [
        (
            "file-absent-missing",
            include_str!("../../../../volant/tests/golden/module/file-absent-missing.json"),
            json!({"path": "<golden-tmp>/gone", "state": "absent"}),
            |_| {},
        ),
        (
            "file-absent-present",
            include_str!("../../../../volant/tests/golden/module/file-absent-present.json"),
            json!({"path": "<golden-tmp>/d0", "state": "absent"}),
            |s| {
                mkdir(&s.path("d0/inner"), 0o755);
                fs::write(s.path("d0/inner/x"), "x").unwrap();
            },
        ),
        (
            "file-chown-denied",
            include_str!("../../../../volant/tests/golden/module/file-chown-denied.json"),
            json!({"owner": "root", "path": "<golden-tmp>/f.txt"}),
            |s| chmod(&s.path("f.txt"), 0o600),
        ),
        (
            "file-dest-alias",
            include_str!("../../../../volant/tests/golden/module/file-dest-alias.json"),
            json!({"dest": "<golden-tmp>/a/b", "state": "directory"}),
            |s| mkdir(&s.path("a/b"), 0o750),
        ),
        (
            "file-dir-created",
            include_str!("../../../../volant/tests/golden/module/file-dir-created.json"),
            json!({"mode": "0755", "path": "<golden-tmp>/a/b", "state": "directory"}),
            |_| {},
        ),
        (
            "file-dir-over-file",
            include_str!("../../../../volant/tests/golden/module/file-dir-over-file.json"),
            json!({"path": "<golden-tmp>/f.txt", "state": "directory"}),
            |s| chmod(&s.path("f.txt"), 0o600),
        ),
        (
            "file-dir-same",
            include_str!("../../../../volant/tests/golden/module/file-dir-same.json"),
            json!({"mode": 493, "path": "<golden-tmp>/a/b", "state": "directory"}),
            |s| mkdir(&s.path("a/b"), 0o755),
        ),
        (
            "file-dir-symbolic",
            include_str!("../../../../volant/tests/golden/module/file-dir-symbolic.json"),
            json!({"mode": "u=rwx,g=rx,o=", "path": "<golden-tmp>/a/b", "state": "directory"}),
            |s| mkdir(&s.path("a/b"), 0o755),
        ),
        (
            "file-owner-unknown",
            include_str!("../../../../volant/tests/golden/module/file-owner-unknown.json"),
            json!({"owner": "volant-no-such-user", "path": "<golden-tmp>/f.txt"}),
            |s| chmod(&s.path("f.txt"), 0o600),
        ),
        (
            "file-state-file-missing",
            include_str!("../../../../volant/tests/golden/module/file-state-file-missing.json"),
            json!({"path": "<golden-tmp>/missing", "state": "file"}),
            |_| {},
        ),
        (
            "file-state-file-mode",
            include_str!("../../../../volant/tests/golden/module/file-state-file-mode.json"),
            json!({"mode": "0600", "path": "<golden-tmp>/f.txt", "state": "file"}),
            |_| {},
        ),
        (
            "file-state-file-same",
            include_str!("../../../../volant/tests/golden/module/file-state-file-same.json"),
            json!({"path": "<golden-tmp>/f.txt", "state": "file"}),
            |_| {},
        ),
    ];
    let mut found = Vec::new();
    for (case, recording, args, setup) in cases {
        let scratch = Scratch::new(case);
        scratch.fixture();
        setup(&scratch);
        let args: Value =
            serde_json::from_str(&args.to_string().replace("<golden-tmp>", &scratch.0)).unwrap();
        let path = args["path"].as_str().or(args["dest"].as_str()).unwrap();
        match ask(&args) {
            NativeRun::Done(result) => found.extend(
                differences(recording, result.0, &scratch, &[])
                    .into_iter()
                    .map(|d| format!("{case}: {d}")),
            ),
            NativeRun::Fallback(why) => found.push(format!("{case}: handed back, {why}")),
            NativeRun::Cancelled => found.push(format!("{case}: cancelled")),
        }
        let want: Value = serde_json::from_str(recording).unwrap();
        if after(path) != want["_after"] {
            found.push(format!(
                "{case}: left {}, the reference {}",
                after(path),
                want["_after"]
            ));
        }
    }
    assert!(found.is_empty(), "{found:#?}");
}

/// Outside the subset the task goes to the Python module and the disk is as it was.
#[test]
fn file_hands_back_before_changing_anything() {
    let scratch = Scratch::new("file-back");
    let file = scratch.fixture();
    mkdir(&scratch.path("a/b"), 0o700);
    let dir = scratch.path("a");
    let link = scratch.path("l2");
    let before = || {
        [
            after(&file),
            after(&dir),
            after(&scratch.path("a/b")),
            after(&link),
        ]
    };
    let snapshot = before();
    let mut environment = Context::default();
    environment
        .environment
        .insert("PATH".into(), "/nowhere".into());
    let plain = Context::default;
    for (args, context) in [
        (json!({"path": link, "src": file, "state": "link"}), plain()),
        (
            json!({"path": dir, "mode": "0755", "recurse": true, "state": "directory"}),
            plain(),
        ),
        (json!({"path": scratch.path("l"), "mode": "0600"}), plain()),
        (json!({"path": file, "modification_time": "now"}), plain()),
        (json!({"path": file, "attributes": "+i"}), plain()),
        (json!({"path": file, "mode": "0600"}), environment),
        (json!({"path": file, "mode": "0o600"}), plain()),
        (json!({"path": file, "state": "touch"}), plain()),
        (
            json!({"path": dir, "mode": "0755", "state": "directory", "follow": "no"}),
            plain(),
        ),
    ] {
        let answer = run(args.as_object().unwrap(), &context, &|| false);
        assert!(matches!(answer, NativeRun::Fallback(_)), "{args}");
        assert_eq!(
            before(),
            snapshot,
            "{args} changed the disk before handing back"
        );
    }
}

/// A tree the agent cannot empty is handed back whole, and a link out of it is removed, not followed.
#[test]
fn a_tree_is_removed_whole_or_handed_back_whole() {
    {
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped as root: root can empty any directory");
            return;
        }
        let scratch = Scratch::new("file-tree");
        mkdir(&scratch.path("t/sub"), 0o755);
        fs::write(scratch.path("t/sub/x"), "x").unwrap();
        fs::write(scratch.path("t/y"), "y").unwrap();
        chmod(&scratch.path("t/sub"), 0o500);
        let answer = ask(&json!({"path": scratch.path("t"), "state": "absent"}));
        let left = after(&scratch.path("t/y"));
        chmod(&scratch.path("t/sub"), 0o755);
        assert!(matches!(answer, NativeRun::Fallback(_)));
        assert_eq!(left["exists"], true);
    }
    {
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped as root: the sticky case needs another account than the agent's");
            return;
        }
        let sudo = |args: &[&str]| {
            std::process::Command::new("sudo")
                .arg("-n")
                .args(args)
                .status()
                .is_ok_and(|status| status.success())
        };
        let scratch = Scratch::new("file-rmtree");
        mkdir(&scratch.path("a"), 0o755);
        fs::write(scratch.path("a/y"), "y").unwrap();
        fs::write(scratch.path("a/log"), "log").unwrap();
        mkdir(&scratch.path("s"), 0o755);
        fs::write(scratch.path("s/y"), "y").unwrap();
        mkdir(&scratch.path("m/mnt"), 0o755);
        fs::write(scratch.path("m/y"), "y").unwrap();
        let (log, sticky, mnt) = (
            scratch.path("a/log"),
            scratch.path("s/t"),
            scratch.path("m/mnt"),
        );
        let clean = || {
            sudo(&["chattr", "-a", &log]);
            sudo(&["rm", "-rf", &sticky]);
            sudo(&["umount", &mnt]);
        };
        if !sudo(&["chattr", "+a", &log])
            || !sudo(&["mkdir", "-m", "1777", &sticky])
            || !sudo(&["touch", &format!("{sticky}/theirs")])
            || !sudo(&["mount", "-t", "tmpfs", "volant-test", &mnt])
        {
            clean();
            eprintln!(
                "skipped: sudo -n cannot set up an append-only file, a sticky directory and a mount"
            );
            return;
        }
        let answers =
            ["a", "s", "m"].map(|dir| ask(&json!({"path": scratch.path(dir), "state": "absent"})));
        let left = ["a/y", "s/y", "m/y"].map(|file| after(&scratch.path(file))["exists"].clone());
        clean();
        for (answer, case) in answers.iter().zip(["append-only", "sticky", "mount"]) {
            assert!(matches!(answer, NativeRun::Fallback(_)), "{case}");
        }
        assert_eq!(left, [true, true, true].map(Value::from));
    }
    {
        let scratch = Scratch::new("file-link-out");
        mkdir(&scratch.path("outside"), 0o755);
        fs::write(scratch.path("outside/keep"), "keep").unwrap();
        mkdir(&scratch.path("t/d"), 0o755);
        fs::write(scratch.path("t/d/x"), "x").unwrap();
        std::os::unix::fs::symlink(scratch.path("outside"), scratch.path("t/sub")).unwrap();
        let answer = ask(&json!({"path": scratch.path("t"), "state": "absent"}));
        assert!(matches!(answer, NativeRun::Done(_)));
        assert_eq!(after(&scratch.path("t"))["exists"], false);
        assert_eq!(after(&scratch.path("outside/keep"))["content"], "keep");
    }
}

/// The scan before a removal stops at the cancel; a hung name service ends with the timeout.
#[test]
fn the_scan_and_the_name_service_stop_at_the_cancel_or_timeout() {
    {
        let scratch = Scratch::new("file-scan-cancel");
        mkdir(&scratch.path("flat"), 0o755);
        fs::write(scratch.path("flat/x"), "x").unwrap();
        fs::write(scratch.path("flat/y"), "y").unwrap();
        let polls = std::cell::Cell::new(0);
        let second_poll = || {
            polls.set(polls.get() + 1);
            polls.get() > 1
        };
        let clock = Clock {
            deadline: None,
            cancelled: &second_poll,
        };
        let top = scratch.path("flat");
        assert!(matches!(removable(&top, clock), Err(Halt::Cancelled)));
    }
    {
        let scratch = Scratch::new("file-hung");
        let file = scratch.fixture();
        let _path = super::super::common::golden::fake_getent(&scratch, "exec sleep 600");
        let context = Context {
            timeout: Some(std::time::Duration::from_secs(1)),
            ..Context::default()
        };
        let args = json!({"path": file, "owner": "volant-no-such-user", "mode": "0600"});
        let timed = args.clone();
        let answer = within(30, move || {
            run(timed.as_object().unwrap(), &context, &|| false)
        });
        let NativeRun::Done(result) = answer else {
            panic!("no answer")
        };
        assert_eq!(result, volant_protocol::TaskResult::timed_out(1));
        assert_eq!(after(&file)["mode"], "0644");
        let cancelled = within(30, move || {
            run(args.as_object().unwrap(), &Context::default(), &|| true)
        });
        assert!(matches!(cancelled, NativeRun::Cancelled));
    }
}

/// `_original_basename`, as `copy` sends it: a directory named as the destination stands for
/// the file of that name inside it, in the answer and in `invocation`.
#[test]
fn a_directory_given_with_a_basename_stands_for_the_file_inside() {
    let scratch = Scratch::new("file-basename");
    let file = scratch.fixture();
    let args = json!({"dest": scratch.0, "_original_basename": "f.txt", "state": "file",
                      "mode": "0600", "recurse": false});
    let NativeRun::Done(result) = ask(&args) else {
        panic!("handed back")
    };
    assert_eq!(result.0["path"], file.as_str());
    assert_eq!(result.0["changed"], true);
    assert_eq!(result.0["invocation"]["module_args"]["path"], file.as_str());
    assert_eq!(
        result.0["invocation"]["module_args"]["dest"],
        scratch.0.as_str()
    );
    assert_eq!(after(&file)["mode"], "0600");
    assert_eq!(after(&scratch.0)["mode"], "0755");
}
