// SPDX-License-Identifier: GPL-3.0-or-later

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use regex::Regex;
use serde_json::{Value, json};
use volant_protocol::TaskResult;

use super::super::common::golden::{Scratch, differences, within};
use super::*;

const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../volant/tests/golden/module");

/// The recordings were taken under a `022` umask, which sets the mode of a file created.
/// Nextest runs each test in a process of its own.
fn scratch(name: &str) -> Scratch {
    unsafe { libc::umask(0o022) };
    let scratch = Scratch::new(name);
    fs::create_dir(scratch.path("s")).unwrap();
    scratch
}

/// `content` staged as the agent stages a task's file, in a file of its own, `0600`, and
/// dated long ago, so that only `atomic_move`'s `utime` can give a replaced file the time now.
fn stage(scratch: &Scratch, content: &str) -> String {
    let src = scratch.path("s/src");
    write(&src, content, 0o600);
    set_old_time(&src);
    src
}

fn set_old_time(path: &str) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(978_307_200))
        .unwrap();
}

fn write(path: &str, content: &str, mode: u32) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// A validate naming no program is not quoted; the md5 matches the RFC.
#[test]
fn validate_is_not_quoted_and_md5_matches_the_rfc() {
    {
        let scratch = scratch("copy-no-program");
        let src = stage(&scratch, "v\n");
        let validate = "'secret %s";
        let args = json!({"src": src, "dest": scratch.path("v.txt"), "validate": validate});
        let cancelled = || false;
        let clock = clock(&Context::default(), &cancelled);
        let params = module_args(SPEC, args.as_object().unwrap());
        let request = request(&params, clock).ok().unwrap();
        let Err(Stop::Fail(fail)) = validate_src(&request, validate, clock) else {
            panic!("the validation did not fail");
        };
        assert_eq!(fail["msg"], "validate names no program");
    }
    {
        let md5 = |text: &str| {
            let mut md5 = Md5::new();
            md5.update(text.as_bytes());
            md5.hex()
        };
        for (text, sum) in [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                "abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
            (
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
            ("one\n", "5bbf5a52328e7439ae6e719dfe712200"),
        ] {
            assert_eq!(md5(text), sum, "{text:?}");
        }
    }
}

fn ask(args: &Value) -> NativeRun {
    ask_in(args, &Context::default())
}

fn ask_in(args: &Value, context: &Context) -> NativeRun {
    run(args.as_object().unwrap(), context, &|| false)
}

fn sha1(content: &str) -> String {
    let mut sha1 = sha1_smol::Sha1::new();
    sha1.update(content.as_bytes());
    sha1.digest().to_string()
}

/// What the golden test reads back after a case: `{exists, mode, type, content}`, and the
/// backup's content when there is one.
fn after(path: &str, backup: Option<&str>) -> Value {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return json!({"exists": false});
    };
    let kind = if meta.is_dir() { "directory" } else { "file" };
    let mode = format!("{:04o}", meta.mode() & 0o7777);
    let mut after = json!({"exists": true, "mode": mode, "type": kind});
    if kind == "file" {
        after["content"] = fs::read_to_string(path).unwrap().into();
    }
    if let Some(backup) = backup {
        after["backup_content"] = fs::read_to_string(backup).unwrap().into();
    }
    after
}

/// Every path under the scratch directory, its mode, and what a file holds.
fn snapshot(scratch: &Scratch) -> Vec<(String, u32, Vec<u8>)> {
    let mut found = Vec::new();
    let mut dirs = vec![scratch.0.clone()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path().to_string_lossy().into_owned();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                dirs.push(path.clone());
            }
            let content = if meta.is_file() {
                fs::read(&path).unwrap()
            } else {
                Vec::new()
            };
            found.push((path, meta.mode(), content));
        }
    }
    found.sort();
    found
}

