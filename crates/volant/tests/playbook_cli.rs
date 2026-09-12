// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(unix)]
use std::path::Path;
use std::process::{Command, Output};

fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .display()
        .to_string()
}

fn volant(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(args)
        .env("NO_COLOR", "1")
        .env_remove("COLUMNS")
        // The role search path is part of what several of these tests assert, and it is read
        // from the environment: a machine with `ANSIBLE_ROLES_PATH` exported would redden them
        // for a reason that has nothing to do with the engine.
        .env_remove("ANSIBLE_ROLES_PATH")
        .output()
        .expect("volant runs")
}

/// Runs volant and fails if it has not finished within `deadline`. A barrier that never opens
/// hangs instead of returning, and a hung run only ends at the harness's own timeout, so the
/// tests that prove a wait ends say so with a deadline rather than with elapsed time alone.
fn volant_within(args: &[&str], deadline: std::time::Duration) -> Output {
    volant_within_with_path(args, deadline, None)
}

/// `volant_within`, with an optional directory prepended to `PATH` -- the form `run_probe` needs
/// for the escalation probes, which put a fake `sudo` ahead of the real one rather than resting
/// on properties of the machine.
fn volant_within_with_path(
    args: &[&str],
    deadline: std::time::Duration,
    path: Option<&Path>,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_volant"));
    command
        .args(args)
        .env("NO_COLOR", "1")
        .env_remove("COLUMNS")
        .env_remove("ANSIBLE_ROLES_PATH");
    if let Some(dir) = path {
        command.env(
            "PATH",
            format!(
                "{}:{}",
                dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
    }
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("volant starts");
    let started = std::time::Instant::now();
    loop {
        match child.try_wait().expect("volant is waitable") {
            Some(_) => return child.wait_with_output().expect("volant output"),
            None if started.elapsed() >= deadline => {
                let _ = child.kill();
                let out = child.wait_with_output().expect("volant output");
                panic!(
                    "volant did not finish within {deadline:?}, so a host is still waiting:\n{}",
                    String::from_utf8_lossy(&out.stdout)
                );
            }
            None => std::thread::sleep(std::time::Duration::from_millis(25)),
        }
    }
}

fn settings() -> insta::Settings {
    let mut s = insta::Settings::clone_current();
    s.add_filter(r#""start": "[^"]*""#, r#""start": "[time]""#);
    s.add_filter(r#""end": "[^"]*""#, r#""end": "[time]""#);
    s.add_filter(r#""delta": "[^"]*""#, r#""delta": "[time]""#);
    s.add_filter(r#"The error was: [^"]*"#, "The error was: [engine message]");
    s
}

#[test]
fn a_playbook_runs_end_to_end_with_ansible_output() {
    let out = volant(&[
        "playbook",
        "-i",
        &fixture("inventory.ini"),
        &fixture("site.yml"),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    settings().bind(|| insta::assert_snapshot!(String::from_utf8(out.stdout).unwrap()));
}

#[test]
fn a_failing_task_stops_the_host_and_exits_2() {
    let out = volant(&["playbook", &fixture("failing.yml")]);
    assert_eq!(out.status.code(), Some(2));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("fatal: [localhost]: FAILED!"), "{text}");
    assert!(
        !text.contains("Never reached"),
        "tasks after a failure must not be displayed: {text}"
    );
    assert!(text.contains("failed=1"), "{text}");
}

#[test]
fn changed_when_and_failed_when_apply_to_controller_side_tasks() {
    let out = volant(&["playbook", &fixture("local-conditions.yml")]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(
        text.contains("changed: [localhost]\n")
            && text.contains(r#"changed: [localhost] => {"msg": "changed"}"#),
        "changed_when marks set_fact and debug as changed: {text}"
    );
    assert!(
        text.contains(r#"fatal: [localhost]: FAILED! => {"msg": "assert"}"#),
        "failed_when fails a debug task: {text}"
    );
    assert!(
        !text.contains("Never reached"),
        "the host stops after a failed_when: {text}"
    );
    assert!(
        text.contains(
            "localhost                  : ok=3    changed=2    unreachable=0    failed=1"
        ),
        "{text}"
    );
}

#[test]
fn an_agent_that_dies_mid_batch_makes_its_host_unreachable() {
    let out = volant(&["playbook", &fixture("dying-agent.yml")]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("fatal: [localhost]: UNREACHABLE!"), "{text}");
    assert!(
        !text.contains("Never reached"),
        "tasks after the agent died must not be displayed: {text}"
    );
    assert!(text.contains("unreachable=1"), "{text}");
    assert_eq!(out.status.code(), Some(4), "{text}");
}

#[test]
fn the_playbook_alias_takes_the_same_arguments() {
    let alias = Path::new(env!("CARGO_BIN_EXE_volant")).with_file_name("volant-playbook");
    let out = Command::new(alias)
        .args(["-i", &fixture("inventory.ini"), &fixture("site.yml")])
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn a_bad_playbook_exits_1_with_the_error_on_stderr() {
    let out = volant(&["playbook", &fixture("does-not-exist.yml")]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("does-not-exist.yml"));
}

#[test]
fn variables_loops_conditions_and_facts_render_like_ansible() {
    let out = volant(&[
        "playbook",
        "-i",
        &fixture("vars/inventory.ini"),
        &fixture("vars/site.yml"),
    ]);
    let text = String::from_utf8(out.stdout.clone()).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    settings().bind(|| insta::assert_snapshot!(text));
}

#[test]
fn a_variable_naming_a_later_bound_variable_still_renders() {
    let out = volant(&[
        "playbook",
        "-i",
        &fixture("late/inventory.ini"),
        &fixture("late/site.yml"),
    ]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains("TASK [Say everything]"),
        "a task name renders against the task's own vars: {text}"
    );
    assert!(
        text.contains(r#"(item=a) => {"msg": "greet a"}"#)
            && text.contains(r#"(item=b) => {"msg": "greet b"}"#),
        "a play var naming the loop variable renders per item: {text}"
    );
    assert!(
        text.contains(r#"ok: [alpha] => {"msg": "hello there"}"#),
        "a templated value read through hostvars renders: {text}"
    );
}

#[test]
fn vars_files_are_resolved_for_each_host() {
    let out = volant(&[
        "playbook",
        "-i",
        &fixture("pervars/inventory.ini"),
        &fixture("pervars/site.yml"),
    ]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains(r#"ok: [one] => {"msg": "one is red"}"#)
            && text.contains(r#"ok: [two] => {"msg": "two is blue"}"#),
        "each host reads the file its own variables name: {text}"
    );
}

#[test]
fn each_playbook_resolves_paths_against_its_own_directory() {
    let out = volant(&[
        "playbook",
        &fixture("multi/first.yml"),
        &fixture("multi/sub/second.yml"),
    ]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains(r#"ok: [localhost] => {"msg": "kept here"}"#),
        "the second playbook reads its own vars_files and keeps the first one's facts: {text}"
    );
}

#[test]
fn a_failed_host_leaves_the_following_plays() {
    let out = volant(&[
        "playbook",
        "-i",
        &fixture("vars/inventory.ini"),
        &fixture("two-plays.yml"),
    ]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2), "{text}");
    let survivors = text
        .split("TASK [Survivors only]")
        .nth(1)
        .expect("second play ran");
    assert!(survivors.contains("changed: [alpha]"), "{text}");
    assert!(
        !survivors.contains("[beta]"),
        "beta must be gone from the second play: {text}"
    );
    assert!(
        text.contains(
            "beta                       : ok=0    changed=0    unreachable=0    failed=1"
        ),
        "{text}"
    );
}

#[test]
fn a_deferred_render_error_does_not_resurrect_a_host_that_already_failed() {
    let out = volant(&[
        "playbook",
        "-i",
        &fixture("vars/inventory.ini"),
        &fixture("deferred-error-after-failure.yml"),
    ]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2), "{text}");
    let survivors = text
        .split("TASK [Survivors only]")
        .nth(1)
        .expect("second play ran");
    assert!(survivors.contains("changed: [alpha]"), "{text}");
    assert!(
        !survivors.contains("[beta]"),
        "beta must be gone from the second play: {text}"
    );
    assert!(
        text.contains(
            "beta                       : ok=0    changed=0    unreachable=0    failed=1"
        ),
        "{text}"
    );
}

#[test]
fn extra_vars_and_limit_apply() {
    let out = volant(&[
        "playbook",
        "-i",
        &fixture("cfg/hosts.ini"),
        "-e",
        "colour=blue",
        "-l",
        "two",
        &fixture("cfg/site.yml"),
    ]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains(r#"ok: [two] => {"msg": "two blue"#), "{text}");
    assert!(!text.contains("[one]"), "limit must exclude one: {text}");
    let reference = include_str!("golden/ANSIBLE_VERSION").trim();
    assert!(
        text.contains(&format!("blue {reference} 2\"")),
        "ansible_version comes from the reference file: {text}"
    );
}

#[test]
fn ansible_cfg_supplies_the_inventory_when_none_is_given() {
    let out = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(["playbook", &fixture("cfg/site.yml")])
        .env("NO_COLOR", "1")
        .env("ANSIBLE_CONFIG", fixture("cfg/ansible.cfg"))
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("ok: [one]") && text.contains("ok: [two]"),
        "{text}"
    );
}

#[test]
fn a_limit_that_matches_nothing_is_an_error() {
    let out = volant(&[
        "playbook",
        "-i",
        &fixture("cfg/hosts.ini"),
        "-l",
        "nobody",
        &fixture("cfg/site.yml"),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no hosts"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A `--limit` naming a host that is not there narrows the run and says so, rather than
/// narrowing it silently. Measured against the reference: `-l web1,web-typo` prints
/// `[WARNING]: Could not match supplied host pattern, ignoring: web-typo` and then runs on
/// `web1`, exit 0. The wording is the reference's, word for word, and it is the same line a
/// play's own `hosts` already printed for an unmatched pattern.
#[test]
fn a_limit_naming_a_host_that_is_not_there_warns_and_runs_the_rest() {
    let out = volant(&[
        "playbook",
        "-i",
        &fixture("cfg/hosts.ini"),
        "-l",
        "one,not-a-host",
        &fixture("cfg/site.yml"),
    ]);
    let text = String::from_utf8(out.stdout).unwrap();
    let err = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}\n{err}");
    assert!(
        format!("{text}{err}")
            .contains("Could not match supplied host pattern, ignoring: not-a-host"),
        "the mistyped name is named: {text}\n{err}"
    );
    assert!(
        text.contains("ok: [one]") && !text.contains("ok: [two]"),
        "the limit still applies: {text}"
    );
}

/// A playbook that is there and does not parse exits 4, not 1. Measured against the reference:
/// a YAML error, an unknown play keyword, an unknown module and `a playbook must be a list of
/// plays` all exit 4, while a playbook file that is not there exits 1 (covered above).
#[test]
fn a_playbook_that_does_not_parse_exits_4() {
    let dir = std::env::temp_dir().join(format!("volant-parse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cases = [
        ("yaml.yml", "- hosts: all\n  tasks: [unclosed\n"),
        (
            "keyword.yml",
            "- hosts: all\n  strategy: free\n  tasks: []\n",
        ),
        ("not-a-list.yml", "hosts: all\n"),
    ];
    for (name, body) in cases {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        let out = volant(&["playbook", &path.display().to_string()]);
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        assert_eq!(out.status.code(), Some(4), "{name}: {err}");
        assert!(
            String::from_utf8_lossy(&out.stdout).is_empty(),
            "{name}: nothing runs"
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A configuration file that is there and cannot be read is ignored, and the run goes on.
/// That is the reference's own behaviour, measured against ansible-core 2.19.12: an
/// `ansible.cfg` at mode 000 changes nothing there and the playbook exits **0**, silently.
/// This used to exit 2, which refused playbooks the reference runs.
///
/// One divergence is kept on purpose: a `[WARNING]` naming the file. A configuration the
/// operator wrote and the process cannot open is worth a line, and a warning changes no exit
/// code, so nothing that reads the code sees a difference.
///
/// What would make this red: the refusal coming back (exit 2 and no play), or the warning
/// disappearing so an unreadable file becomes indistinguishable from no file at all.
///
/// The unreadable file here is one that is not text, because that fails for `root` too and this
/// suite has to give the same answer whoever runs it; a file at mode 000 takes the same path.
#[test]
fn an_unreadable_ansible_cfg_is_ignored_with_a_warning() {
    let dir = std::env::temp_dir().join(format!("volant-badcfg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("ansible.cfg");
    std::fs::write(&cfg, b"[defaults]\ninventory = ./hosts.ini\n# \xff\xfe\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(["playbook", &fixture("cfg/site.yml")])
        .env("NO_COLOR", "1")
        .env("ANSIBLE_CONFIG", cfg.display().to_string())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(
        out.status.code(),
        Some(0),
        "the reference runs the playbook and exits 0: {text}\n{err}"
    );
    assert!(
        err.contains("[WARNING]") && err.contains("ansible.cfg"),
        "the file it could not read is named: {text}\n{err}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `[tags] run`, `ANSIBLE_RUN_TAGS` and `--tags` are three sources of one list, and the command
/// line **adds** to whichever of the other two was read rather than replacing it.
///
/// Measured on ansible-core 2.19.12: with `[tags] run = x` in `ansible.cfg`, `--tags y` lists
/// the `x` task **and** the `y` task. The option's own default is that list and the option
/// appends to its default, so a command line can only ever widen what the file asked for. An
/// engine that took the command line as a replacement would run less than the reference does
/// from the same two files, silently.
///
/// What would make this red: either source dropped, which selects only the other one's tasks;
/// or the command line replacing rather than adding, which loses the `x` task here.
#[test]
fn the_three_sources_of_a_tag_list_add_up() {
    let dir = std::env::temp_dir().join(format!("volant-cfgtags-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("ansible.cfg");
    std::fs::write(&cfg, "[tags]\nrun = x\n").unwrap();
    let listing = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/listing");
    let list = |run_tags: Option<&str>, args: &[&str]| -> String {
        let mut command = Command::new(env!("CARGO_BIN_EXE_volant"));
        command
            .args(["playbook", "-i", "inv.ini", "--list-tasks"])
            .args(args)
            .arg("tags.yml")
            .current_dir(&listing)
            .env("NO_COLOR", "1")
            .env("ANSIBLE_CONFIG", cfg.display().to_string())
            .env_remove("ANSIBLE_RUN_TAGS")
            .env_remove("ANSIBLE_SKIP_TAGS");
        if let Some(tags) = run_tags {
            command.env("ANSIBLE_RUN_TAGS", tags);
        }
        String::from_utf8_lossy(&command.output().unwrap().stdout).to_string()
    };
    let from_file = list(None, &[]);
    assert!(
        from_file.contains("\n      a\t") && !from_file.contains("\n      b\t"),
        "the file's own list is read: {from_file}"
    );
    let both = list(None, &["--tags", "y"]);
    assert!(
        both.contains("\n      a\t") && both.contains("\n      b\t"),
        "the command line adds to the file's list, it does not replace it: {both}"
    );
    // The environment replaces the file, and the command line then adds to the environment.
    let from_env = list(Some("y"), &[]);
    assert!(
        from_env.contains("\n      b\t") && !from_env.contains("\n      a\t"),
        "the environment replaces the file's list: {from_env}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_unknown_connection_makes_that_host_unreachable_not_the_run() {
    let out = volant(&[
        "playbook",
        "-i",
        &fixture("bad-connection.ini"),
        &fixture("all-fail.yml"),
    ]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("fatal: [bad]: UNREACHABLE!"), "{text}");
    assert!(text.contains("carrier_pigeon"), "{text}");
    assert!(
        text.contains("PLAY RECAP"),
        "the run must finish with a recap: {text}"
    );
    assert_eq!(
        out.status.code(),
        Some(6),
        "failed good host (2) and unreachable bad host (4): {text}"
    );
}

#[test]
fn the_agent_connection_survives_across_plays() {
    let out = volant(&["playbook", &fixture("reuse.yml")]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains(r#""msg": "same""#),
        "the shell's parent must be the same agent in both plays: {text}"
    );
}

/// Six one-second sleeps, run six at a time and then one at a time. The bounds are absolute
/// rather than a ratio between the two durations: comparing them made the noisier of the two
/// the denominator, so a loaded runner that took 2.4 s for the wide run failed a required CI
/// job with a correct implementation. `narrow >= 5s` cannot be reached by an implementation
/// that ignores `-f 1`, and `wide < 3s` cannot be reached by one that serialises everything.
#[test]
fn forks_bounds_the_hosts_running_at_once() {
    let dir = std::env::temp_dir().join(format!("volant-forks-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let inv: String = (1..=6)
        .map(|i| format!("h{i} ansible_connection=local\n"))
        .collect();
    std::fs::write(dir.join("inv.ini"), inv).unwrap();
    std::fs::write(
        dir.join("sleep.yml"),
        "- hosts: all\n  gather_facts: false\n  tasks:\n    - shell: sleep 1\n",
    )
    .unwrap();
    let inv_path = dir.join("inv.ini").display().to_string();
    let pb = dir.join("sleep.yml").display().to_string();
    let started = std::time::Instant::now();
    let out = volant(&["playbook", "-i", &inv_path, "-f", "6", &pb]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let wide = started.elapsed();
    let started = std::time::Instant::now();
    let out = volant(&["playbook", "-i", &inv_path, "-f", "1", &pb]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let narrow = started.elapsed();
    assert!(
        narrow.as_secs_f64() >= 5.0,
        "forks=1 must serialise six one-second sleeps (narrow {narrow:?})"
    );
    assert!(
        wide.as_secs_f64() < 3.0,
        "forks=6 must run six one-second sleeps together (wide {wide:?})"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn zero_forks_is_refused() {
    let out = volant(&["playbook", "-f", "0", &fixture("site.yml")]);
    // Exit 2, matching the reference and matching clap's own exit code for `-f abc` or `-f -1`
    // on this same flag today; kept at 1 would put a refused `0` on the only remaining flag
    // value clap still hands off to this check, split three ways across two codes.
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("The number of processes (--forks) must be >= 1"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A `forks` the file writes but nobody can read refuses the run, with the reference's own
/// code. Measured against ansible-core 2.19.12: `forks = many` in `ansible.cfg` stops with
/// `ERROR: Config 'DEFAULT_FORKS' from '<path>' has an invalid value: Invalid value provided
/// for 'integer': 'many'` and exit **5**, before any play header. This used to exit 2, which
/// is the code a refused `-f 0` gets - a different event that deserves a different code.
///
/// What would make this red: the file arm falling back to a default and running, or the
/// refusal carrying any code but 5. The fixture names an inventory, so a lost refusal shows up
/// as a successful run and not as an empty one.
#[test]
fn an_unparsable_forks_in_ansible_cfg_is_refused_with_the_reference_s_code() {
    let out = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(["playbook", &fixture("cfg/site.yml")])
        .env("NO_COLOR", "1")
        .env("ANSIBLE_CONFIG", fixture("cfg/bad-forks.cfg"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(5), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("Config 'DEFAULT_FORKS'")
            && stderr.contains("Invalid value provided for 'integer'"),
        "{stderr}"
    );
    assert!(!stdout.contains("PLAY"), "a refusal runs no play: {stdout}");
}

/// The reference reports the run's own `forks` through `ansible_forks`, whatever the host
/// count; a fixed five would have lied as soon as `-f` existed.
#[test]
fn ansible_forks_reports_the_run_setting() {
    let out = volant(&["playbook", "-f", "3", &fixture("forks-var.yml")]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains(r#""ansible_forks": 3"#), "{text}");
    let out = volant(&["playbook", &fixture("forks-var.yml")]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains(r#""ansible_forks": 5"#),
        "the default is five: {text}"
    );
}

/// The account these tests escalate from. Asked of the system rather than read from `USER`,
/// which a container or a service manager can leave unset while the account is perfectly real.
fn me() -> String {
    let out = Command::new("id").arg("-un").output().unwrap();
    assert!(out.status.success(), "'id -un' failed");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A directory holding one executable `sudo` that behaves the way `body` says, first on `PATH`.
fn fake_sudo(name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("volant-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sudo = dir.join("sudo");
    std::fs::write(&sudo, body).unwrap();
    std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

fn volant_with_path(args: &[&str], dir: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(args)
        .env("NO_COLOR", "1")
        .env_remove("COLUMNS")
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .output()
        .expect("volant runs")
}

/// Needs passwordless sudo for the current user, as on the development machine and on the CI
/// runner. The second task drops back to the invoking account, so the same run proves both
/// that escalation happened and that it did not leak into the task that declined it: a
/// `become` that quietly did nothing would print that account twice.
#[test]
fn become_switches_user_and_back() {
    let out = volant(&["playbook", &fixture("become.yml")]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains(&format!(r#""msg": "root then {}""#, me())),
        "{text}"
    );
}

/// A `sudo` that wants a password is a task that failed, never a host that could not be
/// reached: the connection worked, and the run has to exit 2 rather than 4.
#[test]
fn a_missing_sudo_password_fails_the_task_not_the_host() {
    let dir = fake_sudo(
        "fakesudo",
        "#!/bin/sh\necho 'sudo: a password is required' >&2\nexit 1\n",
    );
    let out = volant_with_path(&["playbook", &fixture("become.yml")], &dir);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("fatal: [localhost]: FAILED!")
            && text.contains(volant::transport::MISSING_SUDO_PASSWORD),
        "{text}"
    );
    assert!(
        !text.contains("UNREACHABLE"),
        "escalation failure is a task failure: {text}"
    );
    assert_eq!(out.status.code(), Some(2), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The same host, the same connection, a password that `sudo` rejects. The two messages have
/// to be told apart, because the operator's next move differs.
#[test]
fn a_rejected_sudo_password_is_named_as_such() {
    let dir = fake_sudo(
        "badpassword",
        "#!/bin/sh\ncat > /dev/null\necho 'sudo: Sorry, try again.' >&2\nexit 1\n",
    );
    let out = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(["playbook", "-K", &fixture("become.yml")])
        .env("NO_COLOR", "1")
        .env_remove("COLUMNS")
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map(|mut child| {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"not-the-password\n")
                .unwrap();
            child.wait_with_output().unwrap()
        })
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains(volant::transport::INCORRECT_SUDO_PASSWORD),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!text.contains("UNREACHABLE"), "{text}");
    assert_eq!(out.status.code(), Some(2), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A host with no `sudo` at all: still a failed task, and the shell's own words rather than a
/// guess about passwords.
#[test]
fn a_host_without_sudo_fails_the_task_with_the_shells_words() {
    let dir = std::env::temp_dir().join(format!("volant-nosudo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // An empty directory as the whole PATH: nothing named `sudo` can be found from here.
    let out = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(["playbook", &fixture("become.yml")])
        .env("NO_COLOR", "1")
        .env_remove("COLUMNS")
        .env("PATH", dir.display().to_string())
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("fatal: [localhost]: FAILED!"), "{text}");
    assert!(
        text.contains("sudo"),
        "the message names the program: {text}"
    );
    assert!(!text.contains("UNREACHABLE"), "{text}");
    assert_eq!(out.status.code(), Some(2), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `become_method` other than `sudo` stops the run before anything executes, with the method
/// named. `sudo` is not silently substituted for the program the playbook asked for.
///
/// Exit 2, measured: the reference reads the keyword happily, finds no plugin for the method and
/// fails the task that would have used it. Volant refuses earlier, so nothing runs and there is
/// no recap, but the code an operator's script reads is the same one.
#[test]
fn unsupported_become_methods_are_refused_by_name() {
    let dir = std::env::temp_dir().join(format!("volant-su-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("su.yml");
    std::fs::write(
        &path,
        "- hosts: localhost\n  gather_facts: false\n  become: true\n  become_method: su\n  tasks:\n    - command: true\n",
    )
    .unwrap();
    let out = volant(&["playbook", &path.display().to_string()]);
    assert_eq!(out.status.code(), Some(2));
    let text = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        text.contains("su") && text.contains("not supported"),
        "{text}"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).is_empty(),
        "nothing runs: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let out = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(["playbook", &fixture("become.yml")])
        .env("NO_COLOR", "1")
        .env("ANSIBLE_BECOME_METHOD", "doas")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("doas"),
        "the environment variable is refused by name too: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The same environment against a playbook that escalates nowhere: the method is never used,
    // so there is nothing to refuse. Refusing here would abort every run on a machine whose
    // operator set the variable for something else entirely.
    let plain = dir.join("plain.yml");
    std::fs::write(
        &plain,
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - debug:\n        msg: nothing escalates here\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(["playbook", &plain.display().to_string()])
        .env("NO_COLOR", "1")
        .env("ANSIBLE_BECOME_METHOD", "doas")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "a run that never escalates is unaffected: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("nothing escalates here"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Runs `volant` with `dir` first on `PATH` and `password` on stdin, the way `-K` reads it.
fn volant_with_password(args: &[&str], dir: &std::path::Path, password: &str) -> Output {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(args)
        .env("NO_COLOR", "1")
        .env_remove("COLUMNS")
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("volant runs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(password.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// The only path the password itself travels: `sudo -k -S` reads it off the link's stdin, ahead
/// of the first protocol frame. The fake `sudo` here consumes exactly one line and then runs the
/// command it was given, which is what a real `sudo -k -S` does, so the run exercises the whole
/// sequence for real: the escalation check, the preamble on the link, the handshake and a batch.
/// Left on the pipe, that line would be read by the agent as its first frame header and the
/// handshake would fail, which is the failure this covers.
///
/// The answer the fake `sudo` accepts is built from this process's own id rather than written
/// as a literal, so two runs never share one and a leftover directory from an earlier run
/// cannot satisfy this one.
#[test]
fn a_correct_sudo_password_is_consumed_before_the_first_frame() {
    let expected = format!("only-this-run-{}", std::process::id());
    let dir = fake_sudo(
        "goodpassword",
        &format!(
            "#!/bin/sh\n\
             IFS= read -r given\n\
             [ \"$given\" = {expected} ] || {{ echo 'sudo: Sorry, try again.' >&2; exit 1; }}\n\
             while [ $# -gt 0 ] && [ \"$1\" != -- ]; do shift; done\n\
             shift\n\
             exec \"$@\"\n"
        ),
    );
    let out = volant_with_password(
        &["playbook", "-K", &fixture("become-password.yml")],
        &dir,
        &format!("{expected}\n"),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains(r#""msg": "escalated""#), "{text}");
    assert!(
        !text.contains(&expected),
        "the password never reaches the output: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `sudo` that needs no authentication at all - a `NOPASSWD` rule - never reads stdin,
/// whatever flags it is given: `-k` invalidates a cached authentication, and there is nothing
/// cached to invalidate. So the presence of a password is no reason to write one. Written
/// anyway, the line stays on the pipe and the agent reads it as the first bytes of its first
/// frame; the escalation check still passes, and the run then dies as `UNREACHABLE` with no
/// answer from the agent, which is exactly the unexplained failure this guards against.
///
/// The fake `sudo` here never reads a line, so a run that survives it is a run that wrote
/// nothing. The `-K` answer is built from this process's own id rather than written as a
/// literal, so two runs never share one.
#[test]
fn a_sudo_that_reads_no_password_is_never_written_one() {
    let dir = fake_sudo(
        "nopasswd",
        "#!/bin/sh\n\
         while [ $# -gt 0 ] && [ \"$1\" != -- ]; do shift; done\n\
         shift\n\
         exec \"$@\"\n",
    );
    let answer = format!("only-this-run-{}", std::process::id());
    let out = volant_with_password(
        &["playbook", "-K", &fixture("become-password.yml")],
        &dir,
        &format!("{answer}\n"),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains(r#""msg": "escalated""#),
        "the agent's first frame is the handshake, not the password: {text}"
    );
    assert!(!text.contains("UNREACHABLE"), "{text}");
    assert!(
        !text.contains(&answer) && !String::from_utf8_lossy(&out.stderr).contains(&answer),
        "the password never reaches the output: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Measured against the reference: `beta` reads the stamp `alpha` registered at the previous
/// task, on every run, and both hosts see each other in the play's live list. Ten runs, because
/// a barrier that does nothing passes this once by luck.
#[test]
fn a_host_reading_hostvars_waits_for_the_others() {
    for _ in 0..10 {
        let out = volant_within(
            &[
                "playbook",
                "-i",
                &fixture("vars/inventory.ini"),
                &fixture("hostvars-barrier.yml"),
            ],
            std::time::Duration::from_secs(20),
        );
        let text = String::from_utf8(out.stdout).unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{text}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            text.contains(r#"ok: [beta] => {"msg": "stamped-alpha"}"#),
            "{text}"
        );
        assert!(
            text.contains(r#"ok: [alpha] => {"msg": "alpha,beta of alpha,beta"}"#)
                && text.contains(r#"ok: [beta] => {"msg": "alpha,beta of alpha,beta"}"#),
            "both hosts are still in the play: {text}"
        );
    }
}

/// `ansible_play_hosts` follows the hosts still in the play while `ansible_play_hosts_all` keeps
/// the list it started with, as the reference does.
#[test]
fn play_hosts_shrink_when_a_host_fails_but_all_stays() {
    let dir = std::env::temp_dir().join(format!("volant-live-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("live.yml"),
        "- hosts: web\n  gather_facts: false\n  tasks:\n    - command: \"{{ (inventory_hostname == 'alpha') | ternary('false', 'true') }}\"\n    - debug:\n        msg: \"{{ ansible_play_hosts | join(',') }} of {{ ansible_play_hosts_all | join(',') }}\"\n",
    )
    .unwrap();
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("vars/inventory.ini"),
            &dir.join("live.yml").display().to_string(),
        ],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains(r#"ok: [beta] => {"msg": "beta of alpha,beta"}"#),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The failure path the barrier has to survive: `beta` waits for a host that is dying, so the
/// live set shrinks under it and the wait ends instead of outliving `alpha`.
#[test]
fn a_waiting_host_is_released_when_the_others_die() {
    let dir = std::env::temp_dir().join(format!("volant-release-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("die.yml"),
        "- hosts: web\n  gather_facts: false\n  tasks:\n    - command: \"{{ (inventory_hostname == 'alpha') | ternary('false', 'true') }}\"\n    - debug:\n        msg: \"{{ hostvars['beta'].inventory_hostname }}\"\n",
    )
    .unwrap();
    let started = std::time::Instant::now();
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("vars/inventory.ini"),
            &dir.join("die.yml").display().to_string(),
        ],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains(r#"ok: [beta] => {"msg": "beta"}"#), "{text}");
    assert!(
        started.elapsed().as_secs() < 5,
        "beta must not wait for a dead alpha"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A host that fails still reports the task it failed on, so the test above is released by the
/// play's progress rather than by the live set shrinking. This one removes that: `alpha` never
/// reaches the first task at all, so nothing but the shrinking live set can end `beta`'s wait.
#[test]
fn a_waiting_host_is_released_when_the_others_never_report() {
    let dir = std::env::temp_dir().join(format!("volant-silent-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("hosts.ini"),
        "[web]\nalpha ansible_connection=carrier_pigeon\nbeta ansible_connection=local\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("silent.yml"),
        "- hosts: web\n  gather_facts: false\n  tasks:\n    - command: \"true\"\n    - debug:\n        msg: \"{{ hostvars['beta'].inventory_hostname }}\"\n",
    )
    .unwrap();
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &dir.join("hosts.ini").display().to_string(),
            &dir.join("silent.yml").display().to_string(),
        ],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("fatal: [alpha]: UNREACHABLE!"), "{text}");
    assert!(text.contains(r#"ok: [beta] => {"msg": "beta"}"#), "{text}");
    assert_eq!(out.status.code(), Some(4), "unreachable alpha: {text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The lines one task printed: everything between its header and the next banner.
fn section<'a>(text: &'a str, task: &str) -> &'a str {
    banner_section(text, &format!("TASK [{task}]"))
}

/// The same, for a handler: its banner reads `RUNNING HANDLER [name]`.
fn handler_section<'a>(text: &'a str, name: &'a str) -> &'a str {
    banner_section(text, &format!("RUNNING HANDLER [{name}]"))
}

fn banner_section<'a>(text: &'a str, banner: &str) -> &'a str {
    let after = text
        .split_once(banner)
        .unwrap_or_else(|| panic!("no {banner} in:\n{text}"))
        .1;
    match after
        .find("\nTASK [")
        .into_iter()
        .chain(after.find("\nRUNNING HANDLER ["))
        .chain(after.find("\nPLAY "))
        .min()
    {
        Some(end) => &after[..end],
        None => after,
    }
}

/// Measured against ansible-core 2.19.12: a loop over an empty list prints
/// `skipping: [localhost]`, never `ok`, and registers
/// `{"changed": false, "results": [], "skipped": true, "skipped_reason": "No items in the list"}`.
#[test]
fn an_empty_loop_is_skipped_and_registers_no_items() {
    let out = volant(&["playbook", &fixture("loops-edge.yml")]);
    let text = String::from_utf8(out.stdout).unwrap();
    let empty = section(&text, "An empty loop");
    assert!(
        empty.contains("skipping: [localhost]"),
        "an empty loop is skipped: {text}"
    );
    assert!(
        !empty.contains("ok: [localhost]") && !empty.contains("changed: [localhost]"),
        "an empty loop must not report as ok: {text}"
    );
    assert!(
        section(&text, "What the empty loop registered")
            .contains(r#"ok: [localhost] => {"msg": "reason=No items in the list items=0"}"#),
        "the registered value carries the reference's reason and no items: {text}"
    );
    assert!(
        section(&text, "The empty loop registered a skip")
            .contains(r#"ok: [localhost] => {"msg": "the empty loop was skipped"}"#),
        "`when: empty.skipped` holds: {text}"
    );
}

/// Measured against the reference: every item of a loop runs, the one that failed and the ones
/// behind it alike, whether or not `ignore_errors` is on the task. The reference prints no
/// aggregate line for the task, only the items and then `...ignoring` where the failure was
/// swallowed, and its recap for this playbook reads
/// `ok=8 changed=2 unreachable=0 failed=1 skipped=1 rescued=0 ignored=1`.
#[test]
fn a_loop_runs_every_item_after_one_fails() {
    let out = volant(&["playbook", &fixture("loops-edge.yml")]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2), "{text}");

    let hard = section(&text, "A hard loop failure");
    let failed = hard
        .find("failed: [localhost] (item=false)")
        .unwrap_or_else(|| panic!("the second item must fail: {text}"));
    let last = hard
        .rfind("changed: [localhost] => (item=true)")
        .unwrap_or_else(|| panic!("no successful item: {text}"));
    assert!(
        last > failed,
        "the third item must run after the second one failed: {text}"
    );
    assert_eq!(
        hard.matches("changed: [localhost] => (item=true)").count(),
        2,
        "both successful items run: {text}"
    );
    assert!(
        !hard.contains("fatal: [localhost]"),
        "the reference prints no aggregate line for a loop: {text}"
    );
    assert!(
        !text.contains("Never reached"),
        "a loop that failed without ignore_errors still stops the host: {text}"
    );

    let ignored = section(&text, "A loop whose second item fails");
    assert_eq!(
        ignored
            .matches("changed: [localhost] => (item=true)")
            .count(),
        2,
        "ignore_errors changes nothing about the items that run: {text}"
    );
    assert!(
        ignored.trim_end().ends_with("...ignoring") && !ignored.contains("fatal: [localhost]"),
        "the swallowed failure says so with no aggregate line: {text}"
    );

    let cancelled = section(&text, "A loop whose failure a condition cancels");
    assert_eq!(
        cancelled.matches("changed: [localhost] => (item=").count(),
        3,
        "failed_when clears every item's failure: {text}"
    );
    assert!(
        !cancelled.contains("...ignoring") && !cancelled.contains("failed: [localhost]"),
        "nothing was ignored, the condition spoke: {text}"
    );

    assert!(
        text.contains(
            "localhost                  : ok=8    changed=2    unreachable=0    failed=1    skipped=1    rescued=0    ignored=1"
        ),
        "the recap matches the reference's counters: {text}"
    );
}

/// Measured against the reference: a registered `debug` stores `changed: false` and
/// `failed: false` and no `skipped`, so `when: reg.changed is defined` and `when: not
/// reg.changed` both hold instead of raising an undefined variable.
#[test]
fn a_registered_debug_carries_changed_and_failed() {
    let out = volant(&["playbook", &fixture("loops-edge.yml")]);
    let text = String::from_utf8(out.stdout).unwrap();
    for msg in [
        "changed is defined",
        "not changed",
        "failed is defined and skipped is not",
    ] {
        assert!(
            text.contains(&format!(r#"ok: [localhost] => {{"msg": "{msg}"}}"#)),
            "`{msg}` must be reached: {text}"
        );
    }
    assert!(
        !text.contains("undefined variable"),
        "nothing about the registered debug is undefined: {text}"
    );
    assert!(
        text.contains(r#"ok: [localhost] => {"msg": "hello"}"#),
        "the debug itself still prints only its message: {text}"
    );
}

/// Measured against the reference: an undefined variable is the only `vars_files` template
/// failure it forgives. `"{{ 1 + [] }}.yml"` gives `[ERROR]: Error rendering template:
/// unsupported operand type(s) for +: 'int' and 'list'`, `"{{ nope | badfilter }}.yml"` gives
/// `[ERROR]: Syntax error in template: No filter named 'badfilter'.` and an unclosed
/// `"{{ unclosed "` gives `[ERROR]: Syntax error in template: unexpected end of template,
/// expected 'end of print statement'.` — each of the three on stderr with no `PLAY RECAP` and
/// exit 1. So the run must fail, and it must not blame an undefined variable.
#[test]
fn a_broken_vars_files_template_fails_the_run() {
    for case in ["illegal-operation", "bad-filter", "unclosed"] {
        let out = volant(&[
            "playbook",
            "-i",
            &fixture("vars/inventory.ini"),
            &fixture(&format!("vars-file-errors/{case}.yml")),
        ]);
        let text = String::from_utf8(out.stdout).unwrap();
        let err = String::from_utf8(out.stderr).unwrap();
        assert_eq!(out.status.code(), Some(1), "{case}: {text}\n{err}");
        assert!(
            err.contains("ERROR! rendering vars_files entry"),
            "{case}: the run says which entry it could not render: {err}"
        );
        assert!(
            !err.contains("undefined variable") && !text.contains("undefined variable"),
            "{case}: a broken template is not an undefined variable: {err}"
        );
        assert!(
            !text.contains("PLAY RECAP") && !text.contains("Never reached"),
            "{case}: the play does not run: {text}"
        );
    }
}

/// Measured against the reference: a `vars_files` entry that renders to something other than a
/// string prints "Invalid `vars_files` value of type 'int'. A `vars_files` value should either be
/// a string or list of strings." on stderr, with no recap, and exits 4. The message is matched
/// word for word and so is the code, which is the reference's own for a playbook it cannot make
/// sense of. A `vars_files` entry whose *template* fails is a different case and exits 1 there,
/// also measured; the test above covers it.
#[test]
fn a_vars_files_entry_that_is_not_a_string_is_refused() {
    let out = volant(&[
        "playbook",
        "-i",
        &fixture("vars/inventory.ini"),
        &fixture("vars-file-errors/not-a-string.yml"),
    ]);
    let text = String::from_utf8(out.stdout).unwrap();
    let err = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(4), "{text}\n{err}");
    assert!(
        err.contains(
            "Invalid `vars_files` value of type 'int'. A `vars_files` value should either be a \
             string or list of strings."
        ),
        "the reference's wording, verbatim: {err}"
    );
    assert!(
        !text.contains("PLAY RECAP"),
        "the play does not run: {text}"
    );
}

/// Measured against the reference: a `vars_files` entry that names no file is skipped without a
/// word and the play runs; an entry whose template has no value is skipped with one warning.
/// Exit code 0 on both counts, and a recap for every host.
#[test]
fn a_missing_vars_files_entry_is_skipped() {
    let out = volant(&[
        "playbook",
        "-i",
        &fixture("vars/inventory.ini"),
        &fixture("bad-vars-file.yml"),
    ]);
    // Both streams: the play's own output is on stdout and the warning is on stderr, where
    // ansible-playbook puts every warning it prints.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains(r#"ok: [alpha] => {"msg": "reached on alpha"}"#)
            && text.contains(r#"ok: [beta] => {"msg": "reached on beta"}"#),
        "both hosts run the play: {text}"
    );
    assert!(
        text.contains("[WARNING]: skipping vars_files item due to an undefined variable"),
        "the unresolvable entry is warned about: {text}"
    );
    assert_eq!(
        text.matches("skipping vars_files item").count(),
        1,
        "one warning for the entry, not one per host: {text}"
    );
    assert!(
        !text.contains("does-not-exist.yml"),
        "a missing file is skipped without a word: {text}"
    );
    assert!(
        text.contains("alpha                      : ok=1")
            && text.contains("beta                       : ok=1"),
        "the recap has a line per host: {text}"
    );
}

/// Measured against the reference: two playbook arguments print two `PLAY RECAP` blocks, and
/// the counters carry over rather than resetting, so the same playbook run twice reads `ok=1
/// changed=1` and then `ok=2 changed=2`.
#[test]
fn each_playbook_prints_its_own_recap() {
    let out = volant(&["playbook", &fixture("second.yml"), &fixture("second.yml")]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        text.matches("PLAY RECAP").count(),
        2,
        "one recap per playbook argument: {text}"
    );
    assert!(
        text.contains(
            "localhost                  : ok=1    changed=1    unreachable=0    failed=0    skipped=0    rescued=0    ignored=0"
        ),
        "the first recap counts the first playbook: {text}"
    );
    assert!(
        text.contains(
            "localhost                  : ok=2    changed=2    unreachable=0    failed=0    skipped=0    rescued=0    ignored=0"
        ),
        "the second recap carries the first one's counters: {text}"
    );
}

/// The bounded event channel at scale: seventy hosts, seventy forks, each sending several
/// events while the coordinator waits on the slowest one, and every one of them reaching the
/// recap. `volant_within` is what makes a wedge a failure here: an elapsed-time assertion
/// cannot fire on a run that never returns.
#[test]
fn seventy_hosts_finish_with_a_full_recap() {
    let dir = std::env::temp_dir().join(format!("volant-many-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let inv: String = (1..=70)
        .map(|i| format!("h{i:02} ansible_connection=local\n"))
        .collect();
    std::fs::write(dir.join("inv.ini"), inv).unwrap();
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &dir.join("inv.ini").display().to_string(),
            "-f",
            "70",
            &fixture("many-hosts.yml"),
        ],
        std::time::Duration::from_secs(60),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        text.matches("ok=2").count(),
        70,
        "every host must reach the recap: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A play written as a tree runs as one sequence, in playbook order, with every block's
/// keywords reaching the tasks under it and every `always` running on the way out.
///
/// `volant_within` rather than `volant`: a host that steps over a step without telling the
/// coordinator leaves its per-step loop waiting for a report that will never come, and a run
/// that never returns cannot be caught by an assertion on what it printed.
///
/// What would make this red: a step run out of order, a nested `always` left out of the walk,
/// or a block's `vars` not reaching the task inside it.
#[test]
fn a_play_of_nested_blocks_runs_as_one_sequence() {
    let out = volant_within(
        &["playbook", &fixture("blocks/happy.yml")],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let order: Vec<usize> = [
        "In the block",
        "Nested",
        "Nested cleanup",
        "Outer cleanup",
        "After the block",
    ]
    .iter()
    .map(|name| {
        text.find(&format!("TASK [{name}]"))
            .unwrap_or_else(|| panic!("{name} is missing from {text}"))
    })
    .collect();
    assert!(
        order.windows(2).all(|w| w[0] < w[1]),
        "the steps run in playbook order: {text}"
    );
    assert!(
        !text.contains("TASK [Around everything]"),
        "a block's own name is shown nowhere, measured on the reference: {text}"
    );
    assert!(
        text.contains("localhost                  : ok=5"),
        "five steps ran and every one of them is counted: {text}"
    );
}

/// A failure inside a nested block runs the `always` of every block around it, innermost
/// first, and then the host leaves the play.
///
/// Measured on ansible-core 2.19.12 on the same shape: the step after the failure is skipped,
/// both cleanups run, the step after the outer block never runs, and the recap reads
/// `ok=2 failed=1`.
///
/// What would make this red: a failure that leaves the play straight away, which skips the
/// cleanup the playbook wrote; or one that carries on through the body it was in.
#[test]
fn a_failure_runs_the_cleanups_around_it_and_stops_there() {
    let out = volant_within(
        &["playbook", &fixture("blocks/always.yml")],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2), "{text}");
    let inner = text
        .find("TASK [Inner cleanup]")
        .expect("the inner cleanup");
    let outer = text
        .find("TASK [Outer cleanup]")
        .expect("the outer cleanup");
    assert!(inner < outer, "innermost first: {text}");
    assert!(
        !text.contains("Never reached"),
        "nothing behind the failure runs, inside the block or after it: {text}"
    );
    assert!(
        text.contains(
            "localhost                  : ok=2    changed=2    unreachable=0    failed=1"
        ),
        "the two cleanups are counted and the failure is the only one: {text}"
    );
}

/// A failure whose cleanup is written as a block runs the whole cleanup.
///
/// Measured on ansible-core 2.19.12 on this shape: both cleanup tasks run, in order, and the
/// recap reads `ok=2 changed=2 failed=1` at exit 2.
///
/// What would make this red: a host reading the section it is draining off the step it is on.
/// The first cleanup task carries the nested block's own `Body`, so the host would take it for
/// the end of the cleanup, leave the play, and report a run that did less than the playbook
/// asked for.
#[test]
fn a_cleanup_written_as_a_block_runs_all_of_it() {
    let out = volant_within(
        &["playbook", &fixture("blocks/nested-cleanup.yml")],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2), "{text}");
    let one = text.find("TASK [Cleanup one]").expect("the first cleanup");
    let two = text.find("TASK [Cleanup two]").expect("the second cleanup");
    assert!(one < two, "the cleanup runs in order: {text}");
    assert!(
        !text.contains("Never reached"),
        "nothing behind the failure runs: {text}"
    );
    assert!(
        text.contains(
            "localhost                  : ok=2    changed=2    unreachable=0    failed=1"
        ),
        "both cleanups are counted and the failure is the only one: {text}"
    );
}

/// A failure a `rescue` takes is shown as a failure, counted as `rescued`, and the play goes
/// on for that host - while a host that failed nothing walks past the rescue without entering
/// it.
///
/// Measured on ansible-core 2.19.12 on this shape, two hosts, only `h1` failing: `h1` prints
/// `fatal: [h1]: FAILED!`, the rescue runs on `h1` alone, the `always` runs on both, the step
/// behind the block runs on both with `ansible_play_hosts` still two long, and the recap reads
/// `failed=0 rescued=1` for `h1` and `rescued=0` for `h2`, exit 0.
///
/// What would make this red: a rescue nobody enters (no `TASK [Rescue task]`); a body that
/// carries on under the failure (`Never reached on h1` showing a line for `h1`); a rescued
/// failure counted as `failed`, which exits 2 where the reference exits 0; a rescued host taken
/// out of the live set, which reads `hosts=1`; or `h2` walking into the rescue it has no
/// business in.
#[test]
fn a_failure_in_a_block_is_rescued_and_the_play_goes_on() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("blocks/inv.ini"),
            &fixture("blocks/rescue.yml"),
        ],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "a rescued failure is not a failed run: {text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("fatal: [h1]: FAILED!"),
        "a rescued task still says it failed: {text}"
    );
    let rescue = section(&text, "Rescue task");
    assert!(
        rescue.contains("Fails in the block / command / rc="),
        "the rescue reads the two variables the failure set: {text}"
    );
    assert!(
        !rescue.contains("[h2]"),
        "only the host that failed enters the rescue: {text}"
    );
    let never = section(&text, "Never reached on h1");
    assert!(
        never.contains("changed: [h2]") && !never.contains("[h1]"),
        "the body goes on for h2 and stops for h1: {text}"
    );
    assert!(
        section(&text, "After the block").contains("\"msg\": \"hosts=2\""),
        "a rescued host is still one of the play's hosts: {text}"
    );
    assert!(
        text.contains("h1                         : ok=4    changed=2    unreachable=0    failed=0    skipped=0    rescued=1"),
        "the failure is counted as rescued and nothing else: {text}"
    );
    assert!(
        text.contains("h2                         : ok=4    changed=3    unreachable=0    failed=0    skipped=1    rescued=0"),
        "the host that failed nothing steps over the rescue: {text}"
    );
}

/// A rescue that fails itself still runs the `always`, and then the host leaves the play.
///
/// Measured on ansible-core 2.19.12 on this shape: the `always` runs, the step behind the block
/// does not run for that host, and the recap reads `failed=1 rescued=1` - the body's rescue is
/// kept, the rescue's own failure is not rescued again - at exit 2. Measured with it: by the
/// time its `always` runs, the host is already out of `ansible_play_hosts`.
///
/// What would make this red: an `always` skipped after a failed rescue, which drops the cleanup
/// a playbook wrote for exactly this case; a rescue that rescues itself, which would loop; or a
/// host carrying on into the play after its rescue failed.
#[test]
fn a_failing_rescue_still_runs_always_and_the_host_leaves() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("blocks/inv.ini"),
            &fixture("blocks/rescue-fails.yml"),
        ],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2), "{text}");
    let always = section(&text, "Always after failed rescue");
    assert!(
        always.contains("[h1]") && always.contains("[h2]"),
        "the cleanup runs for the host on its way out as well: {text}"
    );
    assert!(
        always.contains("\"msg\": \"hosts=1\""),
        "the host whose rescue failed has already left the live set: {text}"
    );
    assert!(
        !section(&text, "Not reached on h1").contains("[h1]"),
        "the host leaves the play after its cleanup: {text}"
    );
    assert!(
        text.contains(
            "h1                         : ok=1    changed=0    unreachable=0    failed=1    skipped=0    rescued=1"
        ),
        "the body's rescue is kept and the rescue's own failure stands: {text}"
    );
}

/// A block whose `when` is false skips every task inside it, one line each.
///
/// Measured on ansible-core 2.19.12: the banner of each inner task is printed with a
/// `skipping: [h]` under it, nested blocks included, and the recap counts one `skipped` per
/// task rather than one per block.
///
/// What would make this red: a block skipped whole, which prints nothing and recaps
/// `skipped=0` - a playbook's tasks silently absent from the run and from the report.
#[test]
fn a_block_whose_when_is_false_skips_every_task_inside() {
    let out = volant_within(
        &["playbook", &fixture("blocks/when-false.yml")],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert_eq!(
        text.matches("skipping: [localhost]").count(),
        2,
        "one line per task inside the block, the nested one included: {text}"
    );
    assert!(
        text.contains("localhost                  : ok=0    changed=0    unreachable=0    failed=0    skipped=2"),
        "both tasks are counted as skipped: {text}"
    );
}

/// A host that cannot be reached is not rescued, and its `always` does not run either.
///
/// Measured on ansible-core 2.19.12: an unreachable host inside a block leaves the play at
/// once, the rescue does not take it and the `always` is not run, the recap reads
/// `unreachable=1 rescued=0` and the run exits 4. The trigger here is this engine's own - an
/// unknown connection plugin, which it reports as `UNREACHABLE` - and what is under test is
/// what a host leaving that way does to the sections around it.
///
/// What would make this red: an unreachable host routed through the failure path, which would
/// print `msg: rescued`, exit 2 instead of 4, and report a host as recovered when nothing ever
/// reached it.
#[test]
fn an_unreachable_host_is_not_rescued() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("blocks/unreachable.ini"),
            &fixture("blocks/unreachable.yml"),
        ],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(4), "{text}");
    assert!(text.contains("fatal: [h1]: UNREACHABLE!"), "{text}");
    assert!(
        !text.contains("TASK [Rescue task]") && !text.contains("TASK [Always task]"),
        "neither section runs for a host nothing reached: {text}"
    );
    assert!(
        text.contains(
            "h1                         : ok=0    changed=0    unreachable=1    failed=0    skipped=0    rescued=0"
        ),
        "an unreachable host is counted once, as unreachable: {text}"
    );
}

/// A host lost in the middle of a section says so: no rescue, no cleanup, and a recap that
/// names it.
///
/// The connection dies between two steps of a block's body, which is the one way a host can go
/// unreachable with work already behind it. Measured on ansible-core 2.19.12: an unreachable
/// host inside a block is not rescued and its `always` does not run either, and the run exits 4.
///
/// What would make this red - and it is the worst outcome this file guards against - a lost
/// host routed into the rescue, which would print `rescued`, run the cleanup on a host that is
/// not there, and exit 0 on a playbook that did half of what it says.
#[test]
fn a_host_lost_partway_through_a_block_gets_no_rescue_and_no_cleanup() {
    let out = volant_within(
        &["playbook", &fixture("blocks/agent-dies.yml")],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(4), "{text}");
    assert!(text.contains("fatal: [localhost]: UNREACHABLE!"), "{text}");
    for absent in [
        "TASK [Never reached]",
        "TASK [Rescue task]",
        "TASK [Always task]",
        "TASK [After the block]",
    ] {
        assert!(!text.contains(absent), "{absent} must not run: {text}");
    }
    assert!(
        text.contains(
            "localhost                  : ok=1    changed=1    unreachable=1    failed=0    skipped=0    rescued=0"
        ),
        "the step it did run is counted, and the loss is counted once: {text}"
    );
}

/// A failure `ignore_errors` swallows never reaches the rescue: the body carries on.
///
/// Measured on ansible-core 2.19.12: the task prints `fatal:` followed by `...ignoring`, the
/// next task of the body runs, the rescue does not, and the recap reads `ignored=1 rescued=0`
/// at exit 0.
///
/// What would make this red: the rescue path reading the failure before `ignore_errors` does,
/// which jumps into a recovery for a failure the playbook said to ignore and skips the rest of
/// the body on the way.
#[test]
fn ignore_errors_inside_a_block_does_not_enter_the_rescue() {
    let out = volant_within(
        &["playbook", &fixture("blocks/ignored.yml")],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("...ignoring"), "{text}");
    assert!(
        text.contains("TASK [Still in the body]"),
        "the body carries on under an ignored failure: {text}"
    );
    assert!(
        !text.contains("TASK [Never rescued]"),
        "an ignored failure is not a failure to recover from: {text}"
    );
    assert!(
        text.contains("localhost                  : ok=2    changed=1    unreachable=0    failed=0    skipped=0    rescued=0    ignored=1"),
        "counted as ignored, never as rescued: {text}"
    );
}

/// A `meta` shows one banner per live host and counts nothing.
///
/// Measured on ansible-core 2.19.12 with three hosts, one of them already out of the play:
/// two `TASK [meta]` banners in a row, nothing underneath either of them, and the recap
/// counting only the task that followed, `h0` reading `failed=1` and the other two `ok=1
/// skipped=1`.
///
/// What would make this red: a banner printed on the first report rather than on the first
/// event that shows something, which gives one banner for two hosts; a `meta` counted as a
/// task, which reads `ok=2`; or a host that has left the play still being shown one, which
/// gives three.
#[test]
fn a_meta_shows_one_banner_per_live_host_and_counts_nothing() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("blocks/three-hosts.ini"),
            &fixture("blocks/meta.yml"),
        ],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        text.matches("TASK [meta]").count(),
        2,
        "one banner per live host, and none for the host that has left: {text}"
    );
    assert_eq!(
        text.matches("h0                         : ok=0    changed=0    unreachable=0    failed=1")
            .count(),
        1,
        "the host that failed is out of the play: {text}"
    );
    assert_eq!(
        text.matches("h1                         : ok=1").count(),
        1,
        "the meta counts nothing, so only the task behind it does: {text}"
    );
    assert_eq!(
        text.matches("h2                         : ok=1").count(),
        1,
        "{text}"
    );
}

// The keyword tables, covered in both directions. Every row of `keywords::TASK_KEYWORDS`,
// `keywords::PLAY_KEYWORDS` and `keywords::LOOP_CONTROL_KEYWORDS` has to do what its `Support`
// claims: a `Runs` row changes something an operator can see, a `Preflight` row stops the run
// before anything is printed. The `Runs` list is walked from the tables themselves rather than
// written out here, so a row added without its proof fails the suite instead of slipping
// through - which is how a keyword would come to be accepted by the loader, waved past the
// pre-flight and then ignored, the failure this whole split exists to prevent.
//
// The `Preflight` half walks the same tables in `preflight.rs`'s own unit tests, in one
// process; only "nothing comes out before the refusal" needs a real run, and three fixtures
// below carry it.

use volant::keywords::{
    BLOCK_KEYWORDS, HANDLER_KEYWORDS, LOOP_CONTROL_KEYWORDS, PLAY_KEYWORDS, Support, TASK_KEYWORDS,
};

const PROBE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// A directory for generated probes, emptied first so a previous run cannot answer for this one.
fn probe_dir(kind: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("volant-probe-{kind}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a probe directory");
    dir
}

fn run_probe(
    dir: &std::path::Path,
    kw: &str,
    body: &str,
    extra: &[&str],
    path: Option<&Path>,
) -> (i32, String) {
    let file = dir.join(format!("{kw}.yml"));
    std::fs::write(&file, body).expect("the probe is written");
    let mut args: Vec<&str> = vec!["playbook"];
    args.extend_from_slice(extra);
    let shown = file.display().to_string();
    args.push(&shown);
    let out = volant_within_with_path(&args, PROBE_DEADLINE, path);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

/// Nothing comes out before a pre-flight refusal: no banner, no task header, no recap.
///
/// That a parked keyword is refused by its own name is a property of `parse` plus `check`, and
/// the unit test `every_preflight_keyword_is_refused_by_its_own_name` walks all four tables
/// for it in one process. Only "no output before the refusal" needs a real run, and it is the
/// same property for every row, so three fixtures carry it: one parked task keyword, one parked
/// block keyword, one parked play keyword. Walking the whole grammar here spawned a hundred
/// processes to re-prove in-process what one assertion already proves.
///
/// What would make this red: a refusal raised after the banner, by which time tasks may already
/// have run and the operator has a half-applied playbook to undo.
#[test]
fn a_preflight_refusal_lets_nothing_out_before_it() {
    let dir = probe_dir("preflight");
    let probes = [
        (
            "no_log",
            "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      command: echo hi\n      no_log: probe\n",
        ),
        (
            "run_once",
            "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      block:\n        - command: echo hi\n      run_once: probe\n",
        ),
        (
            "serial",
            "- hosts: localhost\n  gather_facts: false\n  serial: probe\n  tasks:\n    - name: Probe task\n      command: echo hi\n",
        ),
    ];
    for (kw, body) in probes {
        let (code, text) = run_probe(&dir, kw, body, &[], None);
        assert_eq!(code, 4, "{kw}: {text}");
        assert!(
            text.contains(&format!("keyword '{kw}' is not supported yet")),
            "{kw} must name itself: {text}"
        );
        assert!(
            !text.contains("PLAY ["),
            "{kw}: nothing runs before a pre-flight refusal: {text}"
        );
        assert!(
            !text.contains("PLAY RECAP"),
            "{kw}: a refusal before the first connection has nothing to recap: {text}"
        );
    }
    std::fs::remove_dir_all(&dir).expect("the probe directory is removed");
}

/// Every handler the tasks notified, run once each, in **definition** order, at the flush point
/// the playbook asked for and at the one that closes the section.
///
/// The whole of this is measured on ansible-core 2.19.12 with this fixture. `second handler` is
/// written first and notified second, and it runs first; `first handler` is notified twice and
/// runs once; `ok does not notify` came back `ok` and notifies nothing; `notify listen` reaches
/// two handlers through `listen: group`, one of which carries no `name` of that spelling, and
/// only h1 notified so only h1 has a line under those banners; the explicit `meta` shows one
/// `TASK [meta]` per live host with nothing under it; `after flush` notifies `second handler`
/// again and it runs a second time, at the flush that closes `tasks`, for h1 alone because h2
/// has failed by then. Recap `h1 ok=9 changed=4 skipped=1`, `h2 ok=6 failed=1 skipped=1`, exit 2.
///
/// What would make this red: handlers run in notification order (`first handler` first); a
/// handler run twice for one notification (`ok=10`); a task that came back `ok` notifying; a
/// `listen` name resolving to nothing; a host that never notified showing a line under a handler
/// banner; or a host that failed still running its handlers, which is what `--force-handlers` is
/// for and what the next test asks for instead.
#[test]
fn handlers_run_once_in_definition_order_after_the_tasks() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("handlers/inv.ini"),
            &fixture("handlers/handlers.yml"),
        ],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let banners: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("RUNNING HANDLER ["))
        .map(|l| l.split(']').next().unwrap_or_default())
        .collect();
    assert_eq!(
        banners,
        [
            "RUNNING HANDLER [second handler",
            "RUNNING HANDLER [first handler",
            "RUNNING HANDLER [listener",
            "RUNNING HANDLER [second handler",
        ],
        "definition order, and `second handler` once per notification: {text}"
    );
    assert!(
        !handler_section(&text, "listener").contains("[h2]"),
        "only the host that notified has a line: {text}"
    );
    assert_eq!(
        text.matches("TASK [meta]").count(),
        2,
        "one banner per live host for the explicit flush, with nothing under it: {text}"
    );
    let between = text
        .split_once("TASK [meta]")
        .and_then(|(_, rest)| rest.split_once("RUNNING HANDLER"))
        .map(|(before, _)| before)
        .unwrap_or_default();
    assert!(
        !between.contains("ok: ") && !between.contains("changed: "),
        "a flush shows no result line of its own: {text}"
    );
    assert!(
        text.contains(
            "h1                         : ok=9    changed=4    unreachable=0    failed=0    skipped=1"
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "h2                         : ok=6    changed=3    unreachable=0    failed=1    skipped=1"
        ),
        "a host that failed after notifying does not run its handlers: {text}"
    );
}

/// A task that changed nothing notifies nothing, and the handler shows nowhere.
///
/// Measured on this fixture: one line, `ok: [localhost]`, recap `ok=1` and no `RUNNING HANDLER`
/// anywhere. What would make this red: `notify` read without looking at whether the task changed
/// anything, which runs every handler of every playbook on every run.
#[test]
fn a_task_that_did_not_change_does_not_notify() {
    let out = volant_within(
        &["playbook", &fixture("handlers/unchanged.yml")],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(!text.contains("RUNNING HANDLER"), "{text}");
    assert!(
        text.contains(
            "localhost                  : ok=1    changed=0    unreachable=0    failed=0"
        ),
        "{text}"
    );
}

/// `--force-handlers` runs the handlers of a host that failed, and the failure still stands.
///
/// Measured on ansible-core 2.19.12 with the same fixture as above: the only difference is one
/// line, `ok: [h2]` under the last `RUNNING HANDLER [second handler]`, and `h2 ok=7` against
/// `ok=6`. The exit code is still 2 and `failed=1` is still there.
///
/// What would make this red: the flag read and the driver still leaving the play at the failure,
/// which is the shape of a keyword accepted and then ignored; or a forced host counted as
/// something other than failed, which would turn exit 2 into exit 0.
#[test]
fn force_handlers_runs_them_on_a_failed_host() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("handlers/inv.ini"),
            &fixture("handlers/handlers.yml"),
            "--force-handlers",
        ],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(
        text.contains(
            "h2                         : ok=7    changed=3    unreachable=0    failed=1    skipped=1"
        ),
        "the failed host ran the handler it notified and is still counted failed: {text}"
    );
}

/// A `notify` naming nothing stops the run with the reference's own sentence and exit 1.
///
/// Measured on ansible-core 2.19.12: that text, exit **1** and no recap. The one divergence is
/// the moment - the reference prints the notifying task's banner first and refuses while running
/// it, this refuses before the first connection, so nothing has run at all.
///
/// What would make this red: exit 4, which is what an unloadable playbook gets and what a
/// refusal raised without its own code would be; or the run going ahead with a `notify` nobody
/// answers, which is a playbook asking for work that never happens.
#[test]
fn an_unknown_handler_is_refused_with_exit_1() {
    let out = volant_within(
        &["playbook", &fixture("handlers/unknown.yml")],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let err = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(1), "{err}{text}");
    assert!(
        err.contains(
            "The requested handler 'nobody' was not found in either the main handlers list nor in the listening handlers list"
        ),
        "{err}"
    );
    assert!(!text.contains("PLAY RECAP"), "nothing ran: {text}");
}

/// A handler that fails fails its host, and the handlers behind it do not run.
///
/// Measured on ansible-core 2.19.12 on this fixture: `fatal:` under
/// `RUNNING HANDLER [bad handler]`, `good handler` nowhere, recap `ok=1 changed=1 failed=1`,
/// exit 2. Measured again with `--force-handlers`: `good handler` still does not run, so the
/// flag buys a failed host its handlers and not a failed handler its successors.
///
/// What would make this red: a handler whose failure goes unreported, which is a task that
/// failed and a run that exits 0.
#[test]
fn a_failing_handler_fails_the_host() {
    let out = volant_within(
        &["playbook", &fixture("handlers/failing.yml")],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(
        handler_section(&text, "bad handler").contains("fatal: [localhost]: FAILED!"),
        "{text}"
    );
    assert!(
        !text.contains("good handler"),
        "the host leaves, so the handler behind the failing one does not run: {text}"
    );
    assert!(
        text.contains(
            "localhost                  : ok=1    changed=1    unreachable=0    failed=1"
        ),
        "{text}"
    );
}

/// A handler is an ordinary task: it loops, it has a `when`, and a task in a `rescue` may notify
/// one.
///
/// Measured on ansible-core 2.19.12 on this fixture: `(item=1)` and `(item=2)` under
/// `RUNNING HANDLER [loopy]`, `skipping: [localhost]` under `[conditional]`, `rescued-notify`
/// under `[from rescue]`, recap `ok=4 changed=2 skipped=1 rescued=1`, exit 0.
///
/// What would make this red: a notification raised inside a rescue lost with the failure that
/// led to it, which is a handler a playbook asked for and never got.
#[test]
fn handlers_accept_loops_conditions_and_notifications_from_a_rescue() {
    let out = volant_within(
        &["playbook", &fixture("handlers/shapes.yml")],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    let loopy = handler_section(&text, "loopy");
    assert!(
        loopy.contains("(item=1)") && loopy.contains("(item=2)"),
        "{text}"
    );
    assert!(
        handler_section(&text, "conditional").contains("skipping: [localhost]"),
        "{text}"
    );
    assert!(
        handler_section(&text, "from rescue").contains("rescued-notify"),
        "a task in a rescue notifies like any other: {text}"
    );
    assert!(
        text.contains(
            "localhost                  : ok=4    changed=2    unreachable=0    failed=0    skipped=1    rescued=1"
        ),
        "{text}"
    );
}

/// A `meta: flush_handlers` written inside a block runs the handlers there, inside that block:
/// one that fails is taken by the block's own `rescue`, and the block's `always` still runs.
///
/// Measured on ansible-core 2.19.12 on this fixture: `TASK [meta]`, then
/// `RUNNING HANDLER [bad handler]` with a `fatal:` under it, then `the rescue`, then
/// `the cleanup`, then `after the block`; recap `ok=5 changed=2 rescued=1 failed=0`, exit 0.
///
/// This is the fixture the splice is written for. The handler steps are inserted **into** the
/// block's body, one past the flush, and every block range that ends there has to grow with
/// them: that is what the `>=` in `Compiled::splice` is for. What would make this red:
/// `range.start > at` instead of `>=`, which leaves this block's `rescue` starting at the first
/// spliced handler, so the failing handler is "rescued" into itself and the run never returns.
/// `volant_within` is what says so, since no assertion on printed output can catch a hang.
#[test]
fn a_flush_inside_a_block_runs_its_handlers_inside_that_block() {
    let out = volant_within(
        &["playbook", &fixture("handlers/flush-in-block.yml")],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        handler_section(&text, "bad handler").contains("fatal: [localhost]: FAILED!"),
        "{text}"
    );
    assert!(
        section(&text, "the rescue").contains("rescue-ran"),
        "the block's rescue takes the handler's failure: {text}"
    );
    assert!(
        section(&text, "the cleanup").contains("cleanup-ran")
            && section(&text, "after the block").contains("\"msg\": \"after\""),
        "{text}"
    );
    assert!(
        text.contains(
            "localhost                  : ok=5    changed=2    unreachable=0    failed=0    skipped=0    rescued=1"
        ),
        "{text}"
    );
}

/// A flush point one host runs and another steps over.
///
/// The flush is written in a `rescue` only `h1` enters. Measured on ansible-core 2.19.12 on this
/// fixture: one `TASK [meta]` banner, `RUNNING HANDLER [the handler]` with `ok: [h1]` alone under
/// it, then `after the block` for both hosts, then the handler again for `h2` alone at the flush
/// that closes `tasks` - `h2`'s own notification, which the rescue's flush never reached. Recap
/// `h1 ok=4 changed=1 rescued=1`, `h2 ok=3 changed=1 skipped=1`, exit 0.
///
/// This is the other half of the splice's safety, and the half the plan did not have. `h2` walks
/// from the block's body straight past the rescue, which means past a flush point, while `h1` is
/// still short of it - so `h2` has to stop there and wait for the handler steps to go in, exactly
/// as `h1` does after running it. Reading the list one step earlier would leave `h2` holding an
/// index into a list that has since grown underneath it.
///
/// What would make this red: a host that steps over a flush point without waiting for it. The
/// step behind the block reads `ansible_play_hosts`, so it is a barrier and `h2` cannot simply
/// race to the end; what it does instead is run whatever step its stale index now names.
#[test]
fn a_host_that_steps_over_a_flush_point_waits_for_it_too() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("handlers/inv.ini"),
            &fixture("handlers/flush-in-rescue.yml"),
        ],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert_eq!(
        text.matches("TASK [meta]").count(),
        1,
        "the flush shows for the host that entered the rescue and for no other: {text}"
    );
    let banners: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("RUNNING HANDLER ["))
        .collect();
    assert_eq!(
        banners.len(),
        2,
        "one flush each, at different points: {text}"
    );
    let first = handler_section(&text, "the handler");
    assert!(
        first.contains("[h1]") && !first.contains("[h2]"),
        "only the host that reached the flush runs its handler there: {text}"
    );
    assert!(
        section(&text, "after the block").contains("[h1]")
            && section(&text, "after the block").contains("[h2]"),
        "both hosts are still in the play behind the block: {text}"
    );
    assert!(
        text.contains(
            "h1                         : ok=4    changed=1    unreachable=0    failed=0    skipped=0    rescued=1"
        ) && text.contains(
            "h2                         : ok=3    changed=1    unreachable=0    failed=0    skipped=1    rescued=0"
        ),
        "{text}"
    );
}

/// A `Runs` row and what proves it: the playbook to run, the arguments to run it with, the exit
/// code to expect and a string the run has to print.
struct RunsProbe {
    table: &'static str,
    kw: &'static str,
    body: &'static str,
    args: &'static [&'static str],
    code: i32,
    expect: &'static str,
}

const fn runs(
    table: &'static str,
    kw: &'static str,
    body: &'static str,
    args: &'static [&'static str],
    code: i32,
    expect: &'static str,
) -> RunsProbe {
    RunsProbe {
        table,
        kw,
        body,
        args,
        code,
        expect,
    }
}

/// One proof per `Runs` row, every one of them a real run of this binary. Each body is written
/// so that deleting the keyword's handling changes the run: the task stops failing, the loop
/// stops looping, the refusal stops coming.
///
/// Nothing here is proved by naming a test somewhere else. Three escalation rows used to be,
/// and the name was inert text: no compiler and no assertion checked that the named test still
/// existed, those tests are `#[ignore]`d so an ordinary run proved none of the three, and the
/// same hatch would have marked any future row `Runs` with no proof at all. Escalating to an
/// account that does not exist needs no privileged host and no second suite: `sudo` refuses by
/// naming the account, which is a `become_user` that reached it intact, and dropping either
/// escalation keyword lets the task run as the invoking user and succeed.
const RUNS_PROBES: &[RunsProbe] = &[
    runs(
        "task",
        "args",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      command: echo hi\n      args:\n        chdir: /nonexistent-volant-probe\n",
        &[],
        2,
        "nonexistent-volant-probe",
    ),
    runs(
        "task",
        "become",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      command: echo hi\n      become: true\n      become_user: nosuchuser-volant-probe\n",
        &[],
        2,
        "nosuchuser-volant-probe",
    ),
    runs(
        "task",
        "become_method",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      command: echo hi\n      become_method: su\n",
        &[],
        2,
        "become_method 'su' is not supported yet",
    ),
    runs(
        "task",
        "become_user",
        "- hosts: localhost\n  gather_facts: false\n  become: true\n  tasks:\n    - name: Probe task\n      command: echo hi\n      become_user: nosuchuser-volant-probe\n",
        &[],
        2,
        "nosuchuser-volant-probe",
    ),
    // The playbook asks for a task that cannot succeed and then excludes it by tag. Selecting
    // nothing leaves the failure in the run, so deleting the selection turns exit 0 into exit 2
    // -- a red that cannot be produced by the tag being read and then ignored.
    runs(
        "task",
        "tags",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      debug:\n        msg: probe\n      tags: [probe]\n    - name: Never selected\n      command: nosuchbinary-volant-probe\n      tags: [other]\n",
        &["--tags", "probe"],
        0,
        "\"msg\": \"probe\"",
    ),
    runs(
        "task",
        "changed_when",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      debug:\n        msg: probe\n      changed_when: true\n",
        &[],
        0,
        "changed: [localhost]",
    ),
    runs(
        "task",
        "failed_when",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      debug:\n        msg: probe\n      failed_when: true\n",
        &[],
        2,
        "fatal: [localhost]: FAILED!",
    ),
    runs(
        "task",
        "ignore_errors",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      command: nosuchbinary-volant-probe\n      ignore_errors: true\n",
        &[],
        0,
        "ignoring",
    ),
    runs(
        "task",
        "loop",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      debug:\n        msg: \"{{ item }}\"\n      loop: [alpha]\n",
        &[],
        0,
        "(item=alpha)",
    ),
    runs(
        "task",
        "loop_control",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      debug:\n        msg: \"{{ thing }}\"\n      loop: [alpha]\n      loop_control:\n        loop_var: thing\n",
        &[],
        0,
        "\"msg\": \"alpha\"",
    ),
    runs(
        "task",
        "name",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: A named probe\n      debug:\n        msg: probe\n",
        &[],
        0,
        "TASK [A named probe]",
    ),
    runs(
        "task",
        "register",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - command: echo probe-registered\n      register: out\n    - debug:\n        msg: \"{{ out.stdout }}\"\n",
        &[],
        0,
        "\"msg\": \"probe-registered\"",
    ),
    runs(
        "task",
        "timeout",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      command: sleep 5\n      timeout: 1\n",
        &[],
        2,
        "Timed out after 1 second(s).",
    ),
    runs(
        "task",
        "vars",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      debug:\n        msg: \"{{ probe }}\"\n      vars:\n        probe: task-value\n",
        &[],
        0,
        "\"msg\": \"task-value\"",
    ),
    runs(
        "task",
        "when",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      debug:\n        msg: probe\n      when: false\n",
        &[],
        0,
        "skipping: [localhost]",
    ),
    runs(
        "task",
        "with_items",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      debug:\n        msg: \"{{ item }}\"\n      with_items:\n        - [alpha]\n",
        &[],
        0,
        "(item=alpha)",
    ),
    runs(
        "play",
        "become",
        "- hosts: localhost\n  gather_facts: false\n  become: true\n  tasks:\n    - command: echo hi\n",
        &["--become-method", "su"],
        2,
        "become_method 'su' is not supported yet",
    ),
    runs(
        "play",
        "become_method",
        "- hosts: localhost\n  gather_facts: false\n  become_method: su\n  tasks:\n    - command: echo hi\n",
        &[],
        2,
        "become_method 'su' is not supported yet",
    ),
    runs(
        "play",
        "become_user",
        "- hosts: localhost\n  gather_facts: false\n  become: true\n  become_user: nosuchuser-volant-probe\n  tasks:\n    - name: Probe task\n      command: echo hi\n",
        &[],
        2,
        "nosuchuser-volant-probe",
    ),
    // `gather_facts: true` is answered rather than obeyed: this release gathers no facts and
    // says so before the first task, which is the difference between a keyword handled and a
    // keyword ignored.
    runs(
        "play",
        "gather_facts",
        "- hosts: localhost\n  gather_facts: true\n  tasks:\n    - command: echo hi\n",
        &[],
        0,
        "gather_facts is not available in this release",
    ),
    // The recap, not the banner: `PLAY [localhost]` is `name` falling back to `hosts`, so it
    // would still read the same if `hosts` never reached the inventory. A host only reaches the
    // recap by having been selected and run.
    runs(
        "play",
        "hosts",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - command: echo hi\n",
        &[],
        0,
        "localhost                  : ok=1",
    ),
    runs(
        "play",
        "name",
        "- name: A named probe\n  hosts: localhost\n  gather_facts: false\n  tasks:\n    - command: echo hi\n",
        &[],
        0,
        "PLAY [A named probe]",
    ),
    // The three section keywords roles brought in. Each body does nothing but the section it
    // names, so deleting the section's handling leaves the run with nothing to print: the role's
    // task never reaches a host, and the `pre_tasks` and `post_tasks` lists are dropped on the
    // floor. The role directory these read is written by the probe runner beside the playbook,
    // which is the first place the reference looks for one.
    runs(
        "play",
        "roles",
        "- hosts: localhost\n  gather_facts: false\n  roles: [probe_role]\n",
        &[],
        0,
        "\"msg\": \"role-body\"",
    ),
    runs(
        "play",
        "pre_tasks",
        "- hosts: localhost\n  gather_facts: false\n  pre_tasks:\n    - name: Probe task\n      debug:\n        msg: pre-ran\n",
        &[],
        0,
        "\"msg\": \"pre-ran\"",
    ),
    runs(
        "play",
        "post_tasks",
        "- hosts: localhost\n  gather_facts: false\n  post_tasks:\n    - name: Probe task\n      debug:\n        msg: post-ran\n",
        &[],
        0,
        "\"msg\": \"post-ran\"",
    ),
    runs(
        "play",
        "strategy",
        "- hosts: localhost\n  gather_facts: false\n  strategy: free\n  tasks:\n    - command: echo hi\n",
        &[],
        4,
        "strategy 'free' is not supported yet",
    ),
    runs(
        "play",
        "tasks",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: A named probe\n      command: echo hi\n",
        &[],
        0,
        "TASK [A named probe]",
    ),
    // The play's tags have to reach the task under it: the task carries none of its own, so
    // `--tags probe` selects it only through the play. Deleting the inheritance selects nothing,
    // the command that cannot succeed never runs, and the exit code drops from 2 to 0.
    runs(
        "play",
        "tags",
        "- hosts: localhost\n  gather_facts: false\n  tags: [probe]\n  tasks:\n    - name: Probe task\n      command: nosuchbinary-volant-probe\n",
        &["--tags", "probe"],
        2,
        "nosuchbinary-volant-probe",
    ),
    runs(
        "play",
        "vars",
        "- hosts: localhost\n  gather_facts: false\n  vars:\n    probe: play-value\n  tasks:\n    - debug:\n        msg: \"{{ probe }}\"\n",
        &[],
        0,
        "\"msg\": \"play-value\"",
    ),
    runs(
        "play",
        "vars_files",
        "- hosts: localhost\n  gather_facts: false\n  vars_files:\n    - vars_files.vars.yml\n  tasks:\n    - debug:\n        msg: \"{{ probe }}\"\n",
        &[],
        0,
        "\"msg\": \"file-value\"",
    ),
    // A block has a grammar of its own, so it carries its own rows. Every one of these bodies
    // is written so that dropping the block's handling changes the run: the inherited keyword
    // stops reaching the task under it, or the section stops running at all.
    runs(
        "block",
        "block",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - block:\n        - name: Inside the block\n          command: echo hi\n",
        &[],
        0,
        "TASK [Inside the block]",
    ),
    // Measured on ansible-core 2.19.12: a task failing in the body runs the `always` section
    // and only then leaves the play.
    runs(
        "block",
        "always",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - block:\n        - name: Probe task\n          command: nosuchbinary-volant-probe\n      always:\n        - name: Always probe\n          command: echo hi\n",
        &[],
        2,
        "TASK [Always probe]",
    ),
    // Measured on ansible-core 2.19.12: a task failing in the body sends the host into the
    // rescue, and the run exits 0 with `rescued=1`. Written so the failure is the only thing
    // that can end the run: drop the rescue's handling and the failure stands, exit 2.
    runs(
        "block",
        "rescue",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - block:\n        - name: Probe task\n          command: nosuchbinary-volant-probe\n      rescue:\n        - name: Rescue probe\n          command: echo hi\n",
        &[],
        0,
        "TASK [Rescue probe]",
    ),
    // A block's own name is shown nowhere - measured, neither as a banner nor in a listing -
    // so what proves the keyword is read is that the block still runs with one written on it.
    runs(
        "block",
        "name",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: A named block\n      block:\n        - name: Inside a named block\n          command: echo hi\n",
        &[],
        0,
        "TASK [Inside a named block]",
    ),
    runs(
        "block",
        "become",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - block:\n        - name: Probe task\n          command: echo hi\n      become: true\n",
        &["--become-method", "su"],
        2,
        "become_method 'su' is not supported yet",
    ),
    runs(
        "block",
        "become_method",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - block:\n        - command: echo hi\n      become_method: su\n",
        &[],
        2,
        "become_method 'su' is not supported yet",
    ),
    runs(
        "block",
        "become_user",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - block:\n        - name: Probe task\n          command: echo hi\n          become: true\n      become_user: nosuchuser-volant-probe\n",
        &[],
        2,
        "nosuchuser-volant-probe",
    ),
    runs(
        "block",
        "ignore_errors",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - block:\n        - name: Probe task\n          command: nosuchbinary-volant-probe\n      ignore_errors: true\n",
        &[],
        0,
        "...ignoring",
    ),
    // Same shape one level in: the task carries no tag of its own and is selected only through
    // the block's. Deleting the inheritance selects nothing and the run exits 0 instead of 2.
    runs(
        "block",
        "tags",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - block:\n        - name: Probe task\n          command: nosuchbinary-volant-probe\n      tags: [probe]\n",
        &["--tags", "probe"],
        2,
        "nosuchbinary-volant-probe",
    ),
    runs(
        "block",
        "timeout",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - block:\n        - name: Probe task\n          command: sleep 5\n      timeout: 1\n",
        &[],
        2,
        "Timed out after 1 second(s).",
    ),
    runs(
        "block",
        "vars",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - block:\n        - name: Probe task\n          debug:\n            msg: \"{{ probe }}\"\n      vars:\n        probe: block-value\n",
        &[],
        0,
        "\"msg\": \"block-value\"",
    ),
    runs(
        "block",
        "when",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - block:\n        - name: Probe task\n          debug:\n            msg: probe\n      when: false\n",
        &[],
        0,
        "skipping: [localhost]",
    ),
    // `loop_control` is a mapping, so its sub-keys carry their own rows and their own proofs.
    // Reading two of them and dropping the rest is how a keyword the table vouches for goes on
    // ignoring most of what was written under it.
    runs(
        "loop_control",
        "label",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      debug:\n        msg: probe\n      loop: [alpha]\n      loop_control:\n        label: shown-instead\n",
        &[],
        0,
        "(item=shown-instead)",
    ),
    runs(
        "loop_control",
        "loop_var",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      debug:\n        msg: \"{{ thing }}\"\n      loop: [alpha]\n      loop_control:\n        loop_var: thing\n",
        &[],
        0,
        "\"msg\": \"alpha\"",
    ),
    // The handler keywords. Each body is written so that dropping the keyword's handling leaves
    // the probe with nothing to print: no handler list means no handler to notify, no `notify`
    // means nothing asks for one, no `listen` means the name reaches nothing, and without
    // `force_handlers` the failed host leaves before its handler.
    runs(
        "play",
        "handlers",
        "- hosts: localhost\n  gather_facts: false\n  handlers:\n    - name: Probe handler\n      debug:\n        msg: handler-ran\n  tasks:\n    - name: Probe task\n      command: echo hi\n      notify: Probe handler\n",
        &[],
        0,
        "RUNNING HANDLER [Probe handler]",
    ),
    runs(
        "task",
        "notify",
        "- hosts: localhost\n  gather_facts: false\n  handlers:\n    - name: Probe handler\n      command: nosuchbinary-volant-probe\n  tasks:\n    - name: Probe task\n      command: echo hi\n      notify: Probe handler\n",
        &[],
        2,
        "nosuchbinary-volant-probe",
    ),
    // A `notify` on the block, not on the task under it: the task carries none of its own, so
    // dropping the inheritance leaves the handler unnotified and the run exits 0.
    runs(
        "block",
        "notify",
        "- hosts: localhost\n  gather_facts: false\n  handlers:\n    - name: Probe handler\n      command: nosuchbinary-volant-probe\n  tasks:\n    - block:\n        - name: Probe task\n          command: echo hi\n      notify: Probe handler\n",
        &[],
        2,
        "nosuchbinary-volant-probe",
    ),
    // The notified name is one no handler carries as its `name`, so only `listen` can reach it.
    runs(
        "handler",
        "listen",
        "- hosts: localhost\n  gather_facts: false\n  handlers:\n    - name: Probe handler\n      command: nosuchbinary-volant-probe\n      listen: probe-group\n  tasks:\n    - name: Probe task\n      command: echo hi\n      notify: probe-group\n",
        &[],
        2,
        "nosuchbinary-volant-probe",
    ),
    // The host fails after notifying, so the handler only runs because of the flag. Without it
    // the run still exits 2, which is why the proof is the handler's own output line.
    runs(
        "play",
        "force_handlers",
        "- hosts: localhost\n  gather_facts: false\n  force_handlers: true\n  handlers:\n    - name: Probe handler\n      debug:\n        msg: forced-handler-ran\n  tasks:\n    - name: Probe task\n      command: echo hi\n      notify: Probe handler\n    - name: Probe failure\n      command: nosuchbinary-volant-probe\n",
        &[],
        2,
        "\"msg\": \"forced-handler-ran\"",
    ),
];

/// Every keyword the table marks `Runs` has a proof, and every proof belongs to a row.
///
/// What would make this red: a row flipped to `Runs` ahead of the code that honours it, which
/// is the way a keyword comes to be accepted, waved through and ignored; or a proof left behind
/// for a keyword that no longer claims to run.
#[test]
fn every_runs_keyword_has_a_proof_and_every_proof_has_a_row() {
    let mut declared: Vec<(&str, &str)> = [
        ("task", TASK_KEYWORDS),
        ("play", PLAY_KEYWORDS),
        ("loop_control", LOOP_CONTROL_KEYWORDS),
        ("block", BLOCK_KEYWORDS),
        ("handler", HANDLER_KEYWORDS),
    ]
    .into_iter()
    .flat_map(|(table, keywords)| {
        keywords
            .iter()
            .filter(|k| k.support == Support::Runs)
            .map(move |k| (table, k.name))
    })
    .collect();
    let mut proved: Vec<(&str, &str)> = RUNS_PROBES.iter().map(|p| (p.table, p.kw)).collect();
    declared.sort_unstable();
    proved.sort_unstable();
    assert_eq!(declared, proved);
}

/// Each `Runs` proof, run. The bodies are written so that deleting the keyword's handling
/// changes what comes out: `when: false` stops skipping, `timeout: 1` stops killing `sleep`,
/// `register` stops carrying the output to the next task, `strategy: free` stops being refused,
/// `become_user` stops naming an account `sudo` cannot find.
/// A fake `sudo` for the three escalation rows in `RUNS_PROBES` (`task.become`,
/// `task.become_user`, `play.become_user`): it resolves `-u <user>` itself, the way a
/// real `sudo` resolves the account before it ever consults policy, and refuses only the one
/// name these probes write. Every other user -- in particular `root`, the default `become_user`
/// -- succeeds without a password and simply runs the command it was handed.
///
/// This replaces two properties of the machine the probes used to rest on instead of on this
/// engine's own code: that the local `sudo` resolves an unknown user before policy (so the
/// message names the account) and that `sudo` to root needs no password (so the run reaches the
/// task at all). Neither is guaranteed on every CI runner or contributor machine; what the fake
/// proves instead is the one thing these rows are for, that `become_user` reaches `sudo`'s argv.
const FAKE_SUDO_FOR_BECOME_USER: &str = "#!/bin/sh\n\
 user=\n\
 prev=\n\
 for arg in \"$@\"; do\n\
 [ \"$prev\" = -u ] && user=\"$arg\"\n\
 prev=\"$arg\"\n\
 done\n\
 if [ \"$user\" = nosuchuser-volant-probe ]; then\n\
 echo 'sudo: unknown user nosuchuser-volant-probe' >&2\n\
 exit 1\n\
 fi\n\
 while [ $# -gt 0 ] && [ \"$1\" != -- ]; do shift; done\n\
 shift\n\
 exec \"$@\"\n";

/// Whether a `RunsProbe` row is one of the three escalation rows that route through
/// [`FAKE_SUDO_FOR_BECOME_USER`] instead of the machine's real `sudo`.
fn is_become_user_probe(probe: &RunsProbe) -> bool {
    matches!(
        (probe.table, probe.kw),
        ("task", "become")
            | ("task", "become_user")
            | ("play", "become_user")
            | ("block", "become_user")
    )
}

#[test]
fn every_runs_keyword_changes_something_observable() {
    let dir = probe_dir("runs");
    let sudo_dir = fake_sudo("runs-become-user", FAKE_SUDO_FOR_BECOME_USER);
    // The role the `play.roles` row reads, beside the playbook where the reference looks first.
    std::fs::create_dir_all(dir.join("roles/probe_role/tasks")).expect("the probe role is written");
    std::fs::write(
        dir.join("roles/probe_role/tasks/main.yml"),
        "- name: Probe task\n  debug:\n    msg: role-body\n",
    )
    .expect("the probe role's tasks are written");
    // Every probe runs before anything is asserted, so one failing run reports every row that
    // broke rather than only the first: a change that touches several keywords is read once.
    let mut failures = Vec::new();
    for probe in RUNS_PROBES {
        let body = probe.body;
        let name = format!("{}-{}", probe.table, probe.kw);
        if probe.kw == "vars_files" {
            std::fs::write(dir.join(format!("{name}.vars.yml")), "probe: file-value\n")
                .expect("the probe's vars file is written");
        }
        let body = body.replace("vars_files.vars.yml", &format!("{name}.vars.yml"));
        let path = is_become_user_probe(probe).then_some(sudo_dir.as_path());
        let (code, text) = run_probe(&dir, &name, &body, probe.args, path);
        if code != probe.code || !text.contains(probe.expect) {
            failures.push(format!(
                "{name}: wanted exit {} and {:?}, got exit {code}:\n{text}",
                probe.code, probe.expect
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} keyword(s) stopped doing what the table says:\n{}",
        failures.len(),
        failures.join("\n")
    );
    std::fs::remove_dir_all(&dir).expect("the probe directory is removed");
    std::fs::remove_dir_all(&sudo_dir).expect("the fake sudo directory is removed");
}

/// A play's roles, read from the directory beside the playbook and spliced into the step list.
///
/// The snapshot is the reference's own output for this fixture, measured on ansible-core
/// 2.19.12: the sections run `pre_tasks`, roles, `tasks`, `post_tasks`; a `meta/main.yml`
/// dependency runs in front of the role that depends on it; every task of a role carries the
/// `role : name` prefix, and an unnamed one shows its module behind that prefix
/// (`TASK [spec : debug]`); a role read with `tasks_from` shows only that file's tasks; and an
/// `import_tasks` inside a role reads its file against the role's `tasks/` directory, while a
/// nested one reads against the directory of the file that wrote it.
///
/// What would make this red: the `base : ` prefix dropped, the dependency running after `child`
/// instead of before it, a section running out of order, or an imported file looked for beside
/// the playbook rather than beside its importer.
#[test]
fn a_role_is_found_next_to_the_playbook_and_named_in_every_banner() {
    let out = volant(&[
        "playbook",
        "-i",
        &fixture("inventory.ini"),
        &fixture("roles/roles.yml"),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    settings().bind(|| insta::assert_snapshot!(String::from_utf8(out.stdout).unwrap()));
}

/// A role nobody can find stops the run before the first banner, naming every directory it
/// looked in, and exits **1**.
///
/// Measured on ansible-core 2.19.12: `the role 'nosuchrole' was not found in <paths joined by
/// colons>`, exit 1 - not the 4 a playbook it cannot make sense of gets. The order is
/// `<playbook_dir>/roles`, the `roles_path` entries, then `<playbook_dir>`.
///
/// What would make this red: the exit code drifting to the compiler's blanket 4, which is what
/// happens the moment the refusal stops carrying a code of its own; a path missing from the
/// list, which sends an operator looking in the wrong place; or a `PLAY` banner printed before
/// the refusal, which would mean the roles were read after the run had started.
#[test]
fn a_missing_role_names_every_directory_searched_and_exits_1() {
    let out = volant(&["playbook", &fixture("roles/missing.yml")]);
    let err = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("the role 'nosuchrole' was not found in "),
        "{err}"
    );
    // Matched by their tails: the compiler resolves the playbook's directory, so the prefix a
    // fixture path is spelled with is not always the prefix the refusal prints.
    assert!(err.contains("fixtures/roles/roles:"), "{err}");
    assert!(err.contains("/usr/share/ansible/roles"), "{err}");
    assert!(err.contains("/etc/ansible/roles"), "{err}");
    assert!(err.trim_end().ends_with("fixtures/roles"), "{err}");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("PLAY ["),
        "nothing runs before a role is found"
    );
}

/// Two identical entries are one role; two that differ in what they hand it are two.
///
/// Measured: `- base` twice runs once, `- base` and `- { role: base, vars: { p: twice } }` run
/// twice, and a role whose required argument nobody supplied fails the run at exit 2 with the
/// reference's own `argument_errors`.
///
/// What would make this red: an identity that compares the role's name alone, which drops the
/// second entry and reports success having run one role less than the playbook asked for; or an
/// argument check that passes whatever it is given, which lets a role run without a variable it
/// declared it could not work without.
#[test]
fn identical_role_entries_run_once_and_different_parameters_run_twice() {
    let out = volant(&[
        "playbook",
        "-i",
        &fixture("inventory.ini"),
        &fixture("roles/roles-dup.yml"),
    ]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert_eq!(
        text.matches("TASK [base : base task]").count(),
        2,
        "two entries, not three and not one: {text}"
    );
    assert!(text.contains("p=none"), "{text}");
    assert!(text.contains("p=twice"), "{text}");
    assert!(
        text.contains("\"argument_errors\": [\"missing required arguments: needed\"]"),
        "{text}"
    );
    assert!(text.contains("\"argument_spec_name\": \"main\""), "{text}");
    assert!(text.contains("failed=1"), "{text}");
}

/// A role's `defaults` and `vars` stay visible to the rest of the play, and its parameters do
/// not.
///
/// Measured on ansible-core 2.19.12 on this fixture: the play's own task after the roles reads
/// `shared=role-var d=role-default p=fact`, and the `pre_tasks` task that runs **before** any
/// role reads `pre d=role-default shared=role-var`. So the export belongs to the play rather
/// than accumulating as the list is walked, `vars/main.yml` beats the play's `vars:`, and a role
/// parameter is gone once its role is over - `p` is back to the fact the `pre_tasks` set, not
/// the `role-param` two entries handed the role.
///
/// What would make this red: dropping the export, which leaves both tasks reading `unset`; or
/// exporting the parameters too, which would leave the task after the roles reading
/// `p=role-param` - a role parameter outliving the role that was given it.
#[test]
fn role_variables_stay_visible_after_the_role() {
    let out = volant(&[
        "playbook",
        "-i",
        &fixture("inventory.ini"),
        &fixture("roles/roles.yml"),
    ]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains("\"msg\": \"shared=role-var d=role-default p=fact\""),
        "{text}"
    );
    assert!(
        text.contains("\"msg\": \"pre d=role-default shared=role-var\""),
        "{text}"
    );
    assert!(
        text.contains("\"msg\": \"post v=role-var\""),
        "post_tasks see them too: {text}"
    );
}

/// `vars:` on a role entry is a task variable; a free key beside `role:` is a role parameter.
/// The two sit on opposite sides of a fact, which is what tells them apart.
///
/// Measured on ansible-core 2.19.12 on this fixture, whose `pre_tasks` set `p` to `fact`: the
/// entry written `vars: { p: role-param }` runs the role reading `p=fact`, and the entry written
/// with `p: role-param` as a free key runs it reading `p=role-param`. Three of the four entries
/// read the fact - the bare one, the `vars:` one, and `child`'s dependency, which also hands its
/// parameter through `vars:` - and only the free-key one reads its own value.
///
/// What would make this red: folding a role entry's `vars:` into its parameters, which lifts it
/// above the fact and leaves two entries reading `role-param`; or the reverse, reading a free
/// key as a task variable, which leaves all four reading `fact`.
#[test]
fn a_role_entry_vars_loses_to_a_fact_and_a_free_key_beats_it() {
    let out = volant(&[
        "playbook",
        "-i",
        &fixture("inventory.ini"),
        &fixture("roles/roles.yml"),
    ]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert_eq!(
        text.matches("\"msg\": \"d=role-default v=role-var shared=role-var p=fact\"")
            .count(),
        4,
        "a task variable loses to a fact: {text}"
    );
    assert_eq!(
        text.matches("\"msg\": \"d=role-default v=role-var shared=role-var p=role-param\"")
            .count(),
        1,
        "a role parameter beats a fact: {text}"
    );
}

/// `import_role` runs a role the play's `roles:` list has already run with the same parameters.
///
/// Measured on ansible-core 2.19.12 on this fixture: `base` runs five times - four entries in
/// `roles:` counting `child`'s dependency, then once more for the bare `import_role: { name:
/// base }` in `tasks:`, whose entry is identical to the bare one the list already ran.
///
/// What would make this red: letting `import_role` consult the play's own list of what it has
/// run, which drops the import and leaves four - a run reporting success having done less than
/// the playbook asked for.
#[test]
fn an_import_role_runs_again_what_the_roles_list_already_ran() {
    let out = volant(&[
        "playbook",
        "-i",
        &fixture("inventory.ini"),
        &fixture("roles/roles.yml"),
    ]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert_eq!(
        text.matches("TASK [base : base task]").count(),
        5,
        "four entries and the import that repeats one of them: {text}"
    );
}

/// A ring of roles importing each other, and a file importing itself, are refused before the
/// recursion runs out of stack.
///
/// Measured on ansible-core 2.19.12: two roles importing each other are refused with `A
/// recursion loop was detected with the roles specified.`, exit 1, while a file importing itself
/// with `import_tasks` is not caught at all - the interpreter dies and `ansible-playbook` exits
/// 250 printing its own traceback. This engine refuses both, at exit 1, and the second in its
/// own words: the divergence is recorded rather than reproducing a crash.
///
/// What would make this red: an unbounded recursion, which kills the process with no message and
/// no exit code of its own; or a depth counter that only one of the two recursions increments,
/// which leaves the other unbounded.
#[test]
fn a_ring_of_imports_is_refused_instead_of_exhausting_the_stack() {
    let deadline = std::time::Duration::from_secs(20);
    let roles = volant_within(&["playbook", &fixture("roles/role-cycle.yml")], deadline);
    let err = String::from_utf8(roles.stderr).unwrap();
    assert_eq!(roles.status.code(), Some(1), "{err}");
    assert!(
        err.contains("A recursion loop was detected with the roles specified."),
        "{err}"
    );

    let imports = volant_within(&["playbook", &fixture("roles/self-import.yml")], deadline);
    let err = String::from_utf8(imports.stderr).unwrap();
    assert_eq!(imports.status.code(), Some(1), "{err}");
    assert!(
        err.contains("imports nest deeper than 32 levels at "),
        "{err}"
    );
    assert!(err.contains("self-loop.yml"), "{err}");
}

/// An argument an import statement does not take is refused by its own name, rather than read
/// past and dropped.
///
/// Measured on ansible-core 2.19.12: `Invalid options for import_role: typo` and `Invalid
/// options for import_tasks: apply`, exit 4 for both. The nine arguments `import_role` does take
/// were measured the same way, `apply` being the one `import_tasks` refuses that its sibling
/// `include_tasks` accepts.
///
/// What would make this red: reading the arguments the statement knows and ignoring the rest,
/// which accepts a typo silently and runs the role without what the operator meant to hand it.
#[test]
fn an_import_statement_refuses_an_argument_it_does_not_take() {
    for (file, message) in [
        (
            "roles/bad-role-option.yml",
            "Invalid options for import_role: typo",
        ),
        (
            "roles/bad-import-option.yml",
            "Invalid options for import_tasks: apply",
        ),
    ] {
        let out = volant(&["playbook", &fixture(file)]);
        let err = String::from_utf8(out.stderr).unwrap();
        assert_eq!(out.status.code(), Some(4), "{err}");
        assert!(err.contains(message), "{err}");
    }
}

/// `import_playbook` puts the imported file's plays where the statement stands, not at the end.
///
/// Measured: a file importing `sub.yml`, declaring a play of its own and importing `sub.yml`
/// again runs `PLAY [h2]`, `PLAY [h1]`, `PLAY [h2]` in that order, and the second import is
/// written as a template with no variable in it.
///
/// What would make this red: appending the imported plays instead of splicing them, which runs
/// the playbook in an order nobody wrote.
#[test]
fn import_playbook_splices_plays_in_place() {
    let out = volant(&[
        "playbook",
        "-i",
        &fixture("imports/hosts.ini"),
        &fixture("imports/main.yml"),
    ]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    let order: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("PLAY ["))
        .map(|l| l.split_whitespace().nth(1).unwrap_or_default())
        .collect();
    assert_eq!(order, ["[h2]", "[h1]", "[h2]"], "{text}");
}

/// An `import_tasks` naming a file that is not there stops the run at exit 1, in the reference's
/// own words, before anything has run.
///
/// Measured on ansible-core 2.19.12: `Unable to retrieve file contents.` followed by
/// `Could not find or access '<absolute path>' on the Ansible Controller.`, exit 1.
///
/// What would make this red: an import whose file is missing being skipped, which is a run that
/// reports success having done none of what the imported file said.
#[test]
fn an_import_tasks_naming_a_missing_file_stops_the_run() {
    let dir = probe_dir("import-missing");
    std::fs::write(
        dir.join("site.yml"),
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - import_tasks: nosuchfile.yml\n",
    )
    .expect("the probe playbook is written");
    let path = dir.join("site.yml");
    let out = volant(&["playbook", &path.display().to_string()]);
    let err = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("Unable to retrieve file contents."), "{err}");
    // The path is matched by its tail rather than by the whole string: the compiler resolves the
    // playbook's directory, and on macOS a temporary directory resolves to a different prefix
    // than the one it was handed.
    assert!(
        err.contains("Could not find or access '") && err.contains("nosuchfile.yml'"),
        "{err}"
    );
    assert!(err.contains("on the Ansible Controller."), "{err}");
    std::fs::remove_dir_all(&dir).expect("the probe directory is removed");
}
