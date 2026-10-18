// SPDX-License-Identifier: GPL-3.0-or-later
use super::announce::module_path_dirs;
use super::args::PlaybookArgs;
use super::hosts::{order_hosts, shuffle};
use super::passwords::{password_from_file, ssh_words};
use super::run::armed;
#[cfg(unix)]
use super::run::run;
use crate::config::Config;
use crate::inventory::Host;
use crate::python::{self, Facts, Natives};
use crate::stats::error_code;
#[cfg(unix)]
use crate::testing::scratch;
use clap::Parser;

/// `--facts` takes one of its three words and nothing else, `auto` when it is not given, and
/// `--profile` is a plain flag.
#[test]
fn facts_takes_one_of_three_words() {
    let parse = |extra: &[&str]| {
        PlaybookArgs::try_parse_from(["volant"].iter().chain(extra).chain(&["site.yml"]))
    };
    let args = parse(&[]).unwrap();
    assert_eq!((args.facts, args.profile), (Facts::Auto, false));
    let args = parse(&["--facts", "python", "--profile"]).unwrap();
    assert_eq!((args.facts, args.profile), (Facts::Python, true));
    assert_eq!(parse(&["--facts=native"]).unwrap().facts, Facts::Native);
    let err = parse(&["--facts", "nope"]).unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue, "{err}");
}

/// `-M` and `--module-path` repeat and take colon lists; a later one goes in front, and the order
/// inside one stays.
#[rstest::rstest]
#[case::one(&["-M", "/a"], &["/a"])]
#[case::list(&["--module-path", "/a::/b"], &["/a", "/b"])]
#[case::later_first(&["-M", "/a:/b", "-M", "/c"], &["/c", "/a", "/b"])]
fn module_path_goes_in_front_in_the_reference_order(
    #[case] flags: &[&str],
    #[case] expected: &[&str],
) {
    let args =
        PlaybookArgs::try_parse_from(["volant"].iter().chain(flags).chain(&["site.yml"])).unwrap();
    let dirs: Vec<String> = module_path_dirs(&args)
        .iter()
        .map(|d| d.display().to_string())
        .collect();
    assert_eq!(dirs, expected);
}

/// The switch and `--facts` reach every payload the run sends, from the environment through the
/// configuration to the union the tasks take their payloads from.
#[test]
fn the_native_switch_and_facts_reach_the_payloads() {
    let core = |name: &str| python::ModuleFacts {
        module_fqn: format!("ansible.modules.{name}"),
        profile: "legacy".into(),
        rlimit_nofile: 0,
        extensions: serde_json::Map::new(),
        core: true,
    };
    let union = python::Union {
        hash: "ab".into(),
        zip_b64: "UEsDBA==".into(),
        modules: ["stat", "setup"]
            .into_iter()
            .map(|m| (m.to_string(), core(m)))
            .collect(),
        refused: std::collections::BTreeMap::default(),
        natives: Natives::default(),
    };
    let forced = |config: &Config, extra: &[&str], module: &str| {
        let args =
            PlaybookArgs::try_parse_from(["volant"].iter().chain(extra).chain(&["site.yml"]))
                .unwrap();
        armed(union.clone(), config.native_modules, args.facts)
            .payload(module)
            .expect("the union holds it")
            .force_python
    };
    unsafe {
        std::env::set_var("ANSIBLE_CONFIG", "/nonexistent/volant/ansible.cfg");
        std::env::remove_var("VOLANT_NATIVE_MODULES");
    }
    let on = Config::load().unwrap();
    assert!(!forced(&on, &[], "stat"));
    assert!(forced(&on, &[], "setup"), "--facts auto");
    assert!(forced(&on, &["--facts", "python"], "setup"));
    assert!(!forced(&on, &["--facts", "native"], "setup"));
    unsafe { std::env::set_var("VOLANT_NATIVE_MODULES", "0") };
    let off = Config::load().unwrap();
    assert!(forced(&off, &[], "stat"), "VOLANT_NATIVE_MODULES=0");
    assert!(forced(&off, &["--facts", "native"], "setup"));
}

