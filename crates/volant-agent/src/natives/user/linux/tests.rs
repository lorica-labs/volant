// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::json;

use super::*;
use crate::natives::common::golden::fake_getent;
use crate::natives::group::tests::{Fakes, args, scratch_name};
use crate::natives::setup::unbounded;

fn ask(args: &Map<String, Value>, context: &Context) -> Result<Map<String, Value>, Stop> {
    answer(args, context, unbounded(), NSSWITCH)
}

/// `useradd`, `usermod` and `userdel` take the module's arguments in its order, only where needed.
#[test]
#[expect(clippy::too_many_lines)]
fn useradd_usermod_and_userdel_follow_the_module() {
    {
        let fakes = Fakes::new("useradd", &["useradd"], 0);
        if fakes.outside(ask) {
            return;
        }
        let name = scratch_name("volantu");
        let mut created = ask(
            &args(json!({
                "name": name, "uid": 64998, "group": "root", "groups": "root",
                "comment": "c", "home": "/nonexistent-volant", "shell": "/bin/sh",
                "create_home": false, "system": true,
            })),
            &fakes.context,
        )
        .unwrap();
        assert_eq!(
            fakes.argv("useradd").unwrap(),
            [
                "-u",
                "64998",
                "-g",
                "root",
                "-G",
                "root",
                "-c",
                "c",
                "-d",
                "/nonexistent-volant",
                "-s",
                "/bin/sh",
                "-M",
                "-r",
                name.as_str(),
            ]
        );
        // A string `groups` is split, and shown as the list.
        assert_eq!(
            created["invocation"]["module_args"]["groups"],
            json!(["root"])
        );
        created.remove("invocation");
        assert_eq!(
            Value::Object(created),
            json!({"name": name, "state": "present", "changed": true, "system": true,
               "create_home": false, "stdout": "out\n", "stderr": "err\n"})
        );

        // `users` is a group on Debian and Ubuntu, and no account.
        ask(&args(json!({"name": "users"})), &fakes.context).unwrap();
        assert_eq!(fakes.argv("useradd").unwrap(), ["-N", "-m", "users"]);
    }
    {
        let fakes = Fakes::new("usermod", &["usermod"], 0);
        if fakes.outside(ask) {
            return;
        }
        // The account is in no group's member list.
        fakes.script("getent", "[ \"$1\" = group ] && echo 'root:x:0:'");
        let mut same = ask(
        &args(json!({"name": "root", "uid": 0, "group": "root", "home": "/root", "create_home": false})),
        &fakes.context,
    )
    .unwrap();
        assert_eq!(fakes.argv("usermod"), None);
        same.remove("invocation");
        let shell = same["shell"].clone();
        assert_eq!(
            Value::Object(same),
            json!({"name": "root", "state": "present", "changed": false, "append": false,
               "move_home": false, "uid": 0, "group": 0, "comment": "root",
               "home": "/root", "shell": shell})
        );

        let changed = ask(
            &args(json!({
                "name": "root", "uid": 1, "group": "daemon", "groups": ["root"],
                "comment": "c", "home": "/root", "shell": "/bin/volant-none",
                "create_home": false,
            })),
            &fakes.context,
        )
        .unwrap();
        assert_eq!(
            fakes.argv("usermod").unwrap(),
            [
                "-u",
                "1",
                "-g",
                "1",
                "-G",
                "root",
                "-c",
                "c",
                "-s",
                "/bin/volant-none",
                "root"
            ]
        );
        assert_eq!(changed["changed"], true);
        assert_eq!(changed["groups"], "root");

        ask(
            &args(
                json!({"name": "root", "groups": ["root"], "append": true, "create_home": false}),
            ),
            &fakes.context,
        )
        .unwrap();
        assert_eq!(fakes.argv("usermod").unwrap(), ["-a", "-G", "root", "root"]);
    }
    {
        let fakes = Fakes::new("usermod-members", &["usermod"], 0);
        if fakes.outside(ask) {
            return;
        }
        fakes.script(
            "getent",
            "[ \"$1\" = group ] && printf 'root:x:0:root\\ndaemon:x:1:bin,root\\n'",
        );
        for (groups, append, argv) in [
            (json!(["root"]), false, Some(vec!["-G", "root", "root"])),
            (json!([]), false, Some(vec!["-G", "", "root"])),
            (json!(["root"]), true, None),
            (json!([]), true, None),
        ] {
            fakes.clear("usermod");
            let given =
                json!({"name": "root", "groups": groups, "append": append, "create_home": false});
            let answer = ask(&args(given), &fakes.context).unwrap();
            assert_eq!(
                fakes.argv("usermod"),
                argv.clone()
                    .map(|argv| argv.into_iter().map(String::from).collect()),
                "groups {groups}, append {append}"
            );
            assert_eq!(answer["changed"], argv.is_some());
        }
    }
    {
        let fakes = Fakes::new("userdel", &["userdel"], 0);
        if fakes.outside(ask) {
            return;
        }
        let mut gone = ask(
            &args(json!({"name": "root", "state": "absent"})),
            &fakes.context,
        )
        .unwrap();
        assert_eq!(fakes.argv("userdel").unwrap(), ["root"]);
        gone.remove("invocation");
        assert_eq!(
            Value::Object(gone),
            json!({"name": "root", "state": "absent", "changed": true, "force": false,
               "remove": false, "stdout": "out\n", "stderr": "err\n"})
        );

        let name = scratch_name("volantm");
        let mut missing = ask(
            &args(json!({"name": name, "state": "absent"})),
            &fakes.context,
        )
        .unwrap();
        missing.remove("invocation");
        assert_eq!(
            Value::Object(missing),
            json!({"name": name, "state": "absent", "changed": false})
        );
    }
}

