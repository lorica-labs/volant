// SPDX-License-Identifier: GPL-3.0-or-later

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use super::super::common::backup_copy;
use super::super::common::golden::{Scratch, differences, within};
use super::*;

const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../volant/tests/golden/module");

/// The recordings were taken under a `022` umask, which sets the mode of the file `create`
/// makes. Nextest runs each test in a process of its own.
fn scratch(name: &str) -> (Scratch, Context) {
    unsafe { libc::umask(0o022) };
    let scratch = Scratch::new(name);
    fs::create_dir(scratch.path(".tmp")).unwrap();
    let context = Context {
        remote_tmp: scratch.path(".tmp"),
        ..Context::default()
    };
    (scratch, context)
}

fn ask(args: &Value, context: &Context) -> NativeRun {
    run(args.as_object().unwrap(), context, &|| false)
}

fn write(path: &str, content: &str, mode: u32) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
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

/// Every file under the scratch directory and what it holds.
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

/// Every recorded `lineinfile` case, played in the golden play's order on the same files.
#[test]
fn lineinfile_answers_every_recorded_case_like_the_reference() {
    let (scratch, context) = scratch("lineinfile");
    let index: Value =
        serde_json::from_str(&fs::read_to_string(format!("{GOLDEN}/index.json")).unwrap()).unwrap();
    let mut found = Vec::new();
    for case in [
        "lineinfile-missing",
        "lineinfile-created",
        "lineinfile-same",
        "lineinfile-appended",
        "lineinfile-replaced-backup",
        "lineinfile-mode",
        "lineinfile-insertafter",
        "lineinfile-search-string",
        "lineinfile-validate-ok",
        "lineinfile-validate-fail",
        "lineinfile-absent",
        "lineinfile-absent-same",
        "lineinfile-dir",
        "lineinfile-lookahead",
        "lineinfile-backrefs",
        "lineinfile-last-match",
        "lineinfile-insertafter-last",
    ] {
        let entry = &index[case];
        let recording = fs::read_to_string(format!("{GOLDEN}/{case}.json")).unwrap();
        let want: Value = serde_json::from_str(&recording).unwrap();
        let args: Value = serde_json::from_str(
            &entry["args"]
                .to_string()
                .replace("<golden-tmp>", &scratch.0),
        )
        .unwrap();
        let path = args["path"].as_str().unwrap();
        if case == "lineinfile-last-match" {
            write(path, "a=1\nx=1\na=2\nx=2\nz=0\n", 0o644);
        }
        let before = after(path, None);
        let answer = ask(&args, &context);
        if entry["expect"] == "fallback" {
            if !matches!(answer, NativeRun::Fallback(_)) {
                found.push(format!("{case}: answered outside the subset"));
            }
            if after(path, None) != before {
                found.push(format!("{case}: changed the file before handing back"));
            }
            let content = want["_after"]["content"].as_str().unwrap();
            let mode = u32::from_str_radix(want["_after"]["mode"].as_str().unwrap(), 8);
            write(path, content, mode.unwrap());
            continue;
        }
        let NativeRun::Done(result) = answer else {
            found.push(format!("{case}: handed back or cancelled"));
            continue;
        };
        let mut ours = result.0;
        let mut backup = None;
        if let Some(pattern) = entry["patterns"]["backup"].as_str() {
            let name = ours["backup"].as_str().unwrap_or_default().to_string();
            let masked = name.replace(&scratch.0, "<golden-tmp>");
            if !Regex::new(pattern).unwrap().is_match(masked.as_bytes()) {
                found.push(format!("{case}: backup {masked} is not named {pattern}"));
            }
            ours.insert("backup".into(), want["backup"].clone());
            backup = Some(name);
        }
        found.extend(
            differences(&recording, ours, &scratch, &[])
                .into_iter()
                .map(|d| format!("{case}: {d}")),
        );
        let left = after(path, backup.as_deref());
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
#[expect(clippy::too_many_lines)]
fn lineinfile_matches_the_reference_on_every_other_branch() {
    type Case = (
        &'static str,
        Option<&'static str>,
        Value,
        Option<&'static str>,
        bool,
        &'static str,
    );
    let cases: Vec<Case> = vec![
        (
            "insertbefore-last",
            Some("a=1\nb=2\na=3\n"),
            json!({"line": "x=0", "insertbefore": "^a="}),
            Some("a=1\nb=2\nx=0\na=3\n"),
            true,
            "line added",
        ),
        (
            "insertbefore-first",
            Some("a=1\nb=2\na=3\n"),
            json!({"line": "x=0", "insertbefore": "^a=", "firstmatch": true}),
            Some("x=0\na=1\nb=2\na=3\n"),
            true,
            "line added",
        ),
        (
            "bof-nomatch",
            Some("a=1\n"),
            json!({"regexp": "^h", "line": "h", "insertbefore": "BOF"}),
            Some("h\na=1\n"),
            true,
            "line added",
        ),
        (
            "insertafter-noeol",
            Some("a=1\nb=2"),
            json!({"line": "c=3", "insertafter": "^b="}),
            Some("a=1\nb=2c=3\n"),
            true,
            "line added",
        ),
        (
            "append-noeol",
            Some("a=1"),
            json!({"line": "b=2"}),
            Some("a=1\nb=2\n"),
            true,
            "line added",
        ),
        (
            "exact-present",
            Some("a=1\nb=2\n"),
            json!({"line": "a=1"}),
            Some("a=1\nb=2\n"),
            false,
            "",
        ),
        (
            "exact-noeol",
            Some("a=1\nb=2"),
            json!({"line": "b=2"}),
            Some("a=1\nb=2\n"),
            true,
            "line replaced",
        ),
        (
            "absent-line",
            Some("a=1\nb=2\na=1\r\n"),
            json!({"line": "a=1", "state": "absent"}),
            Some("b=2\n"),
            true,
            "2 line(s) removed",
        ),
        (
            "absent-search",
            Some("foo bar\nbaz\n"),
            json!({"search_string": "o b", "state": "absent"}),
            Some("baz\n"),
            true,
            "1 line(s) removed",
        ),
        (
            "firstmatch",
            Some("a=1\na=2\n"),
            json!({"regexp": "^a=", "line": "a=9", "firstmatch": true}),
            Some("a=9\na=2\n"),
            true,
            "line replaced",
        ),
        (
            "insertafter-present",
            Some("a=1\nc=4\n"),
            json!({"line": "c=4", "insertafter": "^a="}),
            Some("a=1\nc=4\n"),
            false,
            "",
        ),
        (
            "eof-nomatch",
            Some("a=1\n"),
            json!({"regexp": "^z", "line": "z=1", "insertafter": "EOF"}),
            Some("a=1\nz=1\n"),
            true,
            "line added",
        ),
        (
            "insertbefore-eof-regex",
            Some("EOF here\nx\n"),
            json!({"line": "y", "insertbefore": "EOF"}),
            Some("y\nEOF here\nx\n"),
            true,
            "line added",
        ),
        (
            "insertafter-nomatch",
            Some("a=1\n"),
            json!({"line": "q", "insertafter": "^nomatch"}),
            Some("a=1\nq\n"),
            true,
            "line added",
        ),
        (
            "dollar-crlf",
            Some("k=1\r\n"),
            json!({"regexp": "^k=1$", "line": "k=2"}),
            Some("k=1\r\nk=2\n"),
            true,
            "line added",
        ),
        (
            "insertafter-then-line",
            Some("a=1\nq\n"),
            json!({"line": "q", "insertafter": "^a="}),
            Some("a=1\nq\n"),
            false,
            "",
        ),
        (
            "insertbefore-bof-first",
            Some("x\n"),
            json!({"line": "x", "insertbefore": "^x"}),
            Some("x\n"),
            false,
            "",
        ),
        (
            "absent-missing",
            None,
            json!({"regexp": "x", "state": "absent"}),
            None,
            false,
            "file not present",
        ),
        (
            "create-subdir",
            None,
            json!({"line": "x", "create": true, "path": "sub/dir/n.conf"}),
            Some("x\n"),
            true,
            "line added",
        ),
        (
            "validate-empty",
            Some("a=1\n"),
            json!({"line": "b=2", "validate": ""}),
            Some("a=1\nb=2\n"),
            true,
            "line added",
        ),
    ];
    let mut found = Vec::new();
    for (case, before, mut args, want, changed, msg) in cases {
        let (scratch, context) = scratch(case);
        let path = scratch.path(args["path"].as_str().unwrap_or("f.conf"));
        if let Some(before) = before {
            write(&path, before, 0o644);
        }
        args["path"] = path.clone().into();
        let NativeRun::Done(result) = ask(&args, &context) else {
            found.push(format!("{case}: handed back or cancelled"));
            continue;
        };
        let left = fs::read_to_string(&path).ok();
        // A failure gets its `changed` from the dispatcher.
        let result = crate::modules::module_result(result.0);
        let got = (left.as_deref(), &result.0["changed"], &result.0["msg"]);
        if got != (want, &json!(changed), &json!(msg)) {
            found.push(format!("{case}: {got:?}"));
        }
        if want.is_some() && after(&path, None)["mode"] != "0644" {
            found.push(format!("{case}: mode {}", after(&path, None)["mode"]));
        }
    }
    assert!(found.is_empty(), "{found:#?}");
}

/// A Python-only regex falls back; translation reads patterns as Python reads bytes.
#[test]
fn a_regex_is_translated_as_python_reads_bytes_or_falls_back() {
    {
        let (scratch, context) = scratch("lineinfile-regex");
        let path = scratch.path("f.conf");
        write(&path, "K3S_TOKEN=old\nb=2\n", 0o644);
        let snapshot_before = snapshot(&scratch);
        for pattern in [
            r"^K3S_TOKEN=\s*(?!s3cr3t\s*$)",
            r"^K3S_TOKEN=\s*(?!\s*$)",
            r"(?<=K)3S",
            r"(?P<k>b)=(?P=k)",
            r"b=2\Z",
            r"(b)=\1",
            r"(?i)k3s",
            r"b=2*+",
            r"(?>b)",
            r"[[:alpha:]]",
            // `b`, which Python refuses in a bytes pattern and Rust reads as `b`.
            concat!('\\', "u0062"),
        ] {
            for args in [
                json!({"path": path, "regexp": pattern, "line": "K3S_TOKEN=new"}),
                json!({"path": path, "insertafter": pattern, "line": "c=3"}),
                json!({"path": path, "regexp": pattern, "state": "absent"}),
            ] {
                assert!(
                    matches!(ask(&args, &context), NativeRun::Fallback(_)),
                    "{args} was answered"
                );
                assert_eq!(
                    snapshot(&scratch),
                    snapshot_before,
                    "{args} changed the disk"
                );
            }
        }
    }
    {
        type Lines<'a> = &'a [(&'a [u8], bool)];
        let cases: &[(&str, Lines<'_>)] = &[
            (
                "^a$",
                &[
                    (b"a\n", true),
                    (b"a", true),
                    (b"a\r\n", false),
                    (b"ab\n", false),
                ],
            ),
            ("^$", &[(b"\n", true), (b"x\n", false), (b"", true)]),
            (r"\d", &[("\u{663}\n".as_bytes(), false), (b"7", true)]),
            (r"\w+=", &[("\u{e9}=\n".as_bytes(), false), (b"_x=1", true)]),
            ("[\u{e9}]", &[(b"\xa9", true), (b"e", false)]),
            ("\u{e9}+", &[(b"\xc3\xa9\xa9", true), (b"\xc3", false)]),
            (r"a{,2}b", &[(b"b", true), (b"aab", true)]),
            (r"x{", &[(b"x{", true), (b"x", false)]),
            (r"x{}", &[(b"x{}", true)]),
            (r"x{1,2", &[(b"x{1,2", true)]),
            (r"\<a\>", &[(b"<a>", true), (b"a", false)]),
            (r"[]a]", &[(b"]", true), (b"a", true), (b"b", false)]),
            (r"[^]a]", &[(b"]", false), (b"b", true), (b"\n", true)]),
            (r"[a-]", &[(b"-", true), (b"b", false)]),
            (r"[\b]", &[(b"\x08", true), (b"b", false)]),
            (r"\bfoo\b", &[(b"a foo.", true), (b"afoo", false)]),
            (r"[\x41-\x43]x", &[(b"Bx", true), (b"Dx", false)]),
            (
                r"\s",
                &[
                    (b"\x0b", true),
                    (b"\x1c", false),
                    ("\u{a0}".as_bytes(), false),
                ],
            ),
            (r".", &[(b"\n", false), (b"\xff", true)]),
            (r"a|", &[(b"zzz", true)]),
            (r"(?P<k>a)b", &[(b"ab", true)]),
            (
                r"\.\s+<\(k3s completion bash\)",
                &[(b". <(k3s completion bash)  # Added\n", true)],
            ),
            (
                r"Defaults(\s)*secure_path(\s)*=",
                &[(b"Defaults    secure_path = /sbin\n", true)],
            ),
            (
                r#"^include "/etc/nftables\.d/\*\.nft"$"#,
                &[(b"include \"/etc/nftables.d/*.nft\"\n", true)],
            ),
            (r"a*?b", &[(b"aab", true)]),
            (r"(?:ab)+$", &[(b"abab\n", true)]),
            (r"#x ", &[(b"#x y", true)]),
        ];
        for (pattern, lines) in cases {
            let regex = compile("regexp", pattern).unwrap_or_else(|err| panic!("{pattern}: {err}"));
            for (line, want) in *lines {
                assert_eq!(regex.is_match(line), *want, "{pattern} on {line:?}");
            }
        }
        for refused in [
            r"a**",
            r"^*",
            r"*a",
            r"a{2}{3}",
            r"a*??",
            r"\8",
            r"[\d-z]",
            r"[z-a]",
            r"(?<n>a)",
            concat!('\\', "u0041"),
            r"[a",
            r"a\",
        ] {
            assert!(
                compile("regexp", refused).is_err(),
                "{refused} was accepted"
            );
        }
    }
}

/// Two backups of one file keep their own content, and a backup never overwrites a file.
#[test]
fn a_backup_keeps_its_own_content_and_overwrites_nothing() {
    {
        let (scratch, context) = scratch("lineinfile-backups");
        let path = scratch.path("f.conf");
        write(&path, "a=1\n", 0o644);
        let mut names = Vec::new();
        for line in ["b=2", "c=3"] {
            let args = json!({"path": path, "line": line, "backup": true});
            let NativeRun::Done(result) = ask(&args, &context) else {
                panic!("{args} was handed back");
            };
            names.push(result.0["backup"].as_str().unwrap().to_string());
        }
        assert_ne!(names[0], names[1]);
        assert_eq!(fs::read_to_string(&names[0]).unwrap(), "a=1\n");
        assert_eq!(fs::read_to_string(&names[1]).unwrap(), "a=1\nb=2\n");
        assert_eq!(fs::read_to_string(&path).unwrap(), "a=1\nb=2\nc=3\n");
        let pattern = format!(
            r"^{}\.\d+\.\d{{4}}-\d{{2}}-\d{{2}}@\d{{2}}:\d{{2}}:\d{{2}}~$",
            regex::escape(&path)
        );
        for name in &names {
            assert!(
                Regex::new(&pattern).unwrap().is_match(name.as_bytes()),
                "{name} does not have the reference's shape"
            );
        }
    }
    {
        let (scratch, _) = scratch("lineinfile-backup-taken");
        let path = scratch.path("f.conf");
        let taken = scratch.path("f.conf.1.2026-01-01@00:00:00~");
        write(&path, "a=1\n", 0o644);
        write(&taken, "older backup\n", 0o644);
        assert!(matches!(
            backup_copy(&path, &taken, super::super::runtime::unbounded()),
            Err(BackupError::Exists(_))
        ));
        assert_eq!(fs::read_to_string(&taken).unwrap(), "older backup\n");
    }
}

/// An immutable file or one with xattrs or flags is handed back.
#[test]
fn a_file_the_native_cannot_change_faithfully_is_handed_back() {
    {
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skipped: only root can make a file immutable");
            return;
        }
        let (scratch, context) = scratch("lineinfile-immutable");
        let path = scratch.path("f.conf");
        write(&path, "a=1\n", 0o644);
        let file = fs::File::open(&path).unwrap();
        let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
        let mut flags: libc::c_long = 0;
        // SAFETY: both ioctls read or write one `long`.
        let set = unsafe {
            libc::ioctl(fd, libc::FS_IOC_GETFLAGS, &raw mut flags) == 0 && {
                flags |= IMMUTABLE;
                libc::ioctl(fd, libc::FS_IOC_SETFLAGS, &raw const flags) == 0
            }
        };
        if !set {
            eprintln!("skipped: this file system takes no immutable flag");
            return;
        }
        let before = snapshot(&scratch);
        let answer = ask(&json!({"path": path, "line": "b=2"}), &context);
        let after = snapshot(&scratch);
        flags &= !IMMUTABLE;
        // SAFETY: as above.
        unsafe { libc::ioctl(fd, libc::FS_IOC_SETFLAGS, &raw const flags) };
        assert!(
            matches!(answer, NativeRun::Fallback(_)),
            "an immutable file was answered"
        );
        assert_eq!(after, before);
    }
    {
        let (scratch, context) = scratch("lineinfile-xattr");
        let tagged = scratch.path("x.conf");
        let flagged = scratch.path("n.conf");
        write(&tagged, "a=1\n", 0o644);
        write(&flagged, "a=1\n", 0o644);
        let name = CString::new(tagged.as_str()).unwrap();
        // SAFETY: a valid path, attribute name and value.
        let tagged_ok = unsafe {
            libc::setxattr(
                name.as_ptr(),
                c"user.volant".as_ptr(),
                c"1".as_ptr().cast(),
                1,
                0,
            )
        } == 0;
        let file = fs::File::open(&flagged).unwrap();
        let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
        let mut flags: libc::c_long = 0;
        // SAFETY: both ioctls read or write one `long`; 0x40 is `FS_NODUMP_FL`.
        let flagged_ok = unsafe {
            libc::ioctl(fd, libc::FS_IOC_GETFLAGS, &raw mut flags) == 0 && {
                flags |= 0x40;
                libc::ioctl(fd, libc::FS_IOC_SETFLAGS, &raw const flags) == 0
            }
        };
        if !tagged_ok || !flagged_ok {
            eprintln!("skipped: this file system takes no user xattr or no nodump flag");
            return;
        }
        let before = snapshot(&scratch);
        for args in [
            json!({"path": tagged, "line": "b=2"}),
            json!({"path": flagged, "line": "b=2", "backup": true}),
        ] {
            assert!(
                matches!(ask(&args, &context), NativeRun::Fallback(_)),
                "{args} was answered"
            );
            assert_eq!(snapshot(&scratch), before, "{args} changed the disk");
        }
    }
}

/// A validate naming no program is not quoted; a failing one leaves the file untouched.
#[test]
fn a_validate_is_not_quoted_and_a_failing_one_leaves_the_file() {
    {
        let (scratch, context) = scratch("lineinfile-no-program");
        let args = json!({"path": scratch.path("f.conf"), "line": "b=2", "validate": "'secret %s"});
        let cancelled = || false;
        let clock = clock(&context, &cancelled);
        let params = module_args(SPEC, args.as_object().unwrap());
        let request = request(&params, clock).ok().unwrap();
        let Err(Stop::Fail(fail)) = write_changes(&request, b"b=2\n", &context.remote_tmp, clock)
        else {
            panic!("the write did not fail");
        };
        assert_eq!(fail["msg"], "validate names no program");
    }
    {
        let (scratch, context) = scratch("lineinfile-validate");
        let path = scratch.path("f.conf");
        write(&path, "a=1\n", 0o644);
        for (validate, msg) in [
            (
                "sh -c 'echo bad >&2; exit 3' %s",
                "failed to validate: rc:3 error:bad\n",
            ),
            ("true", "validate must contain %s: true"),
        ] {
            let args = json!({"path": path, "line": "b=2", "validate": validate, "backup": true});
            let NativeRun::Done(result) = ask(&args, &context) else {
                panic!("{validate} was handed back");
            };
            assert_eq!(result.0["msg"], msg);
            assert_eq!(result.0["failed"], true);
            assert_eq!(fs::read_to_string(&path).unwrap(), "a=1\n");
            assert_eq!(fs::read_dir(scratch.path(".tmp")).unwrap().count(), 0);
        }
        let backups = fs::read_dir(&scratch.0)
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with('~')
            })
            .count();
        assert_eq!(backups, 2, "one backup per task");
    }
}

/// Outside the subset, or where the reference would warn, convert or refuse, nothing is written.
#[test]
fn lineinfile_hands_back_before_changing_anything() {
    let (scratch, context) = scratch("lineinfile-back");
    let path = scratch.path("f.conf");
    write(&path, "a=1\n", 0o644);
    std::os::unix::fs::symlink(&path, scratch.path("l")).unwrap();
    let before = snapshot(&scratch);
    let mut environment = context.clone();
    environment
        .environment
        .insert("PATH".into(), "/nowhere".into());
    let nowhere = Context::default();
    for (args, context) in [
        (
            json!({"regexp": "^(a)=", "line": "\\1=2", "backrefs": true}),
            &context,
        ),
        (json!({"line": "b=2", "attributes": "+i"}), &context),
        (json!({"line": "b=2", "unsafe_writes": true}), &context),
        (json!({"line": "b=2", "create": "yes"}), &context),
        (json!({"line": 5}), &context),
        (json!({"line": "b=2", "state": "gone"}), &context),
        (
            json!({"line": "b=2", "regexp": "a", "search_string": "b"}),
            &context,
        ),
        (
            json!({"line": "b=2", "insertafter": "a", "insertbefore": "b"}),
            &context,
        ),
        (json!({"line": "b=2", "regexp": ""}), &context),
        (json!({"line": "b=2", "nope": 1}), &context),
        (json!({"line": "b=2", "path": "f.conf"}), &context),
        (json!({"line": "b=2", "path": scratch.path("l")}), &context),
        (json!({"line": "b=2"}), &environment),
        (
            json!({"line": "b=2", "validate": "test -s $HOME %s"}),
            &context,
        ),
        (
            json!({"line": "b=2", "validate": "volant-no-such-validator %s"}),
            &context,
        ),
        (
            json!({"line": "b=2", "validate": "test -s %s %d"}),
            &context,
        ),
        (
            json!({"line": "b=2", "validate": "test -s %s # non-empty"}),
            &context,
        ),
        (json!({"line": "b=2", "mode": "0o644"}), &context),
        (json!({"line": "b=2"}), &nowhere),
    ] {
        let mut args = args;
        if args.get("path").is_none() {
            args["path"] = path.clone().into();
        }
        assert!(
            matches!(ask(&args, context), NativeRun::Fallback(_)),
            "{args} was answered"
        );
        assert_eq!(snapshot(&scratch), before, "{args} changed the disk");
    }

    // A directory the agent cannot write, where root could.
    if unsafe { libc::geteuid() } != 0 {
        let dir = scratch.path("ro");
        fs::create_dir(&dir).unwrap();
        write(&scratch.path("ro/f.conf"), "a=1\n", 0o644);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let before = snapshot(&scratch);
        let args = json!({"path": scratch.path("ro/f.conf"), "line": "b=2", "backup": true});
        let answer = ask(&args, &context);
        let after = snapshot(&scratch);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            matches!(answer, NativeRun::Fallback(_)),
            "{args} was answered"
        );
        assert_eq!(after, before, "{args} changed the disk");
    }
}

/// A `validate` that hangs ends with the task's `timeout`, as the Python module does, and with
/// the controller's cancel; the file stays as it was either way.
#[test]
fn a_hung_validate_ends_with_the_timeout_or_the_cancel() {
    let (scratch, mut context) = scratch("lineinfile-hung");
    let path = scratch.path("f.conf");
    write(&path, "a=1\n", 0o644);
    let args = json!({"path": path, "line": "b=2", "validate": "sh -c 'sleep 30' %s"});
    context.timeout = Some(Duration::from_secs(1));
    let (timed, cancel_context) = (
        context.clone(),
        Context {
            timeout: None,
            ..context
        },
    );
    let answer = {
        let args = args.clone();
        within(20, move || ask(&args, &timed))
    };
    let NativeRun::Done(result) = answer else {
        panic!("the timeout was not reported");
    };
    assert_eq!(result.0["msg"], "Task failed: Timed out after 1 second(s).");
    assert_eq!(fs::read_to_string(&path).unwrap(), "a=1\n");

    let cancelled = within(20, move || {
        let flag = Arc::new(AtomicBool::new(false));
        let setter = Arc::clone(&flag);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            setter.store(true, Ordering::SeqCst);
        });
        matches!(
            run(args.as_object().unwrap(), &cancel_context, &|| flag
                .load(Ordering::SeqCst)),
            NativeRun::Cancelled
        )
    });
    assert!(cancelled, "the cancel was not honoured");
    assert_eq!(fs::read_to_string(&path).unwrap(), "a=1\n");
}
