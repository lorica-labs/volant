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
        .output()
        .expect("volant runs")
}

/// Runs volant and fails if it has not finished within `deadline`. A barrier that never opens
/// hangs instead of returning, and a hung run only ends at the harness's own timeout, so the
/// tests that prove a wait ends say so with a deadline rather than with elapsed time alone.
fn volant_within(args: &[&str], deadline: std::time::Duration) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(args)
        .env("NO_COLOR", "1")
        .env_remove("COLUMNS")
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
    let after = text
        .split_once(&format!("TASK [{task}]"))
        .unwrap_or_else(|| panic!("no task named {task} in:\n{text}"))
        .1;
    match after.find("\nTASK [").or_else(|| after.find("\nPLAY ")) {
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
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
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

// The keyword table, covered in both directions. Between them the two tests below say that
// every row of `keywords::TASK_KEYWORDS` and `keywords::PLAY_KEYWORDS` does what its `Support`
// claims: a `Preflight` row stops the run before any banner, a `Runs` row changes something an
// operator can see. Both lists are walked from the table itself rather than written out here,
// so a row added without its proof fails the suite instead of slipping through - which is how
// a keyword would come to be accepted by the loader, waved past the pre-flight and then
// ignored, the failure this whole split exists to prevent.

use volant::keywords::{BLOCK_SECTIONS, PLAY_KEYWORDS, Support, TASK_KEYWORDS};

const PROBE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// A directory for generated probes, emptied first so a previous run cannot answer for this one.
fn probe_dir(kind: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("volant-probe-{kind}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a probe directory");
    dir
}

fn run_probe(dir: &std::path::Path, kw: &str, body: &str, extra: &[&str]) -> (i32, String) {
    let path = dir.join(format!("{kw}.yml"));
    std::fs::write(&path, body).expect("the probe is written");
    let mut args: Vec<&str> = vec!["playbook"];
    args.extend_from_slice(extra);
    let shown = path.display().to_string();
    args.push(&shown);
    let out = volant_within(&args, PROBE_DEADLINE);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

/// One probe playbook per `Preflight` row, built from the row's own name.
///
/// The value is never read - a `Preflight` keyword is parked, not parsed - so one placeholder
/// serves every row, and a keyword that starts being read will say so by failing to load.
fn preflight_probes() -> Vec<(String, String)> {
    let mut probes = Vec::new();
    let task = |kw: &str| {
        format!(
            "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      command: echo hi\n      {kw}: probe\n"
        )
    };
    for kw in TASK_KEYWORDS
        .iter()
        .filter(|k| k.support == Support::Preflight)
    {
        probes.push((kw.name.to_string(), task(kw.name)));
    }
    // A block's sections are grammar the loader has to accept and the pre-flight has to refuse,
    // even though nothing compiles them yet.
    for section in BLOCK_SECTIONS {
        probes.push((section.to_string(), task(section)));
    }
    for kw in PLAY_KEYWORDS
        .iter()
        .filter(|k| k.support == Support::Preflight)
    {
        probes.push((
            kw.name.to_string(),
            format!(
                "- hosts: localhost\n  gather_facts: false\n  {}: probe\n  tasks:\n    - name: Probe task\n      command: echo hi\n",
                kw.name
            ),
        ));
    }
    probes
}

/// Every keyword the table marks `Preflight` stops the run before the first connection, names
/// itself while doing it, and lets no `PLAY [` banner out first.
///
/// What would make this red: a keyword the loader accepts and the pre-flight forgets, which
/// would run the playbook without it and report success; a refusal that stops naming the
/// keyword, leaving the operator to guess; or a refusal raised after the banner, by which time
/// tasks may already have run. Adding a `Preflight` row to either table adds a case here on its
/// own, so the gap cannot be opened silently.
#[test]
fn every_preflight_keyword_is_refused_before_any_banner() {
    let dir = probe_dir("preflight");
    let probes = preflight_probes();
    assert!(
        probes.len() > 60,
        "the tables carry the whole grammar, so this walk is long: {}",
        probes.len()
    );
    for (kw, body) in &probes {
        let (code, text) = run_probe(&dir, kw, body, &[]);
        assert_eq!(code, 4, "{kw}: {text}");
        assert!(
            text.contains(&format!("keyword '{kw}' is not supported yet")),
            "{kw} must name itself: {text}"
        );
        assert!(
            !text.contains("PLAY ["),
            "{kw}: nothing runs before a pre-flight refusal: {text}"
        );
    }
    std::fs::remove_dir_all(&dir).expect("the probe directory is removed");
}

/// A `Runs` row and what proves it: the playbook to run, the arguments to run it with, the exit
/// code to expect and a string the run has to print.
struct RunsProbe {
    table: &'static str,
    kw: &'static str,
    /// `None` when the proof needs a privileged host and lives in the `ssh_*` suite instead;
    /// the string names the test that carries it.
    body: Option<&'static str>,
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
        body: Some(body),
        args,
        code,
        expect,
    }
}

const fn over_ssh(table: &'static str, kw: &'static str, test: &'static str) -> RunsProbe {
    RunsProbe {
        table,
        kw,
        body: None,
        args: &[],
        code: 0,
        expect: test,
    }
}

/// One proof per `Runs` row. Each body is written so that deleting the keyword's handling
/// changes the run: the task stops failing, the loop stops looping, the refusal stops coming.
///
/// Escalation is the exception. `become_user`, and `become` on a task, only show themselves on
/// a host this process can escalate on, so their proofs are the named `ssh_*` tests, which
/// `just remote ssh-test` runs. They are listed rather than left out so the completeness check
/// below still counts them.
const RUNS_PROBES: &[RunsProbe] = &[
    runs(
        "task",
        "args",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      command: echo hi\n      args:\n        chdir: /nonexistent-volant-probe\n",
        &[],
        2,
        "nonexistent-volant-probe",
    ),
    over_ssh("task", "become", "ssh_become_over_ssh"),
    runs(
        "task",
        "become_method",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      command: echo hi\n      become_method: su\n",
        &[],
        2,
        "become_method 'su' is not supported yet",
    ),
    over_ssh(
        "task",
        "become_user",
        "ssh_become_to_an_unprivileged_user_reaches_the_agent",
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
    over_ssh(
        "play",
        "become_user",
        "ssh_become_to_an_unprivileged_user_reaches_the_agent",
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
    runs(
        "play",
        "hosts",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - command: echo hi\n",
        &[],
        0,
        "PLAY [localhost]",
    ),
    runs(
        "play",
        "name",
        "- name: A named probe\n  hosts: localhost\n  gather_facts: false\n  tasks:\n    - command: echo hi\n",
        &[],
        0,
        "PLAY [A named probe]",
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
];

/// Every keyword the table marks `Runs` has a proof, and every proof belongs to a row.
///
/// What would make this red: a row flipped to `Runs` ahead of the code that honours it, which
/// is the way a keyword comes to be accepted, waved through and ignored; or a proof left behind
/// for a keyword that no longer claims to run.
#[test]
fn every_runs_keyword_has_a_proof_and_every_proof_has_a_row() {
    let mut declared: Vec<(&str, &str)> = TASK_KEYWORDS
        .iter()
        .filter(|k| k.support == Support::Runs)
        .map(|k| ("task", k.name))
        .chain(
            PLAY_KEYWORDS
                .iter()
                .filter(|k| k.support == Support::Runs)
                .map(|k| ("play", k.name)),
        )
        .collect();
    let mut proved: Vec<(&str, &str)> = RUNS_PROBES.iter().map(|p| (p.table, p.kw)).collect();
    declared.sort_unstable();
    proved.sort_unstable();
    assert_eq!(declared, proved);
}

/// Each `Runs` proof, run. The bodies are written so that deleting the keyword's handling
/// changes what comes out: `when: false` stops skipping, `timeout: 1` stops killing `sleep`,
/// `register` stops carrying the output to the next task, `strategy: free` stops being refused.
///
/// The escalation rows are skipped here and proved in the `ssh_*` suite, which needs a host to
/// escalate on; the test above is what keeps them from being forgotten.
#[test]
fn every_runs_keyword_changes_something_observable() {
    let dir = probe_dir("runs");
    // Every probe runs before anything is asserted, so one failing run reports every row that
    // broke rather than only the first: a change that touches several keywords is read once.
    let mut failures = Vec::new();
    for probe in RUNS_PROBES {
        let Some(body) = probe.body else {
            continue;
        };
        let name = format!("{}-{}", probe.table, probe.kw);
        if probe.kw == "vars_files" {
            std::fs::write(dir.join(format!("{name}.vars.yml")), "probe: file-value\n")
                .expect("the probe's vars file is written");
        }
        let body = body.replace("vars_files.vars.yml", &format!("{name}.vars.yml"));
        let (code, text) = run_probe(&dir, &name, &body, probe.args);
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
}