/// A failing `useradd` is the module's failure; what the native cannot do hands back before any command.
#[test]
#[expect(clippy::too_many_lines)]
fn a_failure_or_a_case_outside_the_subset_is_the_module_s() {
    {
        let fakes = Fakes::new("useradd-fails", &["useradd"], 9);
        if fakes.outside(ask) {
            return;
        }
        let name = scratch_name("volantf");
        let mut answer = ask(
            &args(json!({"name": name, "create_home": false})),
            &fakes.context,
        )
        .unwrap();
        assert!(answer.remove("invocation").is_some());
        assert_eq!(
            Value::Object(answer),
            json!({"failed": true, "name": name, "msg": "err\n", "rc": 9})
        );
    }
    {
        let fakes = Fakes::new("user-subset", &["useradd", "usermod", "userdel"], 0);
        let name = scratch_name("volants");
        for (key, value) in [
            ("local", json!(true)),
            ("non_unique", json!(true)),
            ("password", json!("!")),
            ("password_expire_max", json!(1)),
            ("password_expire_min", json!(1)),
            ("password_expire_warn", json!(1)),
            ("password_expire_account_disable", json!(1)),
            ("password_lock", json!(true)),
            ("update_password", json!("on_create")),
            ("expires", json!(1.0)),
            ("generate_ssh_key", json!(true)),
            ("ssh_key_bits", json!(2048)),
            ("ssh_key_type", json!("ed25519")),
            ("ssh_key_file", json!("/x")),
            ("ssh_key_comment", json!("c")),
            ("ssh_key_passphrase", json!("p")),
            ("remove", json!(true)),
            ("move_home", json!(true)),
            ("seuser", json!("u")),
            ("login_class", json!("c")),
            ("umask", json!("022")),
            ("force", json!(true)),
            ("skeleton", json!("/etc/skel")),
            ("hidden", json!(true)),
            ("profile", json!("p")),
            ("authorization", json!("a")),
            ("role", json!("r")),
            ("uid_min", json!(1000)),
            ("uid_max", json!(2000)),
        ] {
            for state in ["present", "absent"] {
                for who in [name.as_str(), "root"] {
                    let mut given =
                        args(json!({"name": who, "state": state, "create_home": false}));
                    given.insert(key.into(), value.clone());
                    let answer = ask(&given, &fakes.context);
                    assert!(
                        matches!(answer, Err(Stop::HandBack(_))),
                        "{key} was answered"
                    );
                }
            }
        }
        for command in ["useradd", "usermod", "userdel"] {
            assert_eq!(fakes.argv(command), None, "{command} ran");
        }
    }
    {
        let fakes = Fakes::new("user-nss", &["useradd", "userdel"], 0);
        if fakes.outside(ask) {
            return;
        }
        let name = scratch_name("volantn");
        let _path = fake_getent(
            &fakes.scratch,
            &format!(
                "[ \"$1\" = passwd ] && [ \"$2\" = {name} ] && echo '{name}:x:5000:5000::/home/{name}:/bin/sh' && exit 0; exit 2"
            ),
        );
        for state in ["present", "absent"] {
            let answer = ask(
                &args(json!({"name": name, "state": state, "create_home": false})),
                &fakes.context,
            );
            assert!(
                matches!(answer, Err(Stop::HandBack(_))),
                "{state} was answered"
            );
        }
        assert_eq!(fakes.argv("useradd"), None);
        assert_eq!(fakes.argv("userdel"), None);
    }
    {
        let fakes = Fakes::new("user-home", &["useradd", "usermod"], 0);
        if fakes.outside(ask) {
            return;
        }
        let missing = format!("/nonexistent-volant-{}", std::process::id());
        let name = scratch_name("volanth");
        let parent = ask(
            &args(json!({"name": name, "home": format!("{missing}/home")})),
            &fakes.context,
        );
        assert!(matches!(parent, Err(Stop::HandBack(_))));
        let home = ask(
            &args(json!({"name": "root", "home": missing})),
            &fakes.context,
        );
        assert!(matches!(home, Err(Stop::HandBack(_))));
        assert_eq!(fakes.argv("useradd"), None);
        assert_eq!(fakes.argv("usermod"), None);
    }
}

