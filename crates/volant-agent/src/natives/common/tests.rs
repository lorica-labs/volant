// SPDX-License-Identifier: GPL-3.0-or-later

use std::io;
#[cfg(target_os = "linux")]
use std::{fs, os::unix::fs::MetadataExt};

use serde_json::json;

#[cfg(target_os = "linux")]
use super::super::runtime::unbounded;
use super::*;

/// `heuristic_log_sanitize` answers as the reference's does (measured on 2.19.12), quirks included.
#[test]
fn credentials_are_masked_as_the_reference_masks_them() {
    for (text, want) in [
        ("https://u:secret@h/x", "https://u:********@h/x"),
        ("user:pw@host:path", "user:********@host:path"),
        ("a@b", "a@b"),
        ("x://a:b@c/y://d:e@f", "x://a:********@c/y://d:********@f"),
        ("ssh://git@host/r", "ssh:********@host/r"),
        ("p:q@r@s", "p:q@r@s"),
        ("http://u:p@h/@z", "http://u:********@z"),
        ("\u{e9}:\u{fc}@x", "\u{e9}:\u{fc}@x"),
        ("a:b@", "a:b@"),
    ] {
        assert_eq!(heuristic_log_sanitize(text), want, "{text}");
    }
}

/// The program in a `Popen` error is printed as a Python bytes literal.
#[test]
fn a_program_name_is_printed_as_bytes() {
    let cause = popen_error(2, "/tmp/it's\\a\u{e9}")["event"]["chain"]["event"]["msg"].clone();
    assert_eq!(
        cause,
        json!(r#"[Errno 2] No such file or directory: b"/tmp/it's\\a\xc3\xa9""#)
    );
}

/// The invocation resolves aliases and defaults, hands back unknown arguments, and prints OS errors like Python.
#[test]
fn the_invocation_and_errors_read_like_the_reference() {
    {
        const SPEC: &[ArgSpec] = &[
            ArgSpec {
                name: "path",
                aliases: &["dest", "name"],
                default: || Value::Null,
            },
            ArgSpec {
                name: "follow",
                aliases: &[],
                default: || Value::Bool(false),
            },
            ArgSpec {
                name: "checksum_algorithm",
                aliases: &["checksum"],
                default: || json!("sha1"),
            },
            ArgSpec {
                name: "get_mime",
                aliases: &[],
                default: || Value::Null,
            },
        ];
        use crate::testing::args;
        assert_eq!(
            invocation(SPEC, &args(json!({"dest": "/etc/hostname"}))),
            json!({"module_args": {
                "dest": "/etc/hostname",
                "path": "/etc/hostname",
                "follow": false,
                "checksum_algorithm": "sha1",
                "get_mime": null,
            }})
        );
        assert_eq!(
            invocation(
                SPEC,
                &args(json!({"path": "/a", "dest": "/b", "name": "/c"}))
            )["module_args"]["path"],
            "/c",
            "the last alias given wins, over the canonical name too"
        );
    }
    {
        const SPEC: &[ArgSpec] = &[ArgSpec {
            name: "path",
            aliases: &["dest"],
            default: || Value::Null,
        }];
        use crate::testing::args;
        assert_eq!(check_names(SPEC, &args(json!({"dest": "/a"}))), Ok(()));
        assert!(check_names(SPEC, &args(json!({"path": "/a", "dest": "/a"}))).is_err());
        assert!(check_names(SPEC, &args(json!({"path": "/a", "bogus": 1}))).is_err());
    }
    {
        let denied = io::Error::from_raw_os_error(libc::EACCES);
        assert_eq!(
            os_error(&denied, "/x'y"),
            "[Errno 13] Permission denied: b\"/x'y\""
        );
        assert_eq!(
            os_error(&denied, "/é\n"),
            "[Errno 13] Permission denied: b'/\\xc3\\xa9\\n'"
        );
    }
}

/// `parse_mode` against `AnsibleModule._symbolic_mode_to_octal` of ansible-core 2.19.12, whose
/// answers for these modes, run under umask 022, are the expected values here.
#[test]
#[cfg(target_os = "linux")]
fn a_mode_is_read_as_the_reference_reads_it() {
    // nextest runs each test in its own process: the umask set here reaches no other test.
    unsafe { libc::umask(0o022) };
    for (mode, current, is_dir, want) in [
        ("u=rwx,g=rx,o=", 0o755, true, 0o750),
        ("a+X", 0o644, false, 0o644),
        ("a+X", 0o644, true, 0o755),
        ("u+s,g-w,o=t", 0o775, false, 0o5750),
        ("go=u", 0o640, false, 0o666),
        ("+x", 0o600, false, 0o711),
        ("=r", 0o777, false, 0o444),
        ("+w", 0o600, false, 0o600),
        ("=rw", 0o777, false, 0o644),
        ("-w", 0o666, false, 0o466),
        ("u=rw-x+X", 0o700, false, 0o600),
        ("ug+rw,o-rwx", 0o007, true, 0o660),
        ("u", 0o640, false, 0o640),
        ("", 0o640, false, 0o640),
        ("u-rwx,g=o", 0o6754, false, 0o4044),
        ("o+t,g+s,u+s", 0o755, true, 0o7755),
        ("u+x,", 0o644, false, 0o744),
        ("0755", 0o600, false, 0o755),
        ("755", 0o600, false, 0o755),
    ] {
        assert_eq!(
            parse_mode(&json!(mode), current, is_dir),
            Ok(want),
            "{mode}"
        );
    }
    assert_eq!(parse_mode(&json!(493), 0o600, false), Ok(0o755));
    for bad in ["0x755", "u=rwz", "q+x", "rwx"] {
        assert_eq!(
            parse_mode(&json!(bad), 0o644, false),
            Err(ModeError::Invalid(format!(
                "bad symbolic permission for mode: {bad}"
            )))
        );
    }
    for other in [
        json!("0o755"),
        json!("+755"),
        json!(" 755"),
        json!(7.5),
        json!(true),
        json!(-1),
    ] {
        assert!(
            matches!(
                parse_mode(&other, 0o644, false),
                Err(ModeError::Unsupported(_))
            ),
            "{other}"
        );
    }
}

/// Accounts are found by name and id, and through `getent` when only the name service knows them.
#[test]
#[cfg(target_os = "linux")]
fn accounts_are_found_by_name_id_and_name_service() {
    {
        let clock = unbounded();
        assert_eq!(lookup_user("0", clock).unwrap().unwrap().name, "root");
        assert_eq!(lookup_user("root", clock).unwrap().unwrap().uid, 0);
        assert_eq!(lookup_group("0", clock).unwrap().unwrap().name, "root");
        assert_eq!(lookup_group("root", clock).unwrap().unwrap().gid, 0);
        assert!(lookup_user("volant-no-such-user", clock).unwrap().is_none());
        assert!(matches!(
            owner_account("1234", clock),
            Ok(Account::Id(1234))
        ));
        assert!(matches!(
            owner_account("volant-no-such-user", clock),
            Ok(Account::Unknown(_))
        ));
        assert!(
            owner_account("+12", clock).is_err(),
            "Python reads +12 as a number"
        );
    }
    {
        let scratch = golden::Scratch::new("getent-path");
        let before = std::env::var_os("PATH");
        let during = {
            let _path = golden::fake_getent(&scratch, "exit 2");
            std::env::var_os("PATH")
        };
        assert_ne!(during, before, "the fake was not put on PATH");
        assert_eq!(std::env::var_os("PATH"), before);
    }
    {
        let scratch = golden::Scratch::new("getent");
        let _path = golden::fake_getent(
            &scratch,
            r#"case "$1 $2" in
  "passwd volant-ldapuser"|"passwd 4242") echo "volant-ldapuser:x:4242:4242::/:/bin/sh" ;;
  "group volant-ldapgroup"|"group 4243") echo "volant-ldapgroup:x:4243:" ;;
  *) exit 2 ;;
esac"#,
        );
        let clock = unbounded();
        assert!(matches!(
            owner_account("volant-ldapuser", clock),
            Ok(Account::Id(4242))
        ));
        assert_eq!(
            lookup_user("4242", clock).unwrap().unwrap().name,
            "volant-ldapuser"
        );
        assert!(matches!(
            group_account("volant-ldapgroup", clock),
            Ok(Account::Id(4243))
        ));
        assert_eq!(
            lookup_group("4243", clock).unwrap().unwrap().name,
            "volant-ldapgroup"
        );
        assert!(matches!(
            owner_account("volant-nobody", clock),
            Ok(Account::Unknown(_))
        ));
    }
}

/// Owner, then group, then mode, and the first failure stops the rest.
#[test]
#[cfg(target_os = "linux")]
fn attributes_are_set_in_order_and_stop_at_the_first_failure() {
    let scratch = golden::Scratch::new("attrs");
    let file = scratch.fixture();
    let me = Account::Id(unsafe { libc::geteuid() });
    let err = set_fs_attributes(
        &file,
        Some(&me),
        Some(&Account::Unknown("volant-no-such-group".into())),
        Some(&json!("0600")),
    )
    .unwrap_err();
    let FsError::Failed(fail) = err else {
        panic!("{err:?}")
    };
    assert_eq!(
        fail["msg"],
        "chgrp failed: failed to look up group volant-no-such-group"
    );
    assert_eq!(fs::metadata(&file).unwrap().mode() & 0o7777, 0o644);
    assert!(set_fs_attributes(&file, None, None, Some(&json!("0600"))).unwrap());
    assert!(!set_fs_attributes(&file, None, None, Some(&json!(384))).unwrap());
}