/// Captures what `f` writes on this process's standard output, at the descriptor.
#[cfg(unix)]
fn stdout_of(f: impl FnOnce()) -> String {
    use std::io::Write as _;
    use std::os::fd::AsRawFd as _;
    let dir = scratch();
    let path = dir.path().join("stdout");
    let file = std::fs::File::create(&path).unwrap();
    std::io::stdout().flush().unwrap();
    let saved = unsafe { libc::dup(1) };
    assert!(saved >= 0);
    assert!(unsafe { libc::dup2(file.as_raw_fd(), 1) } >= 0);
    f();
    std::io::stdout().flush().unwrap();
    unsafe {
        libc::dup2(saved, 1);
        libc::close(saved);
    }
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    text
}

/// `--profile` adds nothing to standard output, and `VOLANT_PROFILE_JSON` gets the profile
/// whatever became of the run.
#[cfg(unix)]
#[test]
fn the_profile_stays_off_stdout_and_lands_in_its_file() {
    let dir_guard = scratch();
    let dir = dir_guard.path().to_path_buf();
    std::fs::create_dir_all(&dir).unwrap();
    let playbook = dir.join("site.yml");
    std::fs::write(
        &playbook,
        "- hosts: nosuch
  gather_facts: false
  tasks:
    - debug: {msg: hi}
",
    )
    .unwrap();
    let json = dir.join("profile.jsonl");
    unsafe {
        std::env::set_var("ANSIBLE_CONFIG", "/nonexistent/volant/ansible.cfg");
        std::env::set_var("VOLANT_PROFILE_JSON", &json);
    }
    let playbook_arg = playbook.display().to_string();
    let args = |extra: &[&str]| {
        PlaybookArgs::try_parse_from(
            ["volant", "--no-color"]
                .iter()
                .chain(extra)
                .chain(&[playbook_arg.as_str()]),
        )
        .unwrap()
    };
    let plain = stdout_of(|| assert_eq!(run(args(&[])), 0));
    let profiled = stdout_of(|| assert_eq!(run(args(&["--profile"])), 0));
    assert!(plain.contains("PLAY RECAP"), "{plain}");
    assert_eq!(profiled, plain);
    let first = std::fs::read_to_string(&json).unwrap();
    assert_eq!(first.lines().next(), Some(r#"{"natives":{}}"#), "{first}");

    std::fs::remove_file(&json).unwrap();
    let missing = dir.join("missing.yml").display().to_string();
    let args = PlaybookArgs::try_parse_from(["volant", missing.as_str()]).unwrap();
    assert_ne!(run(args), 0);
    let written = std::fs::read_to_string(&json).unwrap();
    assert_eq!(
        written.lines().next(),
        Some(r#"{"natives":{}}"#),
        "{written}"
    );
}

/// A password file gives its whole text, surrounding whitespace removed; a missing or
/// empty one is refused in the reference's words.
#[rstest::rstest]
#[case::newline(
    Some(
        "sekret
"
    ),
    Ok("sekret")
)]
#[case::no_newline(Some("sekret"), Ok("sekret"))]
#[case::inner_space(
    Some(
        "sek ret 

"
    ),
    Ok("sek ret")
)]
#[case::leading_newline(
    Some(
        "
sekret
"
    ),
    Ok("sekret")
)]
#[case::several_lines(
    Some(
        "one
two
"
    ),
    Ok(
        "one
two"
    )
)]
#[case::crlf(
    Some(
        "sekret
"
    ),
    Ok("sekret")
)]
#[case::empty(Some(""), Err("Empty password was provided from file (<path>)"))]
#[case::newlines_only(
    Some(
        "

"
    ),
    Err("Empty password was provided from file (<path>)")
)]
#[case::spaces_only(
    Some(
        "  
"
    ),
    Err("Empty password was provided from file (<path>)")
)]
#[case::missing(None, Err("The password file <path> was not found"))]
fn a_password_file_gives_its_text(#[case] content: Option<&str>, #[case] want: Result<&str, &str>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pw");
    if let Some(content) = content {
        std::fs::write(&path, content).unwrap();
    }
    let got = password_from_file(&path);
    let want = want
        .map(str::to_string)
        .map_err(|m| m.replace("<path>", &path.display().to_string()));
    assert_eq!(got, want);
}