/// Checks `ours` against `want` key by key, a backup's name against the reference's pattern
/// first, and pushes what differs to `found`. Returns the backup's name.
fn compare(
    case: &str,
    want: &str,
    mut ours: Map<String, Value>,
    scratch: &Scratch,
    volatile: &[&str],
    found: &mut Vec<String>,
) -> Option<String> {
    let recorded: Value = serde_json::from_str(want).unwrap();
    let backup = ours
        .get("backup_file")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(name) = &backup {
        let masked = name.replace(&scratch.0, "<golden-tmp>");
        let dest = recorded["dest"].as_str().unwrap_or_default();
        let pattern = format!(
            r"^{}\.\d+\.\d{{4}}-\d{{2}}-\d{{2}}@\d{{2}}:\d{{2}}:\d{{2}}~$",
            regex::escape(dest)
        );
        if !Regex::new(&pattern).unwrap().is_match(&masked) {
            found.push(format!("{case}: backup {masked} is not named {pattern}"));
        }
        ours.insert("backup_file".into(), recorded["backup_file"].clone());
    }
    found.extend(
        differences(want, ours, scratch, volatile)
            .into_iter()
            .map(|d| format!("{case}: {d}")),
    );
    backup
}

/// Every recorded `copy-module-*` case, played in the golden play's order on the same files.
#[test]
fn copy_answers_every_recorded_case_like_the_reference() {
    let scratch = scratch("copy");
    scratch.fixture();
    let index: Value =
        serde_json::from_str(&fs::read_to_string(format!("{GOLDEN}/index.json")).unwrap()).unwrap();
    let mut found = Vec::new();
    for case in [
        "copy-module-created",
        "copy-module-modified-backup",
        "copy-module-validate-fail",
        "copy-module-no-dir",
        "copy-module-remote-src",
        "copy-module-checksum",
        "copy-module-dest-link",
    ] {
        let entry = &index[case];
        let recording = fs::read_to_string(format!("{GOLDEN}/{case}.json")).unwrap();
        let want: Value = serde_json::from_str(&recording).unwrap();
        let mut args: Value = serde_json::from_str(
            &entry["args"]
                .to_string()
                .replace("<golden-tmp>", &scratch.0),
        )
        .unwrap();
        let dest = args["dest"].as_str().unwrap().to_string();
        let mut staged = None;
        if let Some(content) = args.as_object_mut().unwrap().remove("content") {
            let content = content.as_str().unwrap().to_string();
            let src = stage(&scratch, &content);
            args["src"] = src.clone().into();
            // The plugin's sum only where the task gave none.
            if args.get("checksum").is_none() {
                args["checksum"] = sha1(&content).into();
            }
            args["_original_basename"] = ".staged".into();
            args["follow"] = false.into();
            staged = Some((src.clone(), fs::metadata(&src).unwrap().ino(), content));
        }
        let before = snapshot(&scratch);
        let answer = ask(&args);
        if entry["expect"] == "fallback" {
            if !matches!(answer, NativeRun::Fallback(_)) {
                found.push(format!("{case}: answered outside the subset"));
            }
            if snapshot(&scratch) != before {
                found.push(format!("{case}: changed something before handing back"));
            }
            continue;
        }
        let NativeRun::Done(result) = answer else {
            found.push(format!("{case}: handed back or cancelled"));
            continue;
        };
        let (src, ino, content) = staged.unwrap();
        let mut ours = result.0;
        ours.entry("diff").or_insert_with(|| json!([]));
        ours.entry("checksum")
            .or_insert_with(|| sha1(&content).into());
        for key in ["stdout", "stderr"] {
            if let Some(text) = ours.get(key).and_then(Value::as_str) {
                let lines: Vec<Value> = text.lines().map(Value::from).collect();
                ours.insert(format!("{key}_lines"), lines.into());
            }
        }
        if ours.get("src") == Some(&json!(src)) {
            ours.insert("src".into(), "<golden-staged-path>".into());
        }
        ours["invocation"]["module_args"]["src"] = "<golden-staged-path>".into();
        if want["changed"] == true
            && (Path::new(&src).exists() || fs::metadata(&dest).unwrap().ino() != ino)
        {
            found.push(format!("{case}: the staged source was copied, not moved"));
        }
        let volatile: Vec<&str> = entry["volatile"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let backup = compare(case, &recording, ours, &scratch, &volatile, &mut found);
        let left = after(&dest, backup.as_deref());
        if left != want["_after"] {
            found.push(format!(
                "{case}: left {left}, the reference {}",
                want["_after"]
            ));
        }
    }
    assert!(found.is_empty(), "{found:#?}");
}

/// The branches the golden play does not reach, each against what the reference's module answered.
#[test]
fn copy_matches_the_reference_on_every_other_branch() {
    let scratch = scratch("copy-branches");
    let t = scratch.0.clone();
    let mut found = Vec::new();
    let mut case = |name: &str, content: &str, args: Value, want: &str| {
        let src = stage(&scratch, content);
        let mut args: Value = serde_json::from_str(&args.to_string().replace("<t>", &t)).unwrap();
        args["src"] = src.into();
        let NativeRun::Done(result) = ask(&args) else {
            found.push(format!("{name}: handed back or cancelled"));
            return None;
        };
        let mut ours = result.0;
        ours.remove("invocation");
        compare(name, want, ours, &scratch, &[], &mut found)
    };
    let path = |name: &str| format!("{t}/{name}");

    write(&path("tgt"), "target\n", 0o640);
    std::os::unix::fs::symlink("tgt", path("lnk")).unwrap();
    case(
        "dest-link",
        "one\n",
        json!({"dest": "<t>/lnk", "checksum": "c7059bb19433cc3cabaa6236c83d56668a843dd2", "_original_basename": "x"}),
        r#"{"dest": "<golden-tmp>/lnk", "src": "<golden-tmp>/s/src", "md5sum": "5bbf5a52328e7439ae6e719dfe712200", "checksum": "c7059bb19433cc3cabaa6236c83d56668a843dd2", "changed": true, "uid": "<uid>", "gid": "<gid>", "owner": "<user>", "group": "<group>", "mode": "0644", "state": "file", "size": 4}"#,
    );
    let link_left = (after(&path("lnk"), None), after(&path("tgt"), None));

    std::os::unix::fs::symlink("tgt", path("lnk2")).unwrap();
    let backup = case(
        "dest-link-follow",
        "two\n",
        json!({"dest": "<t>/lnk2", "follow": true, "backup": true}),
        r#"{"dest": "<golden-tmp>/tgt", "src": "<golden-tmp>/s/src", "md5sum": "c193497a1a06b2c72230e6146ff47080", "checksum": "7bbef45b3bc70855010e02460717643125c3beca", "changed": true, "backup_file": "<golden-tmp>/tgt.347163.2026-09-27@14:25:29~", "uid": "<uid>", "gid": "<gid>", "owner": "<user>", "group": "<group>", "mode": "0640", "state": "file", "size": 4}"#,
    );
    let follow_left = (
        is_link(&path("lnk2")),
        after(&path("tgt"), backup.as_deref()),
    );

    std::os::unix::fs::symlink("nowhere", path("dangling")).unwrap();
    case(
        "dest-dangling",
        "d\n",
        json!({"dest": "<t>/dangling"}),
        r#"{"dest": "<golden-tmp>/dangling", "src": "<golden-tmp>/s/src", "md5sum": "e29311f6f1bf1af907f9ef9f44b8328b", "checksum": "e983f374794de9c64e3d1c1de1d490c0756eeeff", "changed": true, "uid": "<uid>", "gid": "<gid>", "owner": "<user>", "group": "<group>", "mode": "0644", "state": "file", "size": 2}"#,
    );
    let dangling_left = after(&path("dangling"), None);

    write(&path("same.txt"), "same\n", 0o644);
    case(
        "same-mode",
        "same\n",
        json!({"dest": "<t>/same.txt", "mode": "0600"}),
        r#"{"dest": "<golden-tmp>/same.txt", "src": "<golden-tmp>/s/src", "md5sum": "847676261680bff61c72961c8198abc0", "checksum": "2c985b161217a952b7a410fd91495cebc349f520", "changed": true, "uid": "<uid>", "gid": "<gid>", "owner": "<user>", "group": "<group>", "mode": "0600", "state": "file", "size": 5}"#,
    );
    let same_kept_source = Path::new(&path("s/src")).exists();

    case(
        "force-false",
        "f\n",
        json!({"dest": "<t>/same.txt", "force": false}),
        r#"{"msg": "file already exists", "src": "<golden-tmp>/s/src", "dest": "<golden-tmp>/same.txt", "changed": false, "uid": "<uid>", "gid": "<gid>", "owner": "<user>", "group": "<group>", "mode": "0600", "state": "file", "size": 5}"#,
    );

    fs::create_dir(path("d")).unwrap();
    case(
        "dest-dir",
        "in dir\n",
        json!({"dest": "<t>/d", "_original_basename": "b.txt"}),
        r#"{"dest": "<golden-tmp>/d/b.txt", "src": "<golden-tmp>/s/src", "md5sum": "739df3538be9245dcb11f80816b3b4bb", "checksum": "ff11801174e1238a93abfc875b818b855464be4e", "changed": true, "uid": "<uid>", "gid": "<gid>", "owner": "<user>", "group": "<group>", "mode": "0644", "state": "file", "size": 7}"#,
    );
    case(
        "dest-dir-slash",
        "in dir2\n",
        json!({"dest": "<t>/d/", "_original_basename": "b2.txt"}),
        r#"{"dest": "<golden-tmp>/d/b2.txt", "src": "<golden-tmp>/s/src", "md5sum": "71b9d43c97402befed6f21701bf78e38", "checksum": "6cba982828e45e03128be9d634ca392be0a9aa9c", "changed": true, "uid": "<uid>", "gid": "<gid>", "owner": "<user>", "group": "<group>", "mode": "0644", "state": "file", "size": 8}"#,
    );

    case(
        "owner-unknown",
        "o\n",
        json!({"dest": "<t>/o.txt", "owner": "volant-no-such-user"}),
        r#"{"changed": false, "path": "<golden-tmp>/o.txt", "failed": true, "msg": "chown failed: failed to look up user volant-no-such-user", "uid": "<uid>", "gid": "<gid>", "owner": "<user>", "group": "<group>", "mode": "0644", "state": "file", "size": 2}"#,
    );
    let owner_left = after(&path("o.txt"), None);

    case(
        "validate-no-pct",
        "v\n",
        json!({"dest": "<t>/v.txt", "validate": "true"}),
        r#"{"changed": false, "failed": true, "msg": "validate must contain %s: true"}"#,
    );
    let validate_left = after(&path("v.txt"), None);

    write(&path("e.txt"), "old\n", 0o604);
    set_old_time(&path("e.txt"));
    let started = SystemTime::now() - Duration::from_secs(1);
    case(
        "existing",
        "e\n",
        json!({"dest": "<t>/e.txt"}),
        r#"{"dest": "<golden-tmp>/e.txt", "src": "<golden-tmp>/s/src", "md5sum": "9ffbf43126e33be52cd2bf7e01d627f9", "checksum": "094e3afb2fe8dfe82f63731cdcd3b999f4856cff", "changed": true, "uid": "<uid>", "gid": "<gid>", "owner": "<user>", "group": "<group>", "mode": "0604", "state": "file", "size": 2}"#,
    );
    let replaced_time = fs::metadata(path("e.txt")).unwrap().modified().unwrap();

    case(
        "mode-invalid",
        "m\n",
        json!({"dest": "<t>/m.txt", "mode": "u=zz"}),
        r#"{"changed": false, "path": "<golden-tmp>/m.txt", "details": "bad symbolic permission for mode: u=zz", "failed": true, "msg": "mode must be in octal or symbolic form", "uid": "<uid>", "gid": "<gid>", "owner": "<user>", "group": "<group>", "mode": "0644", "state": "file", "size": 2}"#,
    );
    let mode_left = after(&path("m.txt"), None);

    fs::create_dir(path("ro")).unwrap();
    fs::set_permissions(path("ro"), fs::Permissions::from_mode(0o555)).unwrap();
    case(
        "not-writable",
        "x\n",
        json!({"dest": "<t>/ro/x.txt"}),
        r#"{"changed": false, "failed": true, "msg": "Destination <golden-tmp>/ro not writable"}"#,
    );
    fs::create_dir_all(path("nox/in")).unwrap();
    fs::set_permissions(path("nox"), fs::Permissions::from_mode(0o600)).unwrap();
    case(
        "not-accessible",
        "x\n",
        json!({"dest": "<t>/nox/in/x.txt"}),
        r#"{"changed": false, "failed": true, "msg": "Destination directory <golden-tmp>/nox/in is not accessible"}"#,
    );
    fs::set_permissions(path("nox"), fs::Permissions::from_mode(0o755)).unwrap();

    let file = |content: &str, mode: &str| json!({"exists": true, "mode": mode, "type": "file", "content": content});
    let followed = json!({
        "exists": true, "mode": "0640", "type": "file", "content": "two\n",
        "backup_content": "target\n",
    });
    for (name, left, want) in [
        ("dest-link", link_left.0, file("one\n", "0644")),
        ("dest-link target", link_left.1, file("target\n", "0640")),
        ("dest-link-follow link", json!(follow_left.0), json!(true)),
        ("dest-link-follow", follow_left.1, followed),
        ("dest-dangling", dangling_left, file("d\n", "0644")),
        ("same-mode source", json!(same_kept_source), json!(true)),
        ("owner-unknown", owner_left, file("o\n", "0644")),
        ("validate-no-pct", validate_left, json!({"exists": false})),
        ("mode-invalid", mode_left, file("m\n", "0644")),
    ] {
        if left != want {
            found.push(format!("{name}: left {left}, the reference {want}"));
        }
    }
    if replaced_time < started {
        found.push("existing: the replaced file kept its old time".into());
    }
    assert!(found.is_empty(), "{found:#?}");
}

/// Every case outside the subset hands back with every file as it was, the staged source included.
#[test]
fn copy_hands_back_before_changing_anything() {
    let scratch = scratch("copy-back");
    let t = scratch.0.clone();
    let file = scratch.fixture();
    fs::create_dir_all(scratch.path("d/sub.txt")).unwrap();
    // Searchable, so that only its kind tells it from a directory.
    let exe = scratch.path("exe");
    write(&exe, "", 0o755);
    let src = stage(&scratch, "new\n");
    let mut found = Vec::new();
    let mut hand_back = |name: &str, extra: Value, context: &Context| {
        let mut args = json!({"src": src, "dest": format!("{t}/n.txt"), "backup": true});
        for (key, value) in extra.as_object().unwrap() {
            args[key] = value.clone();
        }
        let before = snapshot(&scratch);
        if !matches!(ask_in(&args, context), NativeRun::Fallback(_)) {
            found.push(format!("{name}: answered outside the subset"));
        }
        if snapshot(&scratch) != before {
            found.push(format!("{name}: changed something before handing back"));
        }
    };
    let plain = Context::default();
    for (name, extra) in [
        ("remote_src", json!({"remote_src": true})),
        ("directory_mode", json!({"directory_mode": "0755"})),
        ("content", json!({"content": "x"})),
        ("attributes", json!({"attributes": "+i"})),
        ("attr", json!({"attr": "+i"})),
        ("setype", json!({"setype": "tmp_t"})),
        ("unsafe_writes", json!({"unsafe_writes": true})),
        ("mode preserve", json!({"mode": "preserve"})),
        ("mode 0o", json!({"mode": "0o644"})),
        ("backup yes", json!({"backup": "yes"})),
        ("local_follow yes", json!({"local_follow": "yes"})),
        ("unknown argument", json!({"nope": 1})),
        ("two names", json!({"attributes": null, "attr": null})),
        ("relative dest", json!({"dest": "n.txt"})),
        ("repeated slashes", json!({"dest": format!("{t}//n.txt")})),
        ("no src", json!({"src": null})),
        ("src a directory", json!({"src": scratch.path("d")})),
        (
            "directories to create",
            json!({"dest": format!("{t}/new/"), "_original_basename": "n.txt"}),
        ),
        ("parent a file", json!({"dest": format!("{exe}/x")})),
        (
            "dest a directory",
            json!({"dest": format!("{t}/d"), "_original_basename": "sub.txt"}),
        ),
    ] {
        hand_back(name, extra, &plain);
    }
    let environment = Context {
        environment: [("LANG".to_string(), "C".to_string())].into(),
        ..Context::default()
    };
    hand_back("environment", json!({}), &environment);

    // The cases decided after the backup would be written or the link replaced, played on
    // the fixture file and on the link to it: a hand-back that came after either shows in
    // the snapshot as a backup file or a link turned into a file.
    let link = scratch.path("l");
    for dest in [&file, &link] {
        for (name, validate) in [
            ("validate expands", "test -s $HOME%s"),
            ("validate formats", "test %d %s"),
            ("validate comment", "test -s %s # x"),
            ("validate program missing", "volant-no-such-program %s"),
        ] {
            hand_back(
                &format!("{name}, dest {dest}"),
                json!({"dest": dest, "validate": validate}),
                &plain,
            );
        }
    }

    // `preserved_copy` would give the backup the file's inode flags, through the link too.
    set_flags(&file, NODUMP);
    for dest in [&file, &link] {
        hand_back(
            &format!("nodump flag, dest {dest}"),
            json!({"dest": dest}),
            &plain,
        );
    }
    set_flags(&file, 0);

    let name = CString::new(file.clone()).unwrap();
    // SAFETY: the name and the value outlive the call.
    let set = unsafe {
        libc::setxattr(
            name.as_ptr(),
            c"user.volant".as_ptr(),
            b"1".as_ptr().cast(),
            1,
            0,
        )
    };
    assert_eq!(
        set, 0,
        "this file system takes no user xattrs, which the test needs"
    );
    for dest in [&file, &link] {
        hand_back(
            &format!("xattrs, dest {dest}"),
            json!({"dest": dest}),
            &plain,
        );
    }
    assert!(found.is_empty(), "{found:#?}");
}

/// `FS_NODUMP_FL`, which a file's owner may set.
const NODUMP: libc::c_long = 0x40;

/// `chattr =` the flags in `flags` (and the ones the file system keeps) on `path`.
fn set_flags(path: &str, flags: libc::c_long) {
    let file = fs::File::open(path).unwrap();
    let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
    let mut now: libc::c_long = 0;
    // SAFETY: both ioctls read or write one `long` through the pointer.
    let rc = unsafe {
        libc::ioctl(fd, libc::FS_IOC_GETFLAGS, &raw mut now);
        now = (now & AUTOMATIC) | flags;
        libc::ioctl(fd, libc::FS_IOC_SETFLAGS, &raw const now)
    };
    assert_eq!(
        rc, 0,
        "this file system takes no inode flags, which the test needs"
    );
}

/// A rename across a device falls back, and a source on another mount is handed back.
#[test]
fn a_cross_device_rename_and_a_foreign_mount_are_handled() {
    {
        assert!(!crosses((1, Some(7)), (1, Some(7))));
        assert!(crosses((2, Some(7)), (1, Some(7))), "a subvolume");
        assert!(crosses((1, Some(8)), (1, Some(7))), "a bind mount");
        assert!(crosses((1, None), (1, Some(7))), "an unknown mount");
    }
    {
        let scratch = scratch("copy-mount");
        let other = format!("/dev/shm/volant-copy-{}", std::process::id());
        fs::write(&other, "far\n").expect("/dev/shm is writable, which the test needs");
        let apart = mount_id(&other, false) != mount_id(&scratch.0, true);
        let dest = scratch.path("n.txt");
        write(&dest, "near\n", 0o644);
        let answer = ask(&json!({"src": other, "dest": dest, "backup": true}));
        let kept = fs::read_to_string(&other);
        let _ = fs::remove_file(&other);
        assert!(
            apart,
            "/dev/shm shares /var/tmp's mount here, which the test needs apart"
        );
        assert!(
            matches!(answer, NativeRun::Fallback(_)),
            "answered across mounts"
        );
        assert_eq!(kept.unwrap(), "far\n");
        assert_eq!(fs::read_to_string(&dest).unwrap(), "near\n");
        assert_eq!(
            fs::read_dir(&scratch.0).unwrap().count(),
            2,
            "a backup was made"
        );
    }
}

/// A backup stops at the cancel, and a hung validate ends with the timeout or the cancel.
#[test]
fn a_backup_and_a_hung_validate_stop_at_the_cancel() {
    {
        let scratch = scratch("copy-backup-cancel");
        let big = scratch.path("big");
        write(&big, &"x".repeat(3 * 1024 * 1024), 0o644);
        let cancelled = || true;
        let clock = Clock {
            deadline: None,
            cancelled: &cancelled,
        };
        let copied = super::super::common::backup_copy(&big, &scratch.path("big~"), clock);
        assert!(
            matches!(copied, Err(BackupError::Stopped(Halt::Cancelled))),
            "{copied:?}"
        );
    }
    {
        let scratch = scratch("copy-hung");
        let hang = scratch.path("hang");
        write(&hang, "#!/bin/sh\nexec sleep 600\n", 0o755);
        let args = json!({
            "src": stage(&scratch, "x\n"),
            "dest": scratch.path("n.txt"),
            "validate": format!("{hang} %s"),
        });
        let timed = args.clone();
        let answer = within(30, move || {
            let context = Context {
                timeout: Some(Duration::from_secs(1)),
                ..Context::default()
            };
            match ask_in(&timed, &context) {
                NativeRun::Done(result) => Some(result),
                _ => None,
            }
        });
        assert_eq!(answer, Some(TaskResult::timed_out(1)));

        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            flag.store(true, Ordering::Relaxed);
        });
        let answer = within(30, move || {
            let cancel = move || cancelled.load(Ordering::Relaxed);
            matches!(
                run(args.as_object().unwrap(), &Context::default(), &cancel),
                NativeRun::Cancelled
            )
        });
        assert!(answer, "the cancel did not stop the validate");
    }
}
