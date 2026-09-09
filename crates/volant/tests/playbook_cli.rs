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

/// Two measured durations in the same process, never a constant: six one-second sleeps run
/// six at a time against the same six run one at a time.
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
        narrow.as_secs_f64() > wide.as_secs_f64() * 2.5,
        "forks=1 must serialise six one-second sleeps (wide {wide:?}, narrow {narrow:?})"
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

/// `become_method` other than `sudo` stops the run before anything executes: exit 1, and the
/// method named. `sudo` is not silently substituted for the program the playbook asked for.
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
    assert_eq!(out.status.code(), Some(1));
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
    assert_eq!(out.status.code(), Some(1));
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