/// A directory exists and cannot be read; an executable file gives its output, or its
/// failure.
#[cfg(unix)]
#[test]
fn a_directory_and_a_script_as_password_files() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        password_from_file(dir.path()),
        Err(format!(
            "Could not read password file '{}'.",
            dir.path().display()
        ))
    );
    let script = |name: &str, body: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    assert_eq!(
        password_from_file(&script(
            "ok",
            "#!/bin/sh
printf 'a b \n'
"
        )),
        Ok("a b ".to_string())
    );
    let failing = script(
        "bad",
        "#!/bin/sh
echo nope >&2
exit 3
",
    );
    assert_eq!(
        password_from_file(&failing),
        Err(format!(
            "The password script {} returned an error (rc=3): nope
",
            failing.display()
        ))
    );
}

/// The prompt and the file for one password are exclusive, and `ssh` arguments with an
/// open quote are refused by name.
#[rstest::rstest]
#[case::ask_and_file(&["-k", "--connection-password-file", "f"], false)]
#[case::alias(&["-k", "--conn-pass-file", "f"], false)]
#[case::become_ask_and_file(&["-K", "--become-password-file", "f"], false)]
#[case::become_alias(&["-K", "--become-pass-file", "f"], false)]
#[case::one_each(&["-k", "-K"], true)]
#[case::files(&["--conn-pass-file", "f", "--become-pass-file", "g"], true)]
fn password_options_exclude_each_other(#[case] flags: &[&str], #[case] parses: bool) {
    let argv = ["volant"]
        .into_iter()
        .chain(flags.iter().copied())
        .chain(["site.yml"]);
    assert_eq!(PlaybookArgs::try_parse_from(argv).is_ok(), parses);
    assert!(ssh_words("--ssh-common-args", Some("-o 'ProxyJump=a")).is_err());
    assert_eq!(
        ssh_words("--ssh-extra-args", Some("-o A=1 -v")).unwrap(),
        ["-o", "A=1", "-v"]
    );
}

/// `order` arranges a play's hosts, `shuffle` keeps every one of them, and a value the
/// reference does not know is refused with its own exit code.
#[rstest::rstest]
#[case::inventory(None, Ok(vec!["hc", "ha", "hb"]))]
#[case::inventory_named(Some("inventory"), Ok(vec!["hc", "ha", "hb"]))]
#[case::reverse_inventory(Some("reverse_inventory"), Ok(vec!["hb", "ha", "hc"]))]
#[case::sorted(Some("sorted"), Ok(vec!["ha", "hb", "hc"]))]
#[case::reverse_sorted(Some("reverse_sorted"), Ok(vec!["hc", "hb", "ha"]))]
#[case::bogus(
    Some("bogus"),
    Err("Invalid 'order' specified for inventory hosts: bogus")
)]
fn order_arranges_the_hosts(#[case] order: Option<&str>, #[case] want: Result<Vec<&str>, &str>) {
    let mut hosts: Vec<Host> = ["hc", "ha", "hb"]
        .iter()
        .map(|name| Host {
            name: (*name).to_string(),
            vars: std::collections::BTreeMap::new(),
        })
        .collect();
    let got = order_hosts(&mut hosts, order);
    match want {
        Ok(names) => {
            got.expect("the order is known");
            assert_eq!(
                hosts.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(),
                names
            );
        }
        Err(message) => {
            let err = got.expect_err("the order is refused");
            assert_eq!(error_code(&err), 5);
            assert!(format!("{err:#}").contains(message), "{err:#}");
        }
    }
}

#[test]
fn shuffle_keeps_every_host() {
    let mut hosts: Vec<u32> = (0..50).collect();
    shuffle(&mut hosts);
    hosts.sort_unstable();
    assert_eq!(hosts, (0..50).collect::<Vec<_>>());
}