/// A home `useradd -m` left missing is made as `create_homedir` makes it, from the skeleton.
#[test]
fn a_missing_home_is_made_from_the_skeleton() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let fakes = Fakes::new("user-skel", &[], 0);
    let skel = fakes.scratch.path("skel");
    std::fs::create_dir_all(format!("{skel}/sub")).unwrap();
    std::fs::write(format!("{skel}/.profile"), "p\n").unwrap();
    std::fs::set_permissions(
        format!("{skel}/.profile"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    std::os::unix::fs::symlink(".profile", format!("{skel}/link")).unwrap();
    std::fs::set_permissions(
        format!("{skel}/sub"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let defs = fakes.scratch.path("login.defs");
    for (text, mode) in [
        ("HOME_MODE\t0750\nUMASK\t\t077\n", 0o750),
        ("UMASK 077\n", 0o700),
        ("# HOME_MODE 0700\nUMASK 022 \n", 0o755),
    ] {
        let home = fakes.scratch.path("home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::write(&defs, text).unwrap();
        create_home(
            Path::new(&skel),
            Path::new(&defs),
            Path::new(&home),
            uid,
            gid,
        )
        .unwrap();
        let meta = std::fs::metadata(&home).unwrap();
        assert_eq!(meta.mode() & 0o7777, mode, "{text:?}");
        assert_eq!(meta.uid(), uid);
        let profile = std::fs::metadata(format!("{home}/.profile")).unwrap();
        let original = std::fs::metadata(format!("{skel}/.profile")).unwrap();
        assert_eq!(profile.mode() & 0o7777, 0o600);
        assert_eq!(
            (profile.mtime(), profile.mtime_nsec()),
            (original.mtime(), original.mtime_nsec())
        );
        assert_eq!(
            std::fs::read_link(format!("{home}/link")).unwrap(),
            Path::new(".profile")
        );
        let sub = std::fs::metadata(format!("{home}/sub")).unwrap();
        assert_eq!(sub.mode() & 0o7777, 0o700);
    }
    let bare = fakes.scratch.path("bare");
    let no_skel = fakes.scratch.path("no-skel");
    let no_defs = fakes.scratch.path("no-defs");
    create_home(
        Path::new(&no_skel),
        Path::new(&no_defs),
        Path::new(&bare),
        uid,
        gid,
    )
    .unwrap();
    assert!(Path::new(&bare).is_dir());
    assert!(setting("HOME_MODE 0789", "HOME_MODE").unwrap().is_err());
    assert!(setting("HOME_MODES 0700", "HOME_MODE").is_none());
}

/// `invocation` holds every argument of the module with its default, as recorded.
#[test]
fn the_invocation_is_the_module_s() {
    let recorded: Value = serde_json::from_str(include_str!(
        "../../../../../volant/tests/golden/module/user-absent-missing.json"
    ))
    .unwrap();
    let mut want = recorded["invocation"].clone();
    want["module_args"]["ssh_key_comment"] =
        Value::from(format!("ansible-generated on {}", hostname()));
    assert_eq!(
        invocation(
            SPEC,
            &args(json!({"name": "volantshape", "state": "absent"}))
        ),
        want
    );
    assert_eq!(dirname("/a/b"), "/a");
    assert_eq!(dirname("/a"), "/");
    assert_eq!(dirname("//a//b"), "//a");
    assert_eq!(listed("a, b,,a"), ["a", "b"]);
}
