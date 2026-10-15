// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(unix)]
use rstest::rstest;
use std::fmt::Write as _;
use std::path::Path;
use std::process::{Command, Output};

#[path = "common/process.rs"]
mod process;

#[path = "common/python.rs"]
mod python;

fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .display()
        .to_string()
}

/// The deadline a run gets when the test did not name one. Generous, because a test that reaches
/// it has hung rather than been slow.
///
/// What it is for is that the shared spawner has no unbounded form: a fixture becomes blockable
/// the day it gains an `include_tasks` or a `meta: flush_handlers`, and the test driving it says
/// nothing about that, so the bound belongs to the spawner rather than to the caller's memory.
const DEFAULT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

fn volant(args: &[&str]) -> Output {
    volant_within_with_path(args, DEFAULT_DEADLINE, None, &[])
}

/// Runs volant and fails if it has not finished within `deadline`. A barrier that never opens
/// hangs instead of returning, and a hung run only ends at the harness's own timeout, so the
/// tests that prove a wait ends say so with a deadline rather than with elapsed time alone.
fn volant_within(args: &[&str], deadline: std::time::Duration) -> Output {
    volant_within_with_path(args, deadline, None, &[])
}

/// `volant_within`, with extra environment variables -- the form the configuration tests need,
/// since a setting read from the environment can only be given to the process before it starts.
fn volant_within_env(
    args: &[&str],
    deadline: std::time::Duration,
    envs: &[(&str, &str)],
) -> Output {
    volant_within_with_path(args, deadline, None, envs)
}

/// `volant_within`, with an optional directory prepended to `PATH` -- the form `run_probe` needs
/// for the escalation probes, which put a fake `sudo` ahead of the real one rather than resting
/// on properties of the machine.
fn volant_within_with_path(
    args: &[&str],
    deadline: std::time::Duration,
    path: Option<&Path>,
    envs: &[(&str, &str)],
) -> Output {
    volant_within_full(args, deadline, path, None, None, None, envs)
}

/// `volant_within`, with `PATH` replaced outright rather than prepended to, a working directory
/// of its own, and a line fed to standard input -- the three forms the escalation and the
/// configuration tests need, and the reason none of them has to build its own `Command` and lose
/// the deadline with it.
fn volant_within_full(
    args: &[&str],
    deadline: std::time::Duration,
    path: Option<&Path>,
    replace_path: Option<&Path>,
    cwd: Option<&Path>,
    stdin_line: Option<&str>,
    envs: &[(&str, &str)],
) -> Output {
    assert!(
        path.is_none() || replace_path.is_none(),
        "path and replace_path both set: replace_path silently wins, dropping path's fixture"
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_volant"));
    command
        .args(args)
        .env("NO_COLOR", "1")
        .env_remove("COLUMNS")
        .env_remove("ANSIBLE_ROLES_PATH")
        .env_remove("ANSIBLE_RUN_TAGS")
        .env_remove("ANSIBLE_SKIP_TAGS")
        .env_remove("VOLANT_BATCHING")
        // The controller's own Python, which a play that gathers facts needs: taken from the
        // machine rather than from whatever virtualenv the developer running the suite happens
        // to have active, so a run reads the same here and on CI. A test that wants a particular
        // interpreter names it through `envs`, which is applied below and wins.
        .env_remove("VIRTUAL_ENV")
        .env_remove("VOLANT_PYTHON");
    for (name, value) in envs {
        command.env(name, value);
    }
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
    if let Some(dir) = replace_path {
        command.env("PATH", dir.display().to_string());
    }
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    process::run_within(&mut command, deadline, stdin_line.map(str::as_bytes))
}

fn settings() -> insta::Settings {
    let mut s = insta::Settings::clone_current();
    s.add_filter(r#""start": "[^"]*""#, r#""start": "[time]""#);
    s.add_filter(r#""end": "[^"]*""#, r#""end": "[time]""#);
    s.add_filter(r#""delta": "[^"]*""#, r#""delta": "[time]""#);
    s.add_filter(r#"The error was: [^"]*"#, "The error was: [engine message]");
    s.add_filter(r"Origin: \S*/tests/fixtures/", "Origin: [fixtures]/");
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

/// Measured on ansible-core 2.19.12 with two hosts and standard input not a terminal: a `pause`
/// runs for the first host alone. One `ok: [h1]`, one warning, `q` registered on h1 only, and a
/// `when` that leaves h1 out skips the pause for both. A pause that fails takes the other hosts
/// out of the play with no line and no recap entry, and the run exits 2.
///
/// What would make this red: a pause run per host, which prints `ok: [h2]`, registers `q` on h2 and pauses h2 under its `when`; or a failed pause that leaves h2 running
/// the next task.
#[test]
fn a_pause_runs_once_for_the_first_host() {
    let inventory = fixture("controller/two-hosts.ini");
    let run = |playbook: &str| {
        volant_within_full(
            &["playbook", "-i", &inventory, &fixture(playbook)],
            DEFAULT_DEADLINE,
            None,
            None,
            None,
            Some(""),
            &[],
        )
    };
    let out = run("controller/pause-once.yml");
    let text = String::from_utf8(out.stdout).unwrap();
    let errors = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}{errors}");
    let task = |name: &str| {
        text.split(&format!("TASK [{name}] "))
            .nth(1)
            .and_then(|rest| rest.split("\n\n").next())
            .map(|block| block.lines().skip(1).collect::<Vec<_>>())
            .unwrap_or_default()
    };
    assert_eq!(task("prompt"), ["ok: [h1]"], "{text}");
    assert_eq!(
        task("seen"),
        [
            "ok: [h1] => {",
            "    \"msg\": true",
            "}",
            "ok: [h2] => {",
            "    \"msg\": false",
            "}"
        ],
        "{text}"
    );
    assert_eq!(task("only h2"), ["skipping: [h1]"], "{text}");
    assert!(
        text.contains("h1                         : ok=2    changed=0    unreachable=0    failed=0    skipped=1")
            && text.contains("h2                         : ok=1    changed=0    unreachable=0    failed=0    skipped=0"),
        "{text}"
    );

    let out = run("controller/pause-fails.yml");
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(!text.contains("after") && !text.contains("[h2]"), "{text}");
    assert!(!text.contains("\nh2 "), "h2 has no recap entry: {text}");
}

/// A play whose pattern matches no host is skipped whole, so the pre-flight does not refuse a
/// prompting `pause` in it even on a terminal. Measured on ansible-core 2.19.12 under `script`
/// (a terminal on standard input): `skipping: no hosts matched`, the next play runs, exit 0.
///
/// What would make this red: the pause pre-flight run over a play nothing reaches, which refuses
/// with exit 4 a playbook the reference runs to its end.
#[cfg(target_os = "linux")]
#[test]
fn a_prompting_pause_in_a_play_with_no_host_is_not_refused() {
    let command = format!(
        "'{}' playbook '{}'",
        env!("CARGO_BIN_EXE_volant"),
        fixture("controller/pause-no-host.yml")
    );
    // util-linux `script` runs the command with a terminal on its standard input; `-e` passes
    // its exit code through.
    let mut child = Command::new("script")
        .args(["-qec", &command, "/dev/null"])
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("this test needs util-linux `script` to give the run a terminal");
    let started = std::time::Instant::now();
    while child.try_wait().expect("script is waitable").is_none() {
        if started.elapsed() >= PROBE_DEADLINE {
            let _ = child.kill();
            panic!("volant under script did not finish within {PROBE_DEADLINE:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let out = child.wait_with_output().expect("script output");
    let text = String::from_utf8_lossy(&out.stdout).replace('\r', "");
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("skipping: no hosts matched"), "{text}");
    assert!(text.contains(r#""msg": "after""#), "{text}");
}

/// A controller-side `set_fact` or `include_vars` that failed keeps none of the facts it answered,
/// `ignore_errors` or not, and a loop is judged whole. Measured on ansible-core 2.19.12 with this
/// fixture: `"msg": "False False False"`, recap `ok=4 ... ignored=3`.
///
/// What would make this red: the facts written as the module runs, before `failed_when` has
/// judged the result (`"True True True"`).
#[test]
fn a_failed_controller_side_task_keeps_none_of_its_facts() {
    let out = volant_within(
        &["playbook", &fixture("controller/failed-facts.yml")],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains(r#""msg": "False False False""#), "{text}");
}

/// `retries` is rendered per item, on the agent path, the controller-side one and an action
/// plugin's. Measured on ansible-core 2.19.12 with these fixtures, for each task: one retry line
/// for `r: 1`, two for `r: 2`, the item `r: x` failed alone with `"changed": false` and
/// `"msg": "Task failed: Error processing keyword 'retries': The value 'x' could not be
/// converted to 'int'."`, `r: 0` run once, and the registered `attempts` read `1,2,-,-`. The
/// `r: 0` item still has its `changed_when` applied: no `"changed": true` under the `command`,
/// one under the `copy`.
///
/// What would make this red: the plan rendered once, against the first item, which retries
/// every item once and runs `r: x` (`1,1,1,1`); or an item that does not retry, in a task
/// another item retries, reported without its conditions.
#[test]
fn retries_are_rendered_per_item() {
    let check = |text: &str, tasks: &[&str], changed: usize| {
        for task in tasks {
            assert!(
                text.contains(&format!(r#""msg": "{task} 1,2,-,-""#)),
                "{text}"
            );
            for (left, count) in [(2, 1), (1, 2)] {
                assert_eq!(
                    text.matches(&format!("[localhost]: {task} ({left} retries left)."))
                        .count(),
                    count,
                    "{text}"
                );
            }
        }
        assert_eq!(
            text.matches(
                r#""msg": "Task failed: Error processing keyword 'retries': The value 'x' could not be converted to 'int'.""#
            )
            .count(),
            tasks.len(),
            "{text}"
        );
        assert_eq!(
            text.matches(r#""changed": true"#).count(),
            changed,
            "{text}"
        );
    };
    let out = volant_within(
        &["playbook", &fixture("controller/retries-per-item.yml")],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(0), "{text}");
    check(&text, &["remote", "local"], 0);
    // `copy` names a Python module, which the run builds before it starts.
    let Some(python) = python::reference_python().map(|p| p.display().to_string()) else {
        return;
    };
    let out = volant_within_env(
        &[
            "playbook",
            &fixture("controller/retries-per-item-plugin.yml"),
        ],
        std::time::Duration::from_secs(120),
        &[("VOLANT_PYTHON", &python)],
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(0), "{text}");
    check(&text, &["plugin"], 1);
}

/// `retries` and `delay` are converted as the reference converts them, and a `delay` its
/// `time.sleep` refuses fails the item after the retry line. Measured on ansible-core 2.19.12
/// with this fixture, for both tasks: `nan` and `1e20` each print `(1 retries left)` then fail
/// with `Task failed: Invalid value NaN (not a number)` and `Task failed: timestamp out of range
/// for C PyTime_t`; `delay: x` fails `... could not be converted to 'float'.`; `retries: '2.5'`
/// fails `... could not be converted to 'int'.`; `retries: true` retries once.
///
/// What would make this red: the huge delay waited out (the test's deadline) or NaN taken for
/// it; `'int'` in the `delay` message; or `'2.5'` truncated into two retries.
#[test]
fn retries_and_delay_are_converted_as_the_reference_does() {
    let out = volant_within(
        &["playbook", &fixture("controller/retry-values.yml")],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(0), "{text}");
    for task in ["remote", "local"] {
        assert_eq!(
            text.matches(&format!("[localhost]: {task} (1 retries left)."))
                .count(),
            3,
            "{text}"
        );
    }
    for msg in [
        "Task failed: Invalid value NaN (not a number)",
        "Task failed: timestamp out of range for C PyTime_t",
        "Task failed: Error processing keyword 'delay': The value 'x' could not be converted to 'float'.",
        "Task failed: Error processing keyword 'retries': The value '2.5' could not be converted to 'int'.",
    ] {
        assert_eq!(
            text.matches(&format!(r#""msg": "{msg}""#)).count(),
            2,
            "{msg}: {text}"
        );
    }
}

/// A `timeout` over 100000000 fails each item that runs, on the agent path and the controller
/// side alike, and the play goes on. Measured on ansible-core 2.19.12 with this fixture: both
/// items of the `command` loop fail with `Task failed: Timeout 9223372036854775807 is invalid,
/// it must be between 0 and 100000000.`, the `debug` with the same sentence for 100000001, and
/// `after` runs.
///
/// What would make this red: the timeout taken as it stands, whose deadline overflows and
/// panics the agent (the host then reads unreachable), or runs the `debug`.
#[test]
fn a_timeout_out_of_range_fails_the_task() {
    let out = volant_within(
        &["playbook", &fixture("controller/timeout-out-of-range.yml")],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert_eq!(
        text.matches(r#""msg": "Task failed: Timeout 9223372036854775807 is invalid, it must be between 0 and 100000000.""#)
            .count(),
        2,
        "{text}"
    );
    assert!(
        text.contains(r#""msg": "Task failed: Timeout 100000001 is invalid, it must be between 0 and 100000000.""#),
        "{text}"
    );
    assert!(!text.contains(r#""msg": "x""#), "{text}");
    assert!(text.contains(r#""msg": "after""#), "{text}");
}

/// Measured on ansible-core 2.19.12: a `pause` that asks for an answer without a terminal shows
/// `[WARNING]: Not waiting for response to prompt as stdin is not interactive` once, and its
/// registered result has no `warnings` key. The driver shows every result's `warnings` that way,
/// a module's as well as a controller-side action's.
///
/// What would make this red: the warnings left in the result, which prints nothing and registers
/// a key the reference does not.
#[test]
fn a_result_s_warnings_are_shown_and_not_registered() {
    let out = volant_within_full(
        &["playbook", &fixture("controller/pause-warning.yml")],
        DEFAULT_DEADLINE,
        None,
        None,
        None,
        Some(""),
        &[],
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let errors = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}{errors}");
    assert_eq!(
        errors
            .matches("[WARNING]: Not waiting for response to prompt as stdin is not interactive")
            .count(),
        1,
        "{text}{errors}"
    );
    assert!(
        text.contains(r#""q.warnings": "VARIABLE IS NOT DEFINED!"#),
        "{text}"
    );
}

/// Measured on ansible-core 2.19.12: `timeout` applies to a controller-side action as it does
/// to a module, and a `pause: {seconds: 5}` under `timeout: 1` fails after one second with the
/// message `command` gives. `frame` is left out, as the agent leaves it out.
///
/// What would make this red: the keyword ignored on the controller, which waits the five
/// seconds and reports `ok`.
#[test]
fn a_task_timeout_ends_a_controller_side_action() {
    let out = volant_within(
        &["playbook", &fixture("controller/pause-timeout.yml")],
        std::time::Duration::from_secs(30),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(
        text.contains("fatal: [localhost]: FAILED! => ")
            && text.contains(
                r#""msg": "Task failed: Timed out after 1 second(s).", "timedout": {"period": 1}}"#
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
        text.contains("(item=a) => {\n    \"msg\": \"greet a\"\n}")
            && text.contains("(item=b) => {\n    \"msg\": \"greet b\"\n}"),
        "a play var naming the loop variable renders per item: {text}"
    );
    assert!(
        text.contains("ok: [alpha] => {\n    \"msg\": \"hello there\"\n}"),
        "a templated value read through hostvars renders: {text}"
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
fn ansible_cfg_supplies_the_inventory_when_none_is_given() {
    let out = volant_within_env(
        &["playbook", &fixture("cfg/site.yml")],
        DEFAULT_DEADLINE,
        &[("ANSIBLE_CONFIG", &fixture("cfg/ansible.cfg"))],
    );
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
    let out = volant_within_env(
        &["playbook", &fixture("cfg/site.yml")],
        DEFAULT_DEADLINE,
        &[("ANSIBLE_CONFIG", &cfg.display().to_string())],
    );
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

/// An `ansible_connection` naming no plugin fails that host's task, as ansible-core 2.19.12 does,
/// and the other hosts run on.
#[test]
fn an_unknown_connection_fails_that_host_s_task_not_the_run() {
    let out = volant(&[
        "playbook",
        "-i",
        &fixture("bad-connection.ini"),
        &fixture("all-fail.yml"),
    ]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains(
            r#"fatal: [bad]: FAILED! => {"changed": false, "msg": "Task failed: the connection plugin 'carrier_pigeon' was not found"}"#
        ),
        "{text}"
    );
    assert!(
        text.contains("PLAY RECAP"),
        "the run must finish with a recap: {text}"
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "both hosts failed a task: {text}"
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
    let inv: String = (1..=6).fold(String::new(), |mut out, i| {
        let _ = writeln!(out, "h{i} ansible_connection=local");
        out
    });
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

/// A run setting nobody can use is refused before any play, with the reference's code and words;
/// a `remote_tmp` whose `~` names a user is not one of them.
#[rstest]
#[case::zero_forks(&["-f", "0"], &[], 2, &["The number of processes (--forks) must be >= 1"])]
#[case::unparsable_forks_in_ansible_cfg(
    &[],
    &[("ANSIBLE_CONFIG", "cfg/bad-forks.cfg")],
    5,
    &["Config 'DEFAULT_FORKS'", "Invalid value provided for 'integer'"]
)]
#[case::a_substitution_in_remote_tmp_s_home_part(
    &[],
    &[("ANSIBLE_CONFIG", "cfg/bad-remote-tmp.cfg")],
    5,
    &["~$(id)/x"]
)]
#[case::a_bare_tilde(&[], &[("ANSIBLE_CONFIG", "cfg/ansible.cfg"), ("ANSIBLE_REMOTE_TMP", "~")], 0, &[])]
#[case::a_tilde_user(&[], &[("ANSIBLE_CONFIG", "cfg/ansible.cfg"), ("ANSIBLE_REMOTE_TMP", "~root")], 0, &[])]
#[case::a_dotted_tilde_user(
    &[],
    &[("ANSIBLE_CONFIG", "cfg/ansible.cfg"), ("ANSIBLE_REMOTE_TMP", "~some.user-1")],
    0,
    &[]
)]
fn a_run_setting_is_refused_before_any_play_or_honoured(
    #[case] flags: &[&str],
    #[case] envs: &[(&str, &str)],
    #[case] code: i32,
    #[case] says: &[&str],
) {
    // A configuration file is a fixture; any other value is passed as written.
    let envs: Vec<(&str, String)> = envs
        .iter()
        .map(|(k, v)| {
            let v = if v.ends_with(".cfg") {
                fixture(v)
            } else {
                (*v).to_string()
            };
            (*k, v)
        })
        .collect();
    let envs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut args = vec!["playbook"];
    args.extend_from_slice(flags);
    let book = if flags.is_empty() {
        fixture("cfg/site.yml")
    } else {
        fixture("site.yml")
    };
    args.push(&book);
    let out = volant_within_env(&args, DEFAULT_DEADLINE, &envs);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(code), "{stdout}\n{stderr}");
    for words in says {
        assert!(stderr.contains(words), "{stderr}");
    }
    if code != 0 {
        assert!(!stdout.contains("PLAY"), "a refusal runs no play: {stdout}");
    }
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

/// A `python3` on `PATH` with no ansible-core behind it, so a probe that needs the controller to
/// have none reads the same on a machine that has one.
fn fake_python3_without_ansible(name: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("volant-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let python = dir.join("python3");
    std::fs::write(&python, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
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

/// Six escalated tasks, each closing its batch with a `register`. The escalated agent opens
/// once, at either setting: the link belongs to the host it runs on and not to the fork permit,
/// so the permit goes back in front of every wait while the link stays with the driver.
///
/// Strict `linear` is what makes the second half worth asserting. It raises a boundary in front
/// of nearly every task, so a link released with the permit was rebuilt per task -- an `ssh`, a
/// `sudo` and a handshake each time, which is what made an escalated play slower than the
/// reference it is meant to beat. Both settings are read here rather than in two tests, because
/// they are one mechanism at its two settings and a second copy would only restate the first.
///
/// Counted through a `sudo` that records every invocation and then runs the command it was
/// given as the invoking user: the count is what is being measured, and needing real privileges
/// to measure it would make this a test of the machine.
///
/// What would make this red: the driver releasing its escalated links with the fork permit
/// again, which puts six launches in the log under the strict default; or handing them back
/// after every batch whatever the setting, which puts six in both.
#[test]
fn escalated_links_survive_a_batch_that_a_register_closed() {
    let escalations = |envs: &[(&str, &str)]| {
        let dir = fake_sudo(
            "countsudo",
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$VOLANT_SUDO_LOG\"\n\
             while [ \"$1\" != \"--\" ]; do shift; done\nshift\nexec \"$@\"\n",
        );
        let log = dir.join("sudo.log");
        let mut all = vec![("VOLANT_SUDO_LOG", log.display().to_string())];
        all.extend(envs.iter().map(|(k, v)| (*k, (*v).to_string())));
        let borrowed: Vec<(&str, &str)> = all.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let out = volant_within_with_path(
            &["playbook", &fixture("become-registered.yml")],
            DEFAULT_DEADLINE,
            Some(&dir),
            &borrowed,
        );
        let text = String::from_utf8(out.stdout).unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{text}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let recorded = std::fs::read_to_string(&log).expect("the fake sudo wrote its log");
        // The version probe settles which `sudo` form the link uses and is not a launch of the
        // agent itself; everything else in the log is one.
        let launches = recorded
            .lines()
            .filter(|line| !line.contains("--version"))
            .count();
        let _ = std::fs::remove_dir_all(&dir);
        (launches, recorded)
    };

    let (batched, recorded) = escalations(&[("VOLANT_BATCHING", "1")]);
    assert_eq!(
        batched, 1,
        "one escalated agent for six tasks with batching:\n{recorded}"
    );
    let (strict, recorded) = escalations(&[]);
    assert_eq!(
        strict, 1,
        "one escalated agent for six tasks under the strict default too:\n{recorded}"
    );
}

/// One fork, two hosts, and a step waiting for the other host right behind a `register` that
/// ended a batch: the run finishes, with and without batching.
#[rstest]
fn a_barrier_behind_a_registered_task_opens_with_one_fork(#[values(false, true)] batching: bool) {
    let envs: &[(&str, &str)] = if batching {
        &[("VOLANT_BATCHING", "1")]
    } else {
        &[]
    };
    let out = volant_within_env(
        &[
            "playbook",
            "-i",
            &fixture("vars/inventory.ini"),
            "-f",
            "1",
            &fixture("hostvars-barrier.yml"),
        ],
        std::time::Duration::from_secs(20),
        envs,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("stamped-alpha"), "beta read alpha: {text}");
    assert_eq!(
        text.matches("failed=0").count(),
        2,
        "both hosts reach the recap: {text}"
    );
}

/// One fork, two hosts, and a flush point behind a `register` that ended a batch: the run
/// finishes, with and without batching.
#[rstest]
fn a_flush_point_behind_a_registered_task_opens_with_one_fork(
    #[values(false, true)] batching: bool,
) {
    let envs: &[(&str, &str)] = if batching {
        &[("VOLANT_BATCHING", "1")]
    } else {
        &[]
    };
    let out = volant_within_env(
        &[
            "playbook",
            "-i",
            &fixture("handlers/inv.ini"),
            "-f",
            "1",
            &fixture("handlers/flush-behind-a-register.yml"),
        ],
        std::time::Duration::from_secs(20),
        envs,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        text.matches("handler-ran").count(),
        2,
        "the handler runs on both hosts: {text}"
    );
    assert_eq!(
        text.matches("ok=3").count(),
        2,
        "both hosts reach the recap: {text}"
    );
}

/// An escalation that cannot happen fails the task, never the host: a `sudo` wanting a password,
/// one refusing the password given, and no `sudo` at all.
#[rstest]
#[case::a_missing_password(
    Some("#!/bin/sh\necho 'sudo: a password is required' >&2\nexit 1\n"),
    None,
    volant::transport::MISSING_SUDO_PASSWORD
)]
#[case::a_rejected_password(
    Some("#!/bin/sh\ncat > /dev/null\necho 'sudo: Sorry, try again.' >&2\nexit 1\n"),
    Some("not-the-password\n"),
    volant::transport::INCORRECT_SUDO_PASSWORD
)]
#[case::no_sudo_at_all(None, None, "sudo")]
fn an_escalation_that_cannot_happen_fails_the_task(
    #[case] sudo: Option<&str>,
    #[case] password: Option<&str>,
    #[case] says: &str,
) {
    let dir = if let Some(body) = sudo {
        fake_sudo(&format!("sudo-{}", says.len()), body)
    } else {
        let dir = std::env::temp_dir().join(format!("volant-nosudo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    };
    let book = fixture("become.yml");
    let args: Vec<&str> = if password.is_some() {
        vec!["playbook", "-K", &book]
    } else {
        vec!["playbook", &book]
    };
    // A fake `sudo` goes in front of the real one; with none, `PATH` holds nothing at all.
    let (front, only) = if sudo.is_some() {
        (Some(dir.as_path()), None)
    } else {
        (None, Some(dir.as_path()))
    };
    let out = volant_within_full(&args, DEFAULT_DEADLINE, front, only, None, password, &[]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("fatal: [localhost]: FAILED!") && text.contains(says),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !text.contains("UNREACHABLE"),
        "escalation failure is a task failure: {text}"
    );
    assert_eq!(out.status.code(), Some(2), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `become_method` other than `sudo` stops the run before anything executes, with the method named.
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
    let out = volant_within_env(
        &["playbook", &fixture("become.yml")],
        DEFAULT_DEADLINE,
        &[("ANSIBLE_BECOME_METHOD", "doas")],
    );
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
    let out = volant_within_env(
        &["playbook", &plain.display().to_string()],
        DEFAULT_DEADLINE,
        &[("ANSIBLE_BECOME_METHOD", "doas")],
    );
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

/// Runs `volant` with `dir` first on `PATH` and one line written to stdin, which is where `-K`
/// reads from. The line is a fixture, never a credential: the fake `sudo` these tests put on
/// `PATH` rejects whatever it is given.
fn volant_feeding_stdin(args: &[&str], dir: &Path, line: &str) -> Output {
    volant_within_full(
        args,
        DEFAULT_DEADLINE,
        Some(dir),
        None,
        None,
        Some(line),
        &[],
    )
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
    let out = volant_feeding_stdin(
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
    let out = volant_feeding_stdin(
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

/// A directory of its own, an inventory of two local hosts, and the file they append to, for
/// the two order tests below. `slug` keeps the two apart, since they share a process.
fn two_hosts_and_a_trace(slug: &str) -> (std::path::PathBuf, String, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("volant-{slug}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temp dir");
    let inventory = dir.join("inv.ini");
    std::fs::write(
        &inventory,
        "alpha ansible_connection=local\nbeta ansible_connection=local\n",
    )
    .expect("an inventory");
    let trace = dir.join("trace");
    (dir.clone(), inventory.display().to_string(), trace)
}

/// The trace the fixture wrote, split into the host that appended each line and the task that
/// made it. Which of the two hosts is served first is a race -- with one fork they queue on the
/// same permit and either may take it -- so what a test asserts is the shape of the sequence,
/// never the names in it.
fn trace_steps(trace: &Path) -> Vec<(String, String)> {
    std::fs::read_to_string(trace)
        .expect("the trace")
        .lines()
        .map(|line| {
            let (host, step) = line.rsplit_once('-').expect("a host-step line");
            (host.to_string(), step.to_string())
        })
        .collect()
}

/// `linear` means the hosts of a batch meet in front of every task. Two hosts append a line
/// each to one file, twice; with one fork the order is fully determined, and it says which
/// engine ran.
///
/// The assertion is the file's contents, not the terminal: the coordinator reorders what it
/// prints to keep the display in task order, so a run whose execution interleaved wrongly can
/// still print in the right order. Only a shared effect shows the difference.
///
/// What would make this red: a task running ahead of another host's previous task, which is
/// what this release did whenever no task mentioned `hostvars`.
#[test]
fn every_task_is_a_barrier_by_default() {
    let (dir, inventory, trace) = two_hosts_and_a_trace("order-strict");

    let out = volant_within(
        &[
            "playbook",
            "-i",
            &inventory,
            "-f",
            "1",
            "-e",
            &format!("trace={}", trace.display()),
            &fixture("order/shared-file.yml"),
        ],
        std::time::Duration::from_secs(60),
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let steps = trace_steps(&trace);
    let order: Vec<&str> = steps.iter().map(|(_, step)| step.as_str()).collect();
    assert_eq!(
        order,
        ["1", "1", "2", "2"],
        "both hosts must finish task 1 before either starts task 2: {steps:?}"
    );
    assert_ne!(
        steps[0].0, steps[1].0,
        "one line per host per task: {steps:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The other direction: with batching asked for, a host carries on through the tasks between
/// two barriers, which is the whole point of the option. Same fixture, same forks, different
/// order -- so this test is what proves the switch is wired rather than ignored.
///
/// What would make this red: `[volant] batching` or `VOLANT_BATCHING` read and dropped, which
/// would leave the engine strict in both modes and the measured cost of the option unexplained.
#[test]
fn batching_lets_a_host_run_ahead_when_it_is_asked_for() {
    let (dir, inventory, trace) = two_hosts_and_a_trace("order-batching");

    let out = volant_within_env(
        &[
            "playbook",
            "-i",
            &inventory,
            "-f",
            "1",
            "-e",
            &format!("trace={}", trace.display()),
            &fixture("order/shared-file.yml"),
        ],
        std::time::Duration::from_secs(60),
        &[("VOLANT_BATCHING", "1")],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let steps = trace_steps(&trace);
    let order: Vec<&str> = steps.iter().map(|(_, step)| step.as_str()).collect();
    assert_eq!(
        order,
        ["1", "2", "1", "2"],
        "a host asked to batch runs both of its tasks before the next host starts: {steps:?}"
    );
    assert_eq!(
        steps[0].0, steps[1].0,
        "the same host wrote both: {steps:?}"
    );
    assert_ne!(steps[0].0, steps[2].0, "then the other one did: {steps:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A templated task name shows what it rendered to when the step ran, not what the host's
/// variables say by the time the banner prints. Under batching `alpha` runs past the banner's
/// step and through the `set_fact` behind it while `beta` sleeps, and the coordinator cannot
/// print that step before `beta` is done with the one in front of it.
///
/// What would make this red: the banner rendered at print time, which reads `release` after
/// `alpha`'s `set_fact` and prints `Deploy 2` where the reference prints `Deploy 1`.
#[test]
fn a_banner_shows_the_name_as_it_rendered_at_its_step() {
    let (dir, inventory, _) = two_hosts_and_a_trace("banner-at-its-step");
    let play = dir.join("play.yml");
    std::fs::write(
        &play,
        "- hosts: all\n  gather_facts: false\n  vars:\n    release: 1\n  tasks:\n\
         \x20   - name: Beta is slow\n      command: sleep 2\n      when: inventory_hostname == 'beta'\n\
         \x20   - name: \"Deploy {{ release }}\"\n      debug:\n        msg: deploying\n\
         \x20   - set_fact:\n        release: 2\n",
    )
    .expect("a playbook");
    let out = volant_within_env(
        &["playbook", "-i", &inventory, play.to_str().expect("a path")],
        std::time::Duration::from_secs(60),
        &[("VOLANT_BATCHING", "1")],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(stdout.contains("TASK [Deploy 1]"), "{stdout}");
    assert!(!stdout.contains("Deploy 2"), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A lookup reads the controller when its task renders, and every task of a batch is rendered
/// before the batch goes out, so a task calling one starts its own batch. Here the `shell` in
/// front of it rewrites the file the lookup reads.
///
/// What would make this red: the lookup rendered in the same batch as the `shell`, which reads
/// the file before the `shell` ran and writes `old`.
#[test]
fn a_lookup_reads_the_controller_after_the_tasks_in_front_of_it() {
    let Some(python) = python::reference_python().map(|p| p.display().to_string()) else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("volant-lookup-cut-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    std::fs::write(dir.join("key"), "old\n").expect("a file");
    std::fs::write(dir.join("inv.ini"), "probe ansible_connection=local\n").expect("an inventory");
    let play = dir.join("play.yml");
    std::fs::write(
        &play,
        "- hosts: all\n  gather_facts: false\n  tasks:\n\
         \x20   - shell: \"echo new > {{ dir }}/key\"\n\
         \x20   - shell: \"echo {{ lookup('file', dir ~ '/key') }} > {{ dir }}/out\"\n",
    )
    .expect("a playbook");
    let out = volant_within_env(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            "-e",
            &format!("dir={}", dir.display()),
            play.to_str().expect("a path"),
        ],
        std::time::Duration::from_secs(120),
        &[("VOLANT_BATCHING", "1"), ("VOLANT_PYTHON", &python)],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let written = std::fs::read_to_string(dir.join("out")).expect("the shell wrote its file");
    assert_eq!(written.trim(), "new");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every task of a batch is rendered before the batch goes out, so a task that writes facts
/// ends its batch. Here the second `setup` reads a custom fact the `copy` in front of it just
/// changed, and the `shell` behind it writes what it rendered; in one batch with the `setup`,
/// it would render the fact the first `setup` gathered.
///
/// What would make this red: the batch not ending behind a fact-writing module, which writes
/// `old`.
#[test]
fn a_task_behind_a_fact_writing_module_renders_the_new_facts() {
    let Some(python) = python::reference_python().map(|p| p.display().to_string()) else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("volant-facts-cut-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("facts.d")).expect("a facts directory");
    std::fs::write(dir.join("facts.d/probe.fact"), "{\"v\": \"old\"}\n").expect("a fact");
    std::fs::write(dir.join("inv.ini"), "probe ansible_connection=local\n").expect("an inventory");
    let play = dir.join("play.yml");
    std::fs::write(
        &play,
        "- hosts: all\n  gather_facts: false\n  tasks:\n\
         \x20   - setup:\n        gather_subset: ['!all', '!min', 'local']\n        fact_path: \"{{ dir }}/facts.d\"\n\
         \x20   - copy:\n        content: '{\"v\": \"new\"}'\n        dest: \"{{ dir }}/facts.d/probe.fact\"\n\
         \x20   - setup:\n        gather_subset: ['!all', '!min', 'local']\n        fact_path: \"{{ dir }}/facts.d\"\n\
         \x20   - shell: \"echo {{ ansible_local.probe.v }} > {{ dir }}/out\"\n",
    )
    .expect("a playbook");
    let out = volant_within_env(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            "-e",
            &format!("dir={}", dir.display()),
            // The Python `setup`: `fact_path` is its own, and the native collector hands back
            // on it or leaves `ansible_local` out.
            "--facts",
            "python",
            play.to_str().expect("a path"),
        ],
        std::time::Duration::from_secs(120),
        &[("VOLANT_BATCHING", "1"), ("VOLANT_PYTHON", &python)],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let written = std::fs::read_to_string(dir.join("out")).expect("the shell wrote its file");
    assert_eq!(written.trim(), "new");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Measured against the reference: `beta` reads the stamp `alpha` registered at the previous
/// task, and both hosts see each other in the play's live list.
#[rstest]
fn a_host_reading_hostvars_waits_for_the_others(#[values(false, true)] batching: bool) {
    let envs: &[(&str, &str)] = if batching {
        &[("VOLANT_BATCHING", "1")]
    } else {
        &[]
    };
    // Batching lets a host run ahead, so its race is given ten chances to show.
    for _ in 0..if batching { 10 } else { 1 } {
        let out = volant_within_env(
            &[
                "playbook",
                "-i",
                &fixture("vars/inventory.ini"),
                &fixture("hostvars-barrier.yml"),
            ],
            std::time::Duration::from_secs(20),
            envs,
        );
        let text = String::from_utf8(out.stdout).unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{text}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            text.contains("ok: [beta] => {\n    \"msg\": \"stamped-alpha\"\n}"),
            "{text}"
        );
        assert!(
            text.contains("ok: [alpha] => {\n    \"msg\": \"alpha,beta of alpha,beta\"\n}")
                && text.contains("ok: [beta] => {\n    \"msg\": \"alpha,beta of alpha,beta\"\n}"),
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
        text.contains("ok: [beta] => {\n    \"msg\": \"beta of alpha,beta\"\n}"),
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
    assert!(
        text.contains("ok: [beta] => {\n    \"msg\": \"beta\"\n}"),
        "{text}"
    );
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
        "[web]\nalpha ansible_connection=ssh ansible_host=127.0.0.1 ansible_port=1\nbeta ansible_connection=local\n",
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
    assert!(
        text.contains("ok: [beta] => {\n    \"msg\": \"beta\"\n}"),
        "{text}"
    );
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
        section(&text, "What the empty loop registered").contains(
            "ok: [localhost] => {\n    \"msg\": \"reason=No items in the list items=0\"\n}"
        ),
        "the registered value carries the reference's reason and no items: {text}"
    );
    assert!(
        section(&text, "The empty loop registered a skip")
            .contains("ok: [localhost] => {\n    \"msg\": \"the empty loop was skipped\"\n}"),
        "`when: empty.skipped` holds: {text}"
    );
}

/// A loop over an undefined variable under a block whose `when` is false is skipped, not failed.
/// Measured on ansible-core 2.19.12 with the first task below alone (`k3s-ansible`'s `Copy
/// manifests` under `when: extra_manifests is defined` has this shape): `skipping: [h]`, `ok=0
/// failed=0 skipped=1`. Its `task_executor.py` keeps the loop's undefined error, runs the task
/// once through `_execute` with no item, and raises the error only once the conditional holds.
/// So the task is a plain one there: one `skipping:` line, the plain skip registered with no
/// `results`, an include counted like any skipped task, and the same loop under a true `when`
/// fails on the undefined variable.
///
/// What would make this red: the loop rendered before the `when` is read, which fails the first
/// case with `undefined variable`; the task still reported as a loop, which prints a second
/// `skipping:` line, registers `msg` and `results`, and counts the include as nothing; or the
/// kept error dropped once the `when` holds, which runs the task with no items.
#[test]
fn an_undefined_loop_under_a_false_when_is_skipped() {
    let dir = std::env::temp_dir().join(format!("volant-loop-when-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("other.yml"), "- debug: { msg: included }\n").unwrap();
    let play = |gate: &str, list: &str| {
        format!(
            "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - when: {gate}\n      block:\n        - name: Copy\n          debug: {{ msg: ran }}\n          loop: {list}\n          register: r\n        - name: Inc\n          include_tasks: other.yml\n          loop: {list}\n    - name: Show\n      debug: {{ msg: \"{{{{ r | list | sort | join(',') }}}}\" }}\n"
        )
    };
    let run = |name: &str, body: String| {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        let out = volant_within(&["playbook", &path.display().to_string()], PROBE_DEADLINE);
        let text = String::from_utf8(out.stdout).unwrap();
        (out.status.code(), text)
    };
    // The first line is what is left of the banner's row of stars.
    let lines = |text: &str, task: &str| -> Vec<String> {
        section(text, task)
            .lines()
            .skip(1)
            .map(|line| line.trim_end().to_string())
            .filter(|line| !line.is_empty())
            .collect()
    };

    let (code, text) = run("false.yml", play("nope is defined", "\"{{ nope }}\""));
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(lines(&text, "Copy"), ["skipping: [localhost]"], "{text}");
    assert_eq!(lines(&text, "Inc"), ["skipping: [localhost]"], "{text}");
    assert!(
        text.contains(r#""msg": "changed,false_condition,skip_reason,skipped""#),
        "the plain skip is registered: {text}"
    );
    assert!(
        text.contains("ok=1    changed=0    unreachable=0    failed=0    skipped=2"),
        "{text}"
    );

    let (code, text) = run("true.yml", play("nope is not defined", "\"{{ nope }}\""));
    assert_eq!(code, Some(2), "{text}");
    assert!(
        section(&text, "Copy").contains("The task includes an option with an undefined variable."),
        "{text}"
    );

    // A loop that renders keeps reading its `when` per item.
    let (code, text) = run("fine.yml", play("nope is defined", "[a, b]"));
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(
        lines(&text, "Copy"),
        [
            "skipping: [localhost] => (item=a)",
            "skipping: [localhost] => (item=b)",
            "skipping: [localhost]",
        ],
        "{text}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Measured against ansible-core 2.19.12: a loop whose every item a `when` leaves out prints
/// `skipping: [localhost] => (item=a)`, `skipping: [localhost] => (item=b)`, then one more line,
/// `skipping: [localhost]`, for the task itself. `geerlingguy.git`'s `Build git.` does this on
/// a host that already has git.
///
/// What would make this red: the task's own line left out after its items, the two item lines
/// alone.
#[test]
fn a_loop_whose_every_item_is_skipped_prints_the_task_s_skipping_line() {
    let out = volant_within(
        &["playbook", &fixture("loop-all-skipped.yml")],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    // The first line is what is left of the banner's row of stars.
    let lines: Vec<&str> = section(&text, "Every item skipped")
        .lines()
        .skip(1)
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(
        lines,
        [
            "skipping: [localhost] => (item=a)",
            "skipping: [localhost] => (item=b)",
            "skipping: [localhost]",
        ],
        "{text}"
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
            text.contains(&format!(
                "ok: [localhost] => {{\n    \"msg\": \"{msg}\"\n}}"
            )),
            "`{msg}` must be reached: {text}"
        );
    }
    assert!(
        !text.contains("undefined variable"),
        "nothing about the registered debug is undefined: {text}"
    );
    assert!(
        text.contains("ok: [localhost] => {\n    \"msg\": \"hello\"\n}"),
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
        text.contains("ok: [alpha] => {\n    \"msg\": \"reached on alpha\"\n}")
            && text.contains("ok: [beta] => {\n    \"msg\": \"reached on beta\"\n}"),
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

/// The bounded event channel at scale: seventy hosts, seventy forks, each sending several
/// events while the coordinator waits on the slowest one, and every one of them reaching the
/// recap. `volant_within` is what makes a wedge a failure here: an elapsed-time assertion
/// cannot fire on a run that never returns.
#[test]
fn seventy_hosts_finish_with_a_full_recap() {
    let dir = std::env::temp_dir().join(format!("volant-many-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let inv: String = (1..=70).fold(String::new(), |mut out, i| {
        let _ = writeln!(out, "h{i:02} ansible_connection=local");
        out
    });
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

/// A host lost in the middle of a section says so: no rescue, no cleanup, and a recap that names
/// it.
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
// claims: a `Runs` row changes something an operator can see, a `Partial` row changes something
// too and says under the generated table what it leaves out, and a `Preflight` row stops the run
// before anything is printed. The rows owing a proof are walked from the tables themselves,
// everything that is not `Preflight`, rather than written out here, so a row added without its
// proof fails the suite instead of slipping through - which is how a keyword would come to be
// accepted by the loader, waved past the pre-flight and then ignored, the failure this whole
// split exists to prevent.
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
    dir: &Path,
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
    let out = volant_within_with_path(&args, PROBE_DEADLINE, path, &[]);
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
            "throttle",
            "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      command: echo hi\n      throttle: probe\n",
        ),
        (
            "any_errors_fatal",
            "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      block:\n        - command: echo hi\n      any_errors_fatal: probe\n",
        ),
        (
            "order",
            "- hosts: localhost\n  gather_facts: false\n  order: probe\n  tasks:\n    - name: Probe task\n      command: echo hi\n",
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

/// An escalation the host refuses hands its fork back before the driver carries on.
#[test]
fn a_refused_escalation_hands_its_fork_back_before_waiting() {
    let dir = fake_sudo(
        "forkflush",
        "#!/bin/sh\necho 'sudo: a password is required' >&2\nexit 1\n",
    );
    let out = volant_within_with_path(
        &[
            "playbook",
            "-f",
            "1",
            "-i",
            &fixture("handlers/inv.ini"),
            &fixture("handlers/flush-past-a-refused-become.yml"),
        ],
        std::time::Duration::from_secs(20),
        Some(&dir),
        &[],
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        section(&text, "recovered").contains("[h1]")
            && section(&text, "recovered").contains("[h2]"),
        "the refused escalation is a task failure the block's rescue takes: {text}"
    );
    for host in ["h1", "h2"] {
        assert!(
            text.contains(&format!(
                "{host}                         : ok=3    changed=1    unreachable=0    failed=0    skipped=0    rescued=1"
            )),
            "{text}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `notify` naming nothing stops the run with the reference's own sentence and exit 1.
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

/// A templated `ignore_errors`, rendered per host, decides on an ordinary task and on an include
/// statement alike.
#[test]
fn a_templated_ignore_errors_decides_on_tasks_and_include_statements() {
    let run = |cmd: &str, inc: &str| {
        let out = volant_within(
            &[
                "playbook",
                "-i",
                &fixture("include/inv.ini"),
                &fixture("include/ignore-templated.yml"),
                "-e",
                &format!("cmd_lenient={cmd} inc_lenient={inc}"),
            ],
            PROBE_DEADLINE,
        );
        (out.status.code(), String::from_utf8(out.stdout).unwrap())
    };
    let (code, text) = run("true", "true");
    assert_eq!(code, Some(0), "{text}");
    assert!(text.contains("\"msg\": \"after\""), "{text}");
    assert!(text.contains("ignored=2"), "{text}");

    let (code, text) = run("false", "true");
    assert_eq!(code, Some(2), "{text}");
    assert!(!text.contains("a missing file"), "{text}");

    let (code, text) = run("true", "false");
    assert_eq!(code, Some(2), "{text}");
    assert!(!text.contains("\"msg\": \"after\""), "{text}");
    assert!(
        text.contains("failed=1") && text.contains("ignored=1"),
        "{text}"
    );
}

/// A `notify` in an included file naming no handler ends the run at exit 1, whatever
/// `ignore_errors` or a `rescue` says.
#[test]
fn a_notify_in_an_include_naming_no_handler_ends_the_run() {
    for playbook in ["include/notify-typo.yml", "include/notify-typo-rescue.yml"] {
        let out = volant_within(
            &[
                "playbook",
                "-i",
                &fixture("include/inv.ini"),
                &fixture(playbook),
            ],
            PROBE_DEADLINE,
        );
        let text = String::from_utf8(out.stdout).unwrap();
        let errors = String::from_utf8(out.stderr).unwrap();
        assert_eq!(out.status.code(), Some(1), "{playbook}: {text}{errors}");
        assert!(
            errors.contains(
                "The requested handler 'Reoad app' was not found in either the main handlers list nor in the listening handlers list"
            ),
            "{playbook}: {errors}"
        );
        assert!(!text.contains("PLAY RECAP"), "{playbook}: {text}");
        for never in ["reloaded", "rescued", "after", "...ignoring"] {
            assert!(!text.contains(never), "{playbook} printed {never}: {text}");
        }
    }
}

/// A task that failed notifies nothing, so its misspelt handler is never looked up: the reference
/// searches for a handler in the ok branch of `_process_pending_results` alone.
#[test]
fn a_failed_task_s_misspelt_notify_does_not_end_the_run() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("include/inv.ini"),
            &fixture("include/notify-typo-failed.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let errors = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}{errors}");
    assert!(text.contains("...ignoring"), "{text}");
    assert!(text.contains(r#""msg": "after""#), "{text}");
    assert!(!text.contains("reloaded"), "{text}");
}

/// A templated `notify` in an included file is refused by name, as it is in the play, rather than
/// looked up as a handler called `restart {{ svc }}`: the statement fails for the host that...
#[test]
fn a_templated_notify_in_an_include_is_refused_by_name() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("include/inv.ini"),
            &fixture("include/notify-templated.yml"),
            "-e",
            "svc=web",
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let errors = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(2), "{text}{errors}");
    assert!(
        text.contains("a templated 'notify' is not supported yet"),
        "{text}{errors}"
    );
    assert!(!text.contains("TASK [notifies a templated name]"), "{text}");
    assert!(!text.contains("restarted"), "{text}");
}

/// A local failure that steps over an include, with fewer forks than hosts.
#[rstest]
fn a_failure_that_steps_over_an_include_gives_its_fork_permit_back(
    #[values(false, true)] batching: bool,
) {
    let envs: &[(&str, &str)] = if batching {
        &[("VOLANT_BATCHING", "1")]
    } else {
        &[]
    };
    let out = volant_within_env(
        &[
            "playbook",
            "-i",
            &fixture("include/inv.ini"),
            "-f",
            "1",
            &fixture("include/fail-then-splice.yml"),
        ],
        PROBE_DEADLINE,
        envs,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains("rescued on h1") && text.contains("rescued on h2"),
        "both hosts got through the jump and into the rescue: {text}"
    );
    assert!(
        text.contains(
            "h1                         : ok=2    changed=1    unreachable=0    failed=0    skipped=0    rescued=1"
        ) && text.contains(
            "h2                         : ok=2    changed=1    unreachable=0    failed=0    skipped=0    rescued=1"
        ),
        "{text}"
    );
}

/// A flush point stepped over from the step immediately in front of it, with that step still in the
/// batch.
#[test]
fn a_flush_point_right_behind_a_running_task_is_reached_with_the_batch_empty() {
    let out = volant_within(
        &[
            "playbook",
            "--skip-tags",
            "dropped",
            &fixture("handlers/flush-behind-a-task.yml"),
        ],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        handler_section(&text, "the handler").contains("handler-ran"),
        "the notification survives the flush nobody entered: {text}"
    );
    assert!(
        section(&text, "after the block").contains("\"msg\": \"after\""),
        "{text}"
    );
    assert!(
        text.contains(
            "localhost                  : ok=3    changed=1    unreachable=0    failed=0    skipped=0    rescued=0"
        ),
        "{text}"
    );
}

/// `until`, `retries` and `delay`, measured on ansible-core 2.19.12 with this very fixture.
#[test]
fn a_task_retries_until_its_condition_holds_and_counts_its_attempts() {
    let dir = probe_dir("until");
    let marker = dir.join("marker");
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("inventory.ini"),
            "-e",
            &format!("marker={}", marker.display()),
            &fixture("until/until.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");

    // One line only: the second attempt created the file and succeeded.
    assert_eq!(
        text.matches("FAILED - RETRYING: [localhost]: retry until file (3 retries left).")
            .count(),
        1,
        "{text}"
    );
    assert!(text.contains(r#""r.attempts": 2"#), "{text}");

    let never = section(&text, "never succeeds");
    assert!(
        never.contains("FAILED - RETRYING: [localhost]: never succeeds (2 retries left).")
            && never.contains("FAILED - RETRYING: [localhost]: never succeeds (1 retries left).")
            && never.contains(r#""attempts": 2"#)
            && never.contains("...ignoring"),
        "{never}"
    );
    assert!(text.contains(r#""n.attempts": 2"#), "{text}");

    let three = section(&text, "until without retries");
    for left in [3, 2, 1] {
        assert!(
            three.contains(&format!(
                "FAILED - RETRYING: [localhost]: until without retries ({left} retries left)."
            )),
            "{three}"
        );
    }
    assert!(text.contains(r#""d.attempts": 3"#), "{text}");
    assert!(text.contains(r#""e.attempts": 2"#), "{text}");

    let looped = section(&text, "until with loop");
    assert_eq!(
        looped
            .matches("FAILED - RETRYING: [localhost]: until with loop (1 retries left).")
            .count(),
        2,
        "one retry line per item, not one for the loop: {looped}"
    );
    for item in ["a", "b"] {
        let line = looped
            .lines()
            .find(|l| l.contains(&format!("(item={item})")))
            .unwrap_or_else(|| panic!("no line for item {item}: {looped}"));
        assert!(
            line.starts_with("failed: [localhost]") && line.contains(r#""attempts": 1"#),
            "{line}"
        );
    }
    assert!(
        text.contains(
            "localhost                  : ok=9    changed=5    unreachable=0    failed=0    skipped=0    rescued=0    ignored=4"
        ),
        "{text}"
    );
    std::fs::remove_dir_all(&dir).expect("the probe directory is removed");
}

/// A task that never leaves the controller retries by the same loop as any other.
#[test]
fn a_controller_side_task_retries_like_any_other() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("inventory.ini"),
            &fixture("until/local.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    let retried = section(&text, "a controller-side task retries too");
    for left in [2, 1] {
        assert!(
            retried.contains(&format!(
                "FAILED - RETRYING: [localhost]: a controller-side task retries too ({left} retries left)."
            )),
            "{retried}"
        );
    }
    assert!(
        retried.contains("fatal: [localhost]: FAILED! => {\n    \"msg\": \"probe\"\n}")
            && retried.contains("...ignoring"),
        "{retried}"
    );
    assert!(section(&text, "after").contains(r#""msg": 2"#), "{text}");
    assert!(
        text.contains(
            "localhost                  : ok=2    changed=0    unreachable=0    failed=0    skipped=0    rescued=0    ignored=1"
        ),
        "{text}"
    );
}

/// An `until` expression that cannot be evaluated ends the task there.
#[test]
fn an_until_that_cannot_be_evaluated_stops_the_attempts() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("inventory.ini"),
            &fixture("until/condition-error.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    let failing = section(&text, "the condition cannot be evaluated");
    assert!(
        failing.contains("fatal: [localhost]: FAILED!")
            && failing.contains("Task failed: Error while evaluating conditional:")
            && failing.contains("...ignoring"),
        "{failing}"
    );
    assert!(
        !failing.contains("FAILED - RETRYING") && !failing.contains("attempts"),
        "a condition that throws is not a false one: {failing}"
    );
    assert!(
        section(&text, "after").contains(r#""msg": "after""#),
        "{text}"
    );
}

/// `delay` is waited after each failed run but the last, proved from the epoch seconds the
/// attempts record rather than from the clock.
#[test]
fn the_delay_is_waited_between_two_attempts() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("inventory.ini"),
            &fixture("until/delay.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    let seconds = section(&text, "seconds");
    let shown = seconds
        .split_once(r#""msg": ""#)
        .unwrap_or_else(|| panic!("no message in:\n{seconds}"))
        .1;
    let shown = &shown[..shown.find('"').expect("a closing quote")];
    let stamps: Vec<i64> = shown
        .split_whitespace()
        .map(|s| s.parse().expect("an epoch second"))
        .collect();
    assert_eq!(stamps.len(), 3, "{shown}");
    assert!(
        stamps[1] - stamps[0] >= 4,
        "the last of three runs came {} second(s) after the task before, not the two delays of \
         two seconds `retries: 2` waits",
        stamps[1] - stamps[0]
    );
}

/// A `no_log` task's error block keeps its prefixes and censors the rest, for a `fail` message, a
/// failed command and an unreachable host, at every verbosity (the reference leaks the first).
#[rstest]
fn a_no_log_failure_s_error_block_never_shows_the_secret(
    #[values("", "-v", "-vvv")] verbosity: &str,
) {
    let inventory = fixture("nolog/secret-failure.ini");
    let book = fixture("nolog/secret-failure.yml");
    // The secret arrives as an extra var, so the source excerpt under each block cannot show it.
    let mut args = vec![
        "playbook",
        "-i",
        &inventory,
        "-e",
        "password=s3cr3t-pw",
        &book,
    ];
    if !verbosity.is_empty() {
        args.push(verbosity);
    }
    let out = volant_within(&args, DEFAULT_DEADLINE);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains("s3cr3t-pw") && !stderr.contains("s3cr3t-pw"),
        "{verbosity}: the secret reached the terminal:\n{stdout}\n{stderr}"
    );
    for block in [
        "[ERROR]: Task failed: Action failed: <error censored due to no log>",
        "[ERROR]: Task failed: Module failed: <error censored due to no log>",
        "[ERROR]: Task failed: Failed to connect to the host via ssh: <error censored due to no log>",
    ] {
        assert!(stdout.contains(block), "{verbosity}: {block}\n{stdout}");
    }
    assert_eq!(out.status.code(), Some(4), "{stdout}\n{stderr}");
}

/// Every line of a `no_log` task is censored at verbosity 0, `-v` and `-vvv`, and the registered
/// value is not (ansible-core 2.19.12).
#[test]
fn no_log_censors_every_line_and_leaves_the_registered_value_alone() {
    const CENSORED: &str = "the output has been hidden due to the fact that 'no_log: true' was specified for this result";
    let inventory = fixture("inventory.ini");
    let nolog = fixture("nolog.yml");
    for verbosity in ["", "-v", "-vvv"] {
        let mut args = vec!["playbook", "-i", &inventory];
        if !verbosity.is_empty() {
            args.push(verbosity);
        }
        args.push(&nolog);
        let out = volant_within(&args, PROBE_DEADLINE);
        let text = String::from_utf8(out.stdout).unwrap();
        let at = format!(
            "at {}",
            if verbosity.is_empty() {
                "-v0"
            } else {
                verbosity
            }
        );
        assert_eq!(out.status.code(), Some(0), "{at}: {text}");

        let ok = section(&text, "secret ok");
        let failed = section(&text, "secret fails");
        let looped = section(&text, "secret loop");
        let shown = section(&text, "secret debug");
        for (what, body) in [
            ("ok", ok),
            ("failed", failed),
            ("loop", looped),
            ("debug", shown),
        ] {
            assert!(
                !body.contains("secret\"") && !body.contains(r#""msg": "hidden""#),
                "{at}: the {what} line leaked: {body}"
            );
        }
        // The registered variable is the real result, at every verbosity.
        assert!(
            text.contains(r#""stdout": "secret""#),
            "{at}: the register must keep the real value: {text}"
        );
        // From `-vvv` on every body is indented, a censored one too.
        let censored_line = if verbosity == "-vvv" {
            format!(
                "fatal: [localhost]: FAILED! => {{\n    \"censored\": \"{CENSORED}\",\n    \"changed\": true\n}}"
            )
        } else {
            format!(
                r#"fatal: [localhost]: FAILED! => {{"censored": "{CENSORED}", "changed": true}}"#
            )
        };
        assert!(
            failed.contains(&censored_line) && failed.contains("...ignoring"),
            "{at}: {failed}"
        );
        assert_eq!(
            looped
                .matches("changed: [localhost] => (item=(censored due to no_log))")
                .count(),
            2,
            "{at}: {looped}"
        );
        // The section starts with the tail of its own banner, so the body is what follows it.
        let body = |s: &str| {
            s.lines()
                .skip(1)
                .filter(|l| !l.trim().is_empty())
                .collect::<Vec<_>>()
                .join(
                    "
",
                )
        };
        if verbosity.is_empty() {
            assert_eq!(body(ok), "changed: [localhost]", "{ok}");
            assert_eq!(body(shown), "ok: [localhost]", "{shown}");
        } else if verbosity == "-vvv" {
            assert!(
                ok.contains(&format!(
                    "changed: [localhost] => {{\n    \"censored\": \"{CENSORED}\",\n    \"changed\": true\n}}"
                )),
                "{at}: {ok}"
            );
            assert!(
                shown.contains(&format!(
                    "ok: [localhost] => {{\n    \"censored\": \"{CENSORED}\"\n}}"
                )),
                "{at}: {shown}"
            );
        } else {
            assert!(
                ok.contains(&format!(
                    r#"changed: [localhost] => {{"censored": "{CENSORED}", "changed": true}}"#
                )),
                "{at}: {ok}"
            );
            assert!(
                shown.contains(&format!(
                    r#"ok: [localhost] => {{"censored": "{CENSORED}"}}"#
                )),
                "{at}: {shown}"
            );
        }
        assert!(
            text.contains(
                "localhost                  : ok=5    changed=3    unreachable=0    failed=0    skipped=0    rescued=0    ignored=1"
            ),
            "{at}: {text}"
        );
    }
}

/// `environment`: the play's layer and the task's are both in force, the task's winning, and a
/// templated value is rendered (ansible-core 2.19.12).
#[test]
fn environment_layers_merge_with_the_task_winning() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("inventory.ini"),
            &fixture("env.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let errors = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}{errors}");
    assert!(text.contains(r#""e.stdout": "play templated""#), "{text}");
    assert!(text.contains(r#""n.stdout": "42""#), "{text}");
    assert!(
        errors.contains(
            "could not parse environment value, skipping: [{'PLAY_ENV': 'play'}, 'notadict']"
        ),
        "the warning goes to stderr: {errors}"
    );
    assert!(
        section(&text, "env not a dict").contains("changed: [localhost]"),
        "a layer we cannot read does not fail the task: {text}"
    );
    assert!(
        section(&text, "a lookup is not the task's environment").contains(r#""msg": "blk ""#),
        "{text}"
    );
    assert!(
        text.contains(
            "localhost                  : ok=7    changed=4    unreachable=0    failed=0    skipped=0    rescued=0    ignored=0"
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
    // The probe's own playbook, `task-<kw>.yml`, is the one file each lookup can find.
    runs(
        "task",
        "with_fileglob",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      debug:\n        msg: \"{{ item | basename }}\"\n      with_fileglob:\n        - \"{{ playbook_dir }}/task-with_fileglob.y*ml\"\n",
        &[],
        0,
        "\"msg\": \"task-with_fileglob.yml\"",
    ),
    runs(
        "task",
        "with_first_found",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      debug:\n        msg: \"{{ item | basename }}\"\n      with_first_found:\n        - \"{{ playbook_dir }}/nosuch.yml\"\n        - \"{{ playbook_dir }}/task-with_first_found.yml\"\n      loop_control:\n        label: one\n",
        &[],
        0,
        // The found file is the loop's first and only item, straight under the banner: a loop
        // over the terms themselves would show `nosuch.yml` on that line.
        "*
ok: [localhost] => (item=one) => {
    \"msg\": \"task-with_first_found.yml\"
}

",
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
    // `gather_facts: true` is obeyed now rather than answered: the play opens with the
    // reference's `setup`, which the controller builds before the first connection. A controller
    // that cannot build it refuses the run, and that refusal is the observable - it exists only
    // because the play asked for facts, and the same playbook writing `false` exits 0 on the same
    // machine. The probe puts a `python3` with no ansible-core in front of whatever this machine
    // has, so the row reads the same either way.
    runs(
        "play",
        "gather_facts",
        "- hosts: localhost\n  gather_facts: true\n  tasks:\n    - command: echo hi\n",
        &[],
        4,
        "no Python interpreter with ansible-core",
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
    // The retry keywords. `until` is proved by a task that **passes** and is retried anyway, so
    // dropping the keyword leaves a run that succeeds instead of one that fails; `retries` by a
    // count no default produces (three is what `until` alone gives); and `delay` by a value it
    // cannot read, which is the only half of that keyword one process can see - that the wait
    // actually happens is `the_delay_is_waited_between_two_attempts`, which reads three epoch
    // seconds out of the results, the last pair proving the wait after the last attempt too.
    runs(
        "task",
        "until",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      command: \"true\"\n      until: false\n      retries: 2\n      delay: 0\n",
        &[],
        2,
        "FAILED - RETRYING: [localhost]: Probe task (1 retries left).",
    ),
    runs(
        "task",
        "retries",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      command: \"true\"\n      until: false\n      retries: 5\n      delay: 0\n      ignore_errors: true\n",
        &[],
        0,
        "(5 retries left).",
    ),
    runs(
        "task",
        "delay",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      command: \"true\"\n      retries: 1\n      delay: notanumber\n",
        &[],
        2,
        "Error processing keyword 'delay'",
    ),
    // `no_log` at every level: the failing task's own line carries the censored body at
    // verbosity 0, so dropping the keyword puts the module's real result there instead.
    runs(
        "task",
        "no_log",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      command: \"false\"\n      no_log: true\n      ignore_errors: true\n",
        &[],
        0,
        "the output has been hidden due to the fact",
    ),
    runs(
        "play",
        "no_log",
        "- hosts: localhost\n  gather_facts: false\n  no_log: true\n  tasks:\n    - name: Probe task\n      command: \"false\"\n      ignore_errors: true\n",
        &[],
        0,
        "the output has been hidden due to the fact",
    ),
    runs(
        "block",
        "no_log",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - block:\n        - name: Probe task\n          command: \"false\"\n          ignore_errors: true\n      no_log: true\n",
        &[],
        0,
        "the output has been hidden due to the fact",
    ),
    // `environment` at every level, each written where only that level can supply the value:
    // dropping the layer leaves the task echoing an empty string.
    runs(
        "task",
        "environment",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      shell: echo \"$PROBE_ENV\"\n      environment:\n        PROBE_ENV: task-value\n      register: e\n    - debug:\n        msg: \"{{ e.stdout }}\"\n",
        &[],
        0,
        "\"msg\": \"task-value\"",
    ),
    runs(
        "play",
        "environment",
        "- hosts: localhost\n  gather_facts: false\n  environment:\n    PROBE_ENV: play-value\n  tasks:\n    - name: Probe task\n      shell: echo \"$PROBE_ENV\"\n      register: e\n    - debug:\n        msg: \"{{ e.stdout }}\"\n",
        &[],
        0,
        "\"msg\": \"play-value\"",
    ),
    runs(
        "block",
        "environment",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - block:\n        - name: Probe task\n          shell: echo \"$PROBE_ENV\"\n          register: e\n        - debug:\n            msg: \"{{ e.stdout }}\"\n      environment:\n        PROBE_ENV: block-value\n",
        &[],
        0,
        "\"msg\": \"block-value\"",
    ),
    // `check_mode` is honoured for `false` and refused by its value for `true`, the way
    // `become_method` and `strategy` are. The row would still be `Runs` with the refusal gone,
    // so what these three assert is the sentence that names the value: parked again, the message
    // loses its `with 'true'` and the probe reddens.
    //
    // The play and block bodies carry no task at all. A task would inherit the merged
    // `check_mode` and get caught by the task-level guard in the second pre-flight pass
    // (`check_steps`, over the compiled steps) regardless of whether the play's or block's own
    // guard still runs - which is exactly why an empty body is the shape that isolates each one:
    // with nothing to merge into, only the guard being probed can refuse the run.
    runs(
        "task",
        "check_mode",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      command: echo hi\n      check_mode: true\n",
        &[],
        4,
        "'check_mode' is not supported yet with 'true'",
    ),
    runs(
        "play",
        "check_mode",
        "- hosts: localhost\n  gather_facts: false\n  check_mode: true\n  tasks: []\n",
        &[],
        4,
        "'check_mode' is not supported yet with 'true'",
    ),
    runs(
        "block",
        "check_mode",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - block: []\n      check_mode: true\n",
        &[],
        4,
        "'check_mode' is not supported yet with 'true'",
    ),
    // `serial` cuts the two hosts of the probe's own inventory into two batches, and each one
    // reads `ansible_play_batch` as itself alone. Without the keyword the play is one batch and
    // both hosts print `h1,h2`, which carries neither expected line.
    runs(
        "play",
        "serial",
        "- hosts: all\n  gather_facts: false\n  serial: 1\n  tasks:\n    - name: Probe task\n      debug:\n        msg: \"{{ ansible_play_batch | join(',') }}\"\n",
        &[],
        0,
        "\"msg\": \"h2\"",
    ),
    // `run_once` runs the task on the first live host of the batch and hands its registered
    // variable to every other one. Without the keyword both hosts run it and each registers its
    // own name, so `h1 on h2` never appears; with it, h2 reads what h1 registered.
    runs(
        "task",
        "run_once",
        "- hosts: all\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      command: echo \"{{ inventory_hostname }}\"\n      run_once: true\n      register: o\n    - name: Probe read\n      debug:\n        msg: \"{{ o.stdout }} on {{ inventory_hostname }}\"\n",
        &[],
        0,
        "\"msg\": \"h1 on h2\"",
    ),
    runs(
        "block",
        "run_once",
        "- hosts: all\n  gather_facts: false\n  tasks:\n    - block:\n        - name: Probe task\n          command: echo \"{{ inventory_hostname }}\"\n          register: o\n      run_once: true\n    - name: Probe read\n      debug:\n        msg: \"{{ o.stdout }} on {{ inventory_hostname }}\"\n",
        &[],
        0,
        "\"msg\": \"h1 on h2\"",
    ),
    // A play's `run_once` reaches every task under it, the read included, so the read lives in
    // a second play. Measured on ansible-core 2.19.12 with this exact shape: `changed: [h1]`
    // alone in the first play, then `h1 on h1` and `h1 on h2` in the second.
    runs(
        "play",
        "run_once",
        "- hosts: all\n  gather_facts: false\n  run_once: true\n  tasks:\n    - name: Probe task\n      command: echo \"{{ inventory_hostname }}\"\n      register: o\n- hosts: all\n  gather_facts: false\n  tasks:\n    - name: Probe read\n      debug:\n        msg: \"{{ o.stdout }} on {{ inventory_hostname }}\"\n",
        &[],
        0,
        "\"msg\": \"h1 on h2\"",
    ),
    // `delegate_to` moves the task onto another host's connection and says so in the line.
    // Parked again, the task runs on h1 itself and the line has no arrow at all.
    runs(
        "task",
        "delegate_to",
        "- hosts: h1\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      command: echo hi\n      delegate_to: h2\n",
        &[],
        0,
        "changed: [h1 -> h2]",
    ),
    runs(
        "block",
        "delegate_to",
        "- hosts: h1\n  gather_facts: false\n  tasks:\n    - block:\n        - name: Probe task\n          command: echo hi\n      delegate_to: h2\n",
        &[],
        0,
        "changed: [h1 -> h2]",
    ),
    // `delegate_facts` sends what the task set into the delegate's variables instead of into
    // its own host's. Parked again, `hostvars['h2'].probe_fact` is undefined and the line reads
    // `none set`.
    runs(
        "task",
        "delegate_facts",
        "- hosts: h1\n  gather_facts: false\n  tasks:\n    - name: Probe task\n      set_fact:\n        probe_fact: moved\n      delegate_to: h2\n      delegate_facts: true\n    - name: Probe read\n      debug:\n        msg: \"{{ hostvars['h2'].probe_fact | default('none') }} {{ probe_fact | default('set') }}\"\n",
        &[],
        0,
        "\"msg\": \"moved set\"",
    ),
    runs(
        "block",
        "delegate_facts",
        "- hosts: h1\n  gather_facts: false\n  tasks:\n    - block:\n        - name: Probe task\n          set_fact:\n            probe_fact: moved\n      delegate_to: h2\n      delegate_facts: true\n    - name: Probe read\n      debug:\n        msg: \"{{ hostvars['h2'].probe_fact | default('none') }} {{ probe_fact | default('set') }}\"\n",
        &[],
        0,
        "\"msg\": \"moved set\"",
    ),
    runs(
        "play",
        "force_handlers",
        "- hosts: localhost\n  gather_facts: false\n  force_handlers: true\n  handlers:\n    - name: Probe handler\n      debug:\n        msg: forced-handler-ran\n  tasks:\n    - name: Probe task\n      command: echo hi\n      notify: Probe handler\n    - name: Probe failure\n      command: nosuchbinary-volant-probe\n",
        &[],
        2,
        "\"msg\": \"forced-handler-ran\"",
    ),
];

/// Every keyword the table says this release answers -- `Runs` or `Partial` -- has a proof, and
/// every proof belongs to a row.
#[test]
fn every_answered_keyword_has_a_proof_and_every_proof_has_a_row() {
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
            .filter(|k| k.support != Support::Preflight)
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
        ("task", "become" | "become_user") | ("play" | "block", "become_user")
    )
}

#[test]
fn every_runs_keyword_changes_something_observable() {
    let dir = probe_dir("runs");
    let sudo_dir = fake_sudo("runs-become-user", FAKE_SUDO_FOR_BECOME_USER);
    let no_ansible = fake_python3_without_ansible("runs-gather-facts");
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
        let path = match (is_become_user_probe(probe), probe.kw) {
            (true, _) => Some(sudo_dir.as_path()),
            (_, "gather_facts") => Some(no_ansible.as_path()),
            _ => None,
        };
        // `serial` needs more than one host to cut a batch out of, `run_once` needs a second
        // host to hand its result to, and `delegate_to` needs a second host to run on. They run
        // against an inventory of two written beside the probe rather than against the implicit
        // localhost.
        let mut args: Vec<&str> = probe.args.to_vec();
        let inventory = dir.join("serial.inv.ini").display().to_string();
        if matches!(
            probe.kw,
            "serial" | "run_once" | "delegate_to" | "delegate_facts"
        ) {
            std::fs::write(
                &inventory,
                "h1 ansible_connection=local\nh2 ansible_connection=local\n",
            )
            .expect("the probe's inventory is written");
            args.extend(["-i", inventory.as_str()]);
        }
        let (code, text) = run_probe(&dir, &name, &body, &args, path);
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
    std::fs::remove_dir_all(&no_ansible).expect("the fake python directory is removed");
}

/// A loop variable named after one of the values the whole inventory shares renders the item, not
/// the inventory.
#[test]
fn a_loop_variable_wins_over_the_inventory_wide_value_it_is_named_after() {
    let dir = probe_dir("loop-var-shadows-shared");
    let (code, text) = run_probe(
        &dir,
        "shadow",
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - debug:\n        msg: \"{{ play_hosts }}\"\n      loop: [1, 2]\n      loop_control:\n        loop_var: play_hosts\n",
        &[],
        None,
    );
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains(r#""msg": 1"#) && text.contains(r#""msg": 2"#),
        "each item renders itself: {text}"
    );
    assert!(
        !text.contains(r#""msg": ["localhost"]"#),
        "and never the play's host list: {text}"
    );
    std::fs::remove_dir_all(&dir).expect("the probe directory is removed");
}

/// A play's roles, read from the directory beside the playbook and spliced into the step list.
#[test]
fn a_role_is_found_next_to_the_playbook_and_named_in_every_banner() {
    // Through the deadline, not bare: this play carries an `include_role`, so it reaches a
    // splice point and can block there. An assertion on elapsed time cannot fail on a hang.
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("inventory.ini"),
            &fixture("roles/roles.yml"),
        ],
        PROBE_DEADLINE,
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    settings().bind(|| insta::assert_snapshot!(String::from_utf8(out.stdout).unwrap()));
}

/// A role nobody can find stops the run before the first banner, naming every directory it looked
/// in, and exits **1**.
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

/// A role's `defaults` and `vars` stay visible to the rest of the play, and its parameters do not.
#[test]
fn role_variables_stay_visible_after_the_role() {
    // Through the deadline, not bare: this play carries an `include_role`, so it reaches a
    // splice point and can block there. An assertion on elapsed time cannot fail on a hang.
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("inventory.ini"),
            &fixture("roles/roles.yml"),
        ],
        PROBE_DEADLINE,
    );
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
#[test]
fn a_role_entry_vars_loses_to_a_fact_and_a_free_key_beats_it() {
    // Through the deadline, not bare: this play carries an `include_role`, so it reaches a
    // splice point and can block there. An assertion on elapsed time cannot fail on a hang.
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("inventory.ini"),
            &fixture("roles/roles.yml"),
        ],
        PROBE_DEADLINE,
    );
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

/// `import_role` and `include_role` both run a role the play's `roles:` list has already run with
/// the same parameters.
#[test]
fn an_import_role_runs_again_what_the_roles_list_already_ran() {
    // Through the deadline, not bare: this play carries an `include_role`, so it reaches a
    // splice point and can block there. An assertion on elapsed time cannot fail on a hang.
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("inventory.ini"),
            &fixture("roles/roles.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert_eq!(
        text.matches("TASK [base : base task]").count(),
        6,
        "four entries, the import and the include that each repeat one of them: {text}"
    );
}

/// A ring of roles importing each other, and a file importing itself, are refused before the
/// recursion runs out of stack.
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

/// An argument an import statement does not take is refused by its own name, rather than read past
/// and dropped.
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

/// A `rescue` around an `include_tasks` takes a failure raised by a task the include brought in.
#[test]
fn a_rescue_takes_a_failure_raised_inside_an_include() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("include/inv.ini"),
            &fixture("include/in-block.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains("\"msg\": \"rescued included task that fails\""),
        "{text}"
    );
    assert!(text.contains("\"msg\": \"always\""), "{text}");
    assert!(text.contains("\"msg\": \"past\""), "{text}");
    assert!(
        text.contains("h1                         : ok=5    changed=0    unreachable=0    failed=0    skipped=0    rescued=1"),
        "{text}"
    );
}

/// An include in a `rescue` and an include in an `always`, both reached by a host on its way out of
/// a failed block, plus a `loop` over an empty list.
#[test]
fn an_include_in_a_rescue_and_in_an_always_are_both_reached() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("include/inv.ini"),
            &fixture("include/cleanup.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert_eq!(text.matches("skipping: [h").count(), 2, "{text}");
    let dir = fixture("include");
    assert!(
        text.contains(&format!("included: {dir}/inc-a.yml for h1, h2")),
        "{text}"
    );
    assert!(
        text.contains(&format!("included: {dir}/inc-b.yml for h1, h2")),
        "{text}"
    );
    assert!(text.contains("\"msg\": \"past\""), "{text}");
    for host in ["h1", "h2"] {
        assert!(
            text.contains(&format!(
                "{host}                         : ok=5    changed=0    unreachable=0    failed=0    skipped=1    rescued=1"
            )),
            "{text}"
        );
    }
}

/// A host draining the `always` of a block nobody rescues reaches an include written in that
/// section, and the rest of the section still runs behind what the include brought in.
#[test]
fn a_draining_host_runs_the_rest_of_an_always_behind_an_include() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("include/inv.ini"),
            &fixture("include/draining.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2), "{text}");
    let dir = fixture("include");
    assert!(
        text.contains(&format!("included: {dir}/inc-b.yml for h1, h2")),
        "{text}"
    );
    assert!(text.contains("\"msg\": \"second\""), "{text}");
    assert!(
        !text.contains("\"msg\": \"past\""),
        "the step behind the block is not run for a host on its way out: {text}"
    );
    for host in ["h1", "h2"] {
        assert!(
            text.contains(&format!(
                "{host}                         : ok=3    changed=0    unreachable=0    failed=1    skipped=0    rescued=0"
            )),
            "{text}"
        );
    }
}

/// A rescue inside an `always` a host is draining takes its own failure, an include between the
/// two, and the host drains on behind it.
#[test]
fn a_rescue_inside_a_cleanup_leaves_the_rest_of_it_to_run() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("include/inv.ini"),
            &fixture("include/cleanup-rescue.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert_eq!(text.matches("\"msg\": \"rest\"").count(), 2, "{text}");
    assert!(
        !text.contains("\"msg\": \"past\""),
        "both hosts are on their way out of the play: {text}"
    );
    assert!(
        text.contains("h1                         : ok=2    changed=0    unreachable=0    failed=1    skipped=0    rescued=1"),
        "{text}"
    );
    assert!(
        text.contains("h2                         : ok=4    changed=0    unreachable=0    failed=1    skipped=1    rescued=0"),
        "{text}"
    );
}

/// A file that includes itself is bounded rather than followed.
#[test]
fn an_include_that_includes_itself_is_bounded() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("include/inv.ini"),
            &fixture("include/self-include.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(
        text.contains("includes nest deeper than 32 levels"),
        "{text}"
    );
    assert!(
        !text.contains("\"msg\": \"never\""),
        "the host left at the ceiling: {text}"
    );
}

/// An argument an include statement does not take, or takes and this release cannot honour, is
/// refused by its own name before the first connection.
#[test]
fn an_include_statement_refuses_an_argument_it_cannot_honour() {
    for (file, message) in [
        ("include/refused.yml", "'include_role' option 'public'"),
        ("include/vars-dir.yml", "'include_vars' option 'dir'"),
    ] {
        let out = volant(&[
            "playbook",
            "-i",
            &fixture("include/inv.ini"),
            &fixture(file),
        ]);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.status.code(), Some(4), "{text}");
        assert!(text.contains(message), "{text}");
        assert!(!text.contains("PLAY ["), "nothing runs before it: {text}");
    }
}

/// A keyword this release refuses, written inside a file a dynamic `include_tasks` pulls in, stops
/// the statement instead of being read and dropped.
#[test]
fn a_keyword_this_release_refuses_is_refused_inside_an_included_file() {
    for (kind, keyword, sentence) in [
        (
            "check-mode",
            "check_mode: true",
            "keyword 'check_mode' is not supported yet with 'true'",
        ),
        (
            "throttle",
            "throttle: 1",
            "keyword 'throttle' is not supported yet",
        ),
    ] {
        let dir = probe_dir(&format!("spliced-{kind}"));
        let marker = dir.join("marker");
        std::fs::write(dir.join("inv.ini"), "h1 ansible_connection=local\n").expect("an inventory");
        std::fs::write(
            dir.join("play.yml"),
            "- hosts: h1\n  gather_facts: false\n  tasks:\n    - name: Bring in a file\n      include_tasks: inc.yml\n",
        )
        .expect("the play");
        std::fs::write(
            dir.join("inc.yml"),
            format!(
                "- name: Runs for real\n  command: touch {}\n  {keyword}\n",
                marker.display()
            ),
        )
        .expect("the included file");
        let out = volant_within(
            &[
                "playbook",
                "-i",
                dir.join("inv.ini").to_str().expect("a path"),
                dir.join("play.yml").to_str().expect("a path"),
            ],
            PROBE_DEADLINE,
        );
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!marker.exists(), "{kind}: the task ran anyway:\n{text}");
        assert_eq!(out.status.code(), Some(2), "{kind}: {text}");
        assert!(
            text.contains(&format!("task 'Runs for real': {sentence}")),
            "{kind}: {text}"
        );
        assert!(
            !text.contains("TASK [Runs for real]"),
            "{kind}: nothing behind the statement ran: {text}"
        );
        assert!(
            text.contains("failed=1"),
            "{kind}: the statement failed for the host that asked: {text}"
        );
    }
}

/// `include_tasks` takes `file`, `_raw_params` and `apply`, and `apply` itself is refused: this
/// release has no layer to put the keywords it carries on.
#[test]
fn include_tasks_refuses_apply_and_an_unknown_key() {
    let dir = probe_dir("include-tasks-options");
    std::fs::write(dir.join("sub.yml"), "- name: T\n  command: echo hi\n")
        .expect("the probe include target is written");
    for (body, message) in [
        (
            "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - include_tasks:\n        file: sub.yml\n        apply:\n          become: true\n",
            "'include_tasks' option 'apply'",
        ),
        (
            "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - include_tasks:\n        file: sub.yml\n        nosuchkey: true\n",
            "Invalid options for include_tasks: nosuchkey",
        ),
    ] {
        std::fs::write(dir.join("site.yml"), body).expect("the probe playbook is written");
        let path = dir.join("site.yml");
        let out = volant(&["playbook", &path.display().to_string()]);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.status.code(), Some(4), "{text}");
        assert!(text.contains(message), "{text}");
        assert!(!text.contains("PLAY ["), "nothing runs before it: {text}");
    }
    std::fs::remove_dir_all(&dir).expect("the probe directory is removed");
}

/// An `import_tasks` naming a file that is not there stops the run at exit 1, in the reference's
/// own words, before anything has run.
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

/// `import_playbook` is a play-level statement; written as a task, the reference cannot run it
/// either, and dies with its own traceback rather than a message naming what was wrong.
#[test]
fn import_playbook_written_as_a_task_is_refused_by_its_own_name() {
    let dir = probe_dir("import-playbook-as-task");
    std::fs::write(
        dir.join("site.yml"),
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - import_playbook: sub.yml\n",
    )
    .expect("the probe playbook is written");
    let path = dir.join("site.yml");
    let out = volant(&["playbook", &path.display().to_string()]);
    let err = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(
        err.contains(
            "Task failed: Action 'ansible.builtin.import_playbook' does not support raw params."
        ),
        "{err}"
    );
    std::fs::remove_dir_all(&dir).expect("the probe directory is removed");
}

/// The lines one run printed, without the blank ones, so a test can say what came in what order.
fn lines(out: &Output) -> Vec<String> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// The same rule without `serial`: a play whose every host fails is a batch that lost every host,
/// so the play behind it never appears.
#[test]
fn every_host_failing_ends_the_run_before_the_next_play() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("serial/inv.ini"),
            &fixture("serial/allfail.yml"),
        ],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert_eq!(text.matches("PLAY [").count(), 1, "{text}");
    assert!(!text.contains("second"), "the next play never runs: {text}");
    assert!(text.contains("PLAY RECAP"), "{text}");
}

/// A play whose resolved hosts have all failed already is not a batch that lost them: its banner
/// prints alone and the run carries on.
#[test]
fn a_play_whose_hosts_have_all_failed_shows_its_banner_alone() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("serial/inv.ini"),
            &fixture("serial/onefail.yml"),
        ],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(2), "{text}");
    let order: Vec<String> = lines(&out)
        .into_iter()
        .filter(|l| l.starts_with("PLAY [") || l.starts_with("TASK ["))
        .map(|l| l.split('*').next().unwrap_or_default().trim().to_string())
        .collect();
    assert_eq!(
        order,
        [
            "PLAY [h1,h2]",
            "TASK [command]",
            "PLAY [h1]",
            "PLAY [h2]",
            "TASK [debug]",
        ],
        "the banner of the emptied play stands alone and the play behind it runs: {text}"
    );
    assert!(
        !text.contains("skipping: no hosts matched"),
        "a play whose hosts failed is not a pattern that matched nothing: {text}"
    );
    assert!(
        !text.contains("second-h1-only") && text.contains(r#""msg": "third""#),
        "{text}"
    );
}

/// A batch whose hosts had all failed before it started runs nothing, says nothing beyond its
/// banner, and does **not** end the run.
#[test]
fn a_batch_of_already_failed_hosts_does_not_end_the_run() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("serial/inv.ini"),
            &fixture("serial/already_failed.yml"),
        ],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert_eq!(
        text.matches("PLAY [all]").count(),
        5,
        "one banner for the first play, three for the batches, one for the third: {text}"
    );
    assert!(
        text.contains("ok: [h2] => {\n    \"msg\": \"h2 / h2,h3 / h1,h2,h3\"\n}")
            && text.contains("ok: [h3] => {\n    \"msg\": \"h3 / h2,h3 / h1,h2,h3\"\n}"),
        "the batches of the surviving hosts run, one host each: {text}"
    );
    assert!(
        !text.contains(
            "[h1] => {
    \"msg\""
        ),
        "the failed host runs nothing: {text}"
    );
    assert!(
        text.contains(r#""msg": "third""#),
        "the play behind the emptied batch runs: {text}"
    );
}

/// `vars_files` is read once per batch of a `serial` play, so a path templated on
/// `ansible_play_batch` reads a different file for each batch.
#[test]
fn vars_files_is_read_per_batch_not_per_play() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("serial/inv.ini"),
            &fixture("serial/vars_files.yml"),
        ],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(
        text.contains("ok: [h2] => {\n    \"msg\": \"batch=h2 marker=from-h2\"\n}"),
        "h2's own batch reads h2's own vars_files: {text}"
    );
    assert!(
        text.contains("ok: [h3] => {\n    \"msg\": \"batch=h3 marker=from-h3\"\n}"),
        "h3's own batch reads h3's own vars_files, not h2's: {text}"
    );
}

/// A `run_once` task that fails takes every other host of the batch out of the play.
#[test]
fn a_failing_run_once_task_ends_the_play_for_the_other_hosts() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("delegate/inv.ini"),
            &fixture("delegate/run-once-fails.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("fatal: [h1]: FAILED!"), "{text}");
    assert!(
        !text.contains("after on"),
        "nothing runs behind a run_once that failed: {text}"
    );
    assert!(
        !text.contains("h2                         :"),
        "a host that ran nothing has no recap line: {text}"
    );

    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("delegate/inv.ini"),
            &fixture("delegate/run-once-rescued.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(
        out.status.code(),
        Some(2),
        "the rescued host is not the whole play: {text}"
    );
    assert!(text.contains(r#""msg": "rescued on h1""#), "{text}");
    assert!(text.contains(r#""msg": "after on h1""#), "{text}");
    assert!(
        !text.contains("on h2"),
        "the other host leaves rather than entering a rescue it never failed into: {text}"
    );
    assert!(
        text.contains("h1                         : ok=2    changed=0    unreachable=0    failed=0    skipped=0    rescued=1"),
        "{text}"
    );
    assert!(!text.contains("h2                         :"), "{text}");
}

/// A `run_once` runner nothing can reach ends the play for the hosts waiting on it, and the exit
/// code stays the unreachable one.
#[test]
fn an_unreachable_run_once_runner_releases_the_hosts_waiting_on_it() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("delegate/run-once-unreachable.ini"),
            &fixture("delegate/run-once-unreachable.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(4), "{text}");
    assert!(text.contains("fatal: [h1]: UNREACHABLE!"), "{text}");
    assert!(
        !text.contains("on h2"),
        "the waiting host leaves rather than running the rest: {text}"
    );
    assert!(!text.contains("h2                         :"), "{text}");
}

/// A delegate nothing can reach takes the **delegating** host out of the run, and the line carries
/// the arrow.
#[test]
fn an_unreachable_delegate_is_the_delegating_hosts_failure() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("delegate/unreachable.ini"),
            &fixture("delegate/delegate-unreachable.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(4), "{text}");
    assert!(text.contains("fatal: [h1 -> gone]: UNREACHABLE!"), "{text}");
    assert!(text.contains("Connection refused"), "{text}");
    assert!(
        text.contains("h1                         : ok=0    changed=0    unreachable=1"),
        "{text}"
    );
    assert!(!text.contains("gone                 "), "{text}");
    assert!(!text.contains(r#""msg": "after""#), "{text}");
}

/// A host that has failed is still a valid delegate.
#[test]
fn a_failed_host_is_still_a_valid_delegate() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("delegate/inv.ini"),
            &fixture("delegate/delegate-failed-host.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("fatal: [h3]: FAILED!"), "{text}");
    assert!(text.contains("changed: [h1 -> h3]"), "{text}");
    assert!(text.contains(r#""msg": "hi""#), "{text}");
}

/// A directory of this test's own, with a payload file whose text is a Jinja expression that
/// creates a marker, and a local inventory. The marker is what the trust tests assert on: it is
/// the effect of the expression, not the text a snapshot would show.
fn trust_dir(name: &str, hosts: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("volant-trust-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temp dir");
    let marker = dir.join("marker");
    std::fs::write(
        dir.join("payload.txt"),
        format!("{{{{ lookup('pipe', 'touch {}') }}}}", marker.display()),
    )
    .expect("the payload");
    // The same expression without the markers around it. A string a `debug: var:` names is
    // compiled as an expression rather than rendered as a template, so the payload that reaches
    // that site carries no braces at all.
    std::fs::write(
        dir.join("bare.txt"),
        format!("lookup('pipe', 'touch {}')", marker.display()),
    )
    .expect("the bare payload");
    std::fs::write(dir.join("inv.ini"), hosts).expect("an inventory");
    (dir, marker)
}

/// The name a `debug: var:` shows is compiled as an expression, not rendered as a template, so a
/// value that arrived from a managed host must never reach it.
#[test]
fn a_variable_named_by_a_result_is_never_evaluated() {
    let (dir, marker) = trust_dir("debug-var", "h1 ansible_connection=local\n");
    let out = volant_within(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            "-e",
            &format!("payload={}", dir.join("bare.txt").display()),
            &fixture("trust/debug-var.yml"),
        ],
        std::time::Duration::from_secs(30),
    );
    assert!(
        !marker.exists(),
        "the expression a result named ran on the controller:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout
            .matches("Encountered untrusted template or expression")
            .count(),
        4,
        "both the plain form and the loop form should be refused, each in its error block and its line:\n{stdout}"
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A fact whose value came from a managed host still cannot name the variable a `debug: var:`
/// compiles.
#[test]
fn a_fact_carrying_a_result_cannot_name_a_variable() {
    let (dir, marker) = trust_dir("result-fact-name", "h1 ansible_connection=local\n");
    let out = volant_within(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            "-e",
            &format!("payload={}", dir.join("bare.txt").display()),
            &fixture("trust/result-fact-name.yml"),
        ],
        std::time::Duration::from_secs(30),
    );
    assert!(
        !marker.exists(),
        "the expression a carried fact named ran on the controller:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Encountered untrusted template or expression"),
        "the carried result should be refused:\n{stdout}"
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A directory holding one executable `ssh` that writes `marker` and exits 255, first on `PATH`,
/// beside an inventory file holding `inventory`. Nothing here reaches the network: the fake is
/// what a run opening an ssh connection finds, and the marker is what it leaves behind.
fn fake_ssh(name: &str, inventory: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("volant-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).expect("a bin dir");
    let marker = dir.join("ssh-was-called");
    let ssh = dir.join("bin/ssh");
    std::fs::write(
        &ssh,
        format!("#!/bin/sh\ntouch {}\nexit 255\n", marker.display()),
    )
    .expect("the fake ssh");
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).expect("make it run");
    std::fs::write(dir.join("inv.ini"), inventory).expect("an inventory");
    (dir, marker)
}

/// Like `fake_ssh`, but keeps every invocation's whole argv instead of a bare marker: task 8's
/// proof needs to inspect what actually reached the remote command line, not just that ssh ran.
fn fake_ssh_recording(name: &str, inventory: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("volant-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).expect("a bin dir");
    let log = dir.join("ssh.log");
    let ssh = dir.join("bin/ssh");
    std::fs::write(
        &ssh,
        format!("#!/bin/sh\necho \"$@\" >> {}\nexit 255\n", log.display()),
    )
    .expect("the fake ssh");
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).expect("make it run");
    std::fs::write(dir.join("inv.ini"), inventory).expect("an inventory");
    (dir, log)
}

/// A `remote_tmp` substitution never reaches the command line the fake `ssh` records.
#[test]
fn a_remote_tmp_substitution_never_reaches_the_recorded_ssh_command_line() {
    let (dir, log) = fake_ssh_recording(
        "remote-tmp-substitution",
        "node ansible_connection=ssh ansible_host=192.0.2.1 ansible_remote_tmp=~$(id)/x\n",
    );
    let out = volant_within_with_path(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            &fixture("connection/local-override.yml"),
        ],
        std::time::Duration::from_secs(30),
        Some(&dir.join("bin")),
        &[],
    );
    let recorded = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !recorded.contains("$("),
        "the substitution reached the ssh command line: {recorded}\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.status.code(),
        Some(4),
        "a host refused for its own remote_tmp is unreachable, not failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // The refusal says which host and which value, because a run of two hundred hosts that only
    // says a `remote_tmp` was refused sends its operator looking through the inventory by hand.
    // Both streams, because this one travels as an `UNREACHABLE` line on stdout while a refusal
    // read from the configuration goes to stderr - reading one of them would pass while the text
    // was on the other.
    let shown = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(shown.contains("node"), "the refusal names no host: {shown}");
    assert!(
        shown.contains("~$(id)/x"),
        "the refusal names no value: {shown}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The other half of the guard, and the one the exit-code test above cannot see: a user name still
/// reaches the wire **bare**, because the remote shell is the only thing that can expand it.
#[test]
fn an_ordinary_tilde_user_reaches_the_ssh_command_line_bare() {
    let (dir, log) = fake_ssh_recording(
        "remote-tmp-bare-tilde",
        "node ansible_connection=ssh ansible_host=192.0.2.1 ansible_remote_tmp=~root/x\n",
    );
    let out = volant_within_with_path(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            &fixture("connection/local-override.yml"),
        ],
        std::time::Duration::from_secs(30),
        Some(&dir.join("bin")),
        &[],
    );
    let recorded = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        recorded.contains("~root/"),
        "the tilde did not reach the command line unquoted: {recorded}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !recorded.contains("'~root"),
        "the tilde was quoted, so the remote shell cannot expand it: {recorded}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Whether a run calls `ssh`, through a fake one that leaves a marker: the connection an extra
/// var, a task's vars or a host or group file asks for, with and without batching.
#[rstest]
#[case::an_extra_var_switches_to_local(
    "node ansible_connection=ssh ansible_host=192.0.2.1\n",
    &["-e", "ansible_connection=local"],
    "connection/local-override.yml",
    &[],
    false,
    Some(0),
    None
)]
#[case::a_task_that_changes_the_connection_opens_its_own_link(
    "node ansible_connection=local\n",
    &[],
    "connection/task-vars-switch.yml",
    &[],
    true,
    None,
    Some("fatal: [node]: UNREACHABLE!")
)]
#[case::a_batch_splits_where_the_connection_changes(
    "node ansible_connection=local\n",
    &[],
    "connection/task-vars-switch.yml",
    &[("VOLANT_BATCHING", "1")],
    true,
    None,
    Some("fatal: [node]: UNREACHABLE!")
)]
#[case::a_group_file_does_not_move_the_implicit_localhost(
    "node ansible_connection=local\n",
    &[],
    "connection/implicit-local/play.yml",
    &[],
    false,
    Some(0),
    None
)]
#[case::a_host_file_does_move_the_implicit_localhost(
    "node ansible_connection=local\n",
    &[],
    "connection/host-file-wins/play.yml",
    &[],
    true,
    None,
    None
)]
fn the_connection_a_run_asks_for_decides_whether_ssh_runs(
    #[case] inventory: &str,
    #[case] extra: &[&str],
    #[case] playbook: &str,
    #[case] envs: &[(&str, &str)],
    #[case] ssh_runs: bool,
    #[case] code: Option<i32>,
    #[case] shows: Option<&str>,
) {
    let name = format!(
        "conn-{}-{}-{}",
        playbook.replace('/', "-"),
        extra.len(),
        envs.len()
    );
    let (dir, marker) = fake_ssh(&name, inventory);
    let inv = dir.join("inv.ini");
    let mut args = vec!["playbook", "-i", inv.to_str().expect("a path")];
    args.extend_from_slice(extra);
    let book = fixture(playbook);
    args.push(&book);
    let out = volant_within_with_path(
        &args,
        std::time::Duration::from_secs(30),
        Some(&dir.join("bin")),
        envs,
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        marker.exists(),
        ssh_runs,
        "ssh ran: {}\n{stdout}\n{stderr}",
        marker.exists()
    );
    if let Some(code) = code {
        assert_eq!(out.status.code(), Some(code), "{stdout}\n{stderr}");
    }
    if let Some(line) = shows {
        assert!(stdout.contains(line), "{stdout}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `no_log` covers every event a task produces, not only its result.
#[test]
fn no_log_covers_the_environment_warning() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("inventory.ini"),
            &fixture("nolog/environment.yml"),
        ],
        std::time::Duration::from_secs(30),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains("CANARY-7f3a9c") && !stderr.contains("CANARY-7f3a9c"),
        "the secret reached the terminal:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[WARNING]: could not parse environment value, skipping: ['{{ secret }}']"),
        "the warning did not report the source the reference reports:\nstderr:\n{stderr}"
    );
    assert_eq!(out.status.code(), Some(0));
}

/// A timeout covers the task, not the first process of it.
#[test]
fn a_timeout_covers_a_descendant_holding_the_pipes() {
    // A `sleep` duration that doubles as the `pgrep -f` marker, as in `local_transport.rs`.
    let marker = format!("5.{}", std::process::id());
    let started = std::time::Instant::now();
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("inventory.ini"),
            &fixture("timeout/descendant.yml"),
            "-e",
            &format!("marker={marker}"),
        ],
        std::time::Duration::from_secs(30),
    );
    let elapsed = started.elapsed();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(2), "{stdout}");
    assert!(
        stdout.contains(r#""msg": "Task failed: Timed out after 1 second(s).""#),
        "the task did not report the reference's timeout text:\n{stdout}"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(3000),
        "the run took {elapsed:?}: the deadline did not reach the readers"
    );
    assert_no_survivor(&marker);
}

/// Fails naming any process whose command line still carries `marker` a moment after the run:
/// the descendant of a timed-out task has to die with its process group.
fn assert_no_survivor(marker: &str) {
    std::thread::sleep(std::time::Duration::from_millis(200));
    let survivors = Command::new("pgrep")
        .args(["-f", marker])
        .output()
        .expect("pgrep runs");
    assert!(
        survivors.stdout.is_empty(),
        "the timed-out task's descendant survived: {}",
        String::from_utf8_lossy(&survivors.stdout)
    );
}

/// The same deadline over the other pipe.
#[test]
fn a_timeout_covers_a_descendant_holding_stdin() {
    // An argument the descendant ignores, carried on its command line for `pgrep -f`.
    let marker = format!("volant-stdin-descendant-{}", std::process::id());
    let started = std::time::Instant::now();
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("inventory.ini"),
            &fixture("timeout/stdin-descendant.yml"),
            "-e",
            &format!("marker={marker}"),
        ],
        std::time::Duration::from_secs(30),
    );
    let elapsed = started.elapsed();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(2), "{stdout}");
    assert!(
        stdout.contains(r#""msg": "Task failed: Timed out after 1 second(s).""#),
        "the task did not report the reference's timeout text:\n{stdout}"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(3000),
        "the run took {elapsed:?}: the deadline did not reach the write to stdin"
    );
    assert_no_survivor(&marker);
}

/// A module argument the reference has and this release does not act on is refused by its own name,
/// before anything connects.
#[test]
fn an_argument_this_release_does_not_honour_is_refused_before_the_play() {
    let out = volant(&["playbook", &fixture("module-argument-refused.yml")]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "{stderr}{stdout}");
    assert!(
        stderr.contains(
            "argument 'expand_argument_vars' is not supported yet with 'true' on 'command'"
        ),
        "the refusal did not name the argument:\n{stderr}"
    );
    assert!(
        !stdout.contains("PLAY ["),
        "a host was reached before the refusal:\n{stdout}"
    );
}

/// The same refusal, for a value the pre-flight could not read.
#[test]
fn a_refused_argument_the_pre_flight_could_not_read_fails_on_the_host() {
    let out = volant(&["playbook", &fixture("module-argument-refused-late.yml")]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(2), "{stdout}");
    assert!(
        stdout.contains(
            "argument 'expand_argument_vars' is not supported yet with 'true' on 'command'. The host renders this value, so the check before the run could not read it."
        ),
        "the task did not refuse the value it was handed:\n{stdout}"
    );
    assert!(
        !stdout.contains("Never reached"),
        "the play carried on past the refusal:\n{stdout}"
    );
}

/// The default of that same argument is where the two engines part, and no playbook writes it.
#[test]
fn nothing_expands_a_module_argument_unless_a_shell_does_it() {
    let out = volant_within_env(
        &[
            "playbook",
            &fixture("module-argument-expansion-default.yml"),
        ],
        DEFAULT_DEADLINE,
        &[("VOLANT_EXPANSION_PROBE", "expanded")],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}{stdout}");
    assert!(
        stdout.contains(r#""msg": "command=$VOLANT_EXPANSION_PROBE shell=expanded""#),
        "the default did something other than what the module page publishes:\n{stdout}"
    );
}

/// An argument neither engine has fails the module at run time, with the reference's sentence and
/// exit code.
#[test]
fn an_argument_neither_engine_has_fails_with_the_reference_s_words() {
    let out = volant(&["playbook", &fixture("module-argument-unknown.yml")]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(2), "{stdout}");
    assert!(
        stdout.contains(
            "Unsupported parameters for (ansible.legacy.command) module: no_such_arg. Supported parameters include: _raw_params, _uses_shell, argv, chdir, cmd, creates, executable, expand_argument_vars, removes, stdin, stdin_add_newline, strip_empty_ends."
        ),
        "the task did not report the reference's sentence:\n{stdout}"
    );
    assert!(
        stdout.contains("PLAY ["),
        "the reference reaches the host and fails the task there:\n{stdout}"
    );
}

/// A fake controller-side Python, so the wiring below is proved without an ansible-core on the
/// machine running the tests -- and without the 631 KB the real helper would build.
///
/// It answers the two requests the controller makes, the playbook's plugin directories and then
/// the build, and holds its end of the pipe open: the controller writes each request before it
/// reads the answer, and a helper that had already exited would break that write instead of
/// answering it. Every start appends a line to `starts.log`,
/// which is what counts them.
fn fake_python(dir: &Path) -> std::path::PathBuf {
    fake_python_answering(
        dir,
        &[br#"{"zip_b64":"UEsDBA==","modules":{"lineinfile":{"module_fqn":"ansible.modules.lineinfile","profile":"legacy","rlimit_nofile":1024,"extensions":{}}}}"#],
    )
}

/// [`fake_python`], answering each request of the run in turn with the next of `answers`.
fn fake_python_answering(dir: &Path, answers: &[&[u8]]) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    // The helper's first answer is to the plugin directories the controller hands it before
    // anything else, which it takes with an empty frame.
    let mut frame = Vec::new();
    for answer in std::iter::once(&b"{}"[..]).chain(answers.iter().copied()) {
        frame.extend_from_slice(&u32::try_from(answer.len()).unwrap().to_be_bytes());
        frame.extend_from_slice(answer);
    }
    std::fs::write(dir.join("answer.bin"), &frame).unwrap();
    let script = dir.join("python");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             if [ \"$2\" = 'import ansible' ]; then exit 0; fi\n\
             echo start >> '{dir}/starts.log'\n\
             cat '{dir}/answer.bin'\n\
             cat > /dev/null\n",
            dir = dir.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// Every blob the agent cached under `remote_tmp`, whatever uid the cache directory carries.
///
/// A cache entry is named by its hash alone since protocol 5, so the name is what is matched: 64
/// lowercase hex characters. A staged copy lives in the connection's own `stage-<host>-<pid>/`
/// directory, which this one-level scan never enters and which is gone once the connection ends;
/// the scan also walks the agent's own `volant-agent-<version>/` directory.
fn cached_blobs(remote_tmp: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(remote_tmp) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for dir in entries.flatten() {
        let Ok(files) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name();
            let name = name.to_string_lossy();
            if name.len() == 64 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                out.push(file.path());
            }
        }
    }
    out
}

/// A playbook naming a Python module reaches its hosts carrying the payload this run built for it,
/// and the helper that built it starts **once for the whole run**.
#[test]
fn a_run_builds_one_payload_and_sends_it_to_every_host() {
    let dir = std::env::temp_dir().join(format!("volant-pypayload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let remote_tmp = dir.join("tmp");
    std::fs::create_dir_all(&remote_tmp).unwrap();
    let python = fake_python(&dir);
    std::fs::write(
        dir.join("hosts.ini"),
        format!(
            "[web]\nalpha ansible_connection=local ansible_remote_tmp={tmp}\nbeta ansible_connection=local ansible_remote_tmp={tmp}\n",
            tmp = remote_tmp.display()
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("py.yml"),
        "- hosts: web\n  gather_facts: false\n  tasks:\n    - lineinfile: path=/tmp/volant-a line=a\n    - lineinfile: path=/tmp/volant-b line=b\n",
    )
    .unwrap();
    let out = volant_within_env(
        &[
            "playbook",
            "-i",
            &dir.join("hosts.ini").display().to_string(),
            &dir.join("py.yml").display().to_string(),
        ],
        std::time::Duration::from_secs(60),
        &[
            ("VOLANT_PYTHON", &python.display().to_string() as &str),
            // The local agent inherits this run's environment, and `VOLANT_REMOTE_TMP` is where
            // it caches a payload: the blob lands under this directory instead of the shared one.
            ("VOLANT_REMOTE_TMP", &remote_tmp.display().to_string()),
        ],
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains("PLAY ["),
        "the pre-flight refused a module that has a payload path:\n{text}"
    );
    assert!(
        !text.contains("built none for it"),
        "the run reached its hosts with no payload for a module it built one for:\n{text}"
    );
    let starts = std::fs::read_to_string(dir.join("starts.log")).unwrap_or_default();
    assert_eq!(
        starts.lines().count(),
        1,
        "one helper for the run, not one per host, batch or task:\n{starts}"
    );
    let blobs = cached_blobs(&remote_tmp);
    assert_eq!(blobs.len(), 1, "one payload cached, got {blobs:?}\n{text}");
    assert_eq!(
        std::fs::read(&blobs[0]).unwrap(),
        b"PK\x03\x04",
        "the bytes the helper handed over are the bytes the host holds"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A role's `vars/main.yml` holding nothing but a comment is a convention some published Galaxy
/// roles use to keep the file present with nothing to override: `geerlingguy.git` 3.0.1 ships one
/// exactly this way. Measured against the reference running that role: PyYAML resolves the empty
/// document to `None`, ansible-core's loader turns that into `{}`, and the play carries on.
///
/// Volant used to refuse the whole play before any host was contacted. The two engines part ways
/// earlier than the loader that refusal sat in: saphyr (pinned at 0.1.0) resolves the same
/// document to one node holding an empty *string* scalar, not null — confirmed with a standalone
/// probe of `MarkedYaml::load_from_str("---\n# comment only\n")`, which returns a single document
/// `Value(String(""))`. `crate::yaml::load` now reads a stream where every line is blank, a
/// comment or a document marker (`holds_nothing`, crates/volant/src/yaml.rs) as `Scalar::Null`
/// before the PyYAML-habits lowering pass runs, so `load_vars_file` sees the same `Value::Null`
/// document it already turns into an empty mapping.
///
/// What would make this red: `holds_nothing` no longer recognising a comment-only stream, or
/// `load_vars_file` (crates/volant/src/vars.rs) refusing a `Value::Null` document again.
#[test]
fn a_comment_only_vars_file_is_an_empty_mapping_not_a_refusal() {
    let dir = std::env::temp_dir().join(format!("volant-emptyvars-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("roles/blank/vars")).unwrap();
    std::fs::create_dir_all(dir.join("roles/blank/tasks")).unwrap();
    std::fs::write(
        dir.join("roles/blank/vars/main.yml"),
        "---\n# This space intentionally left blank.\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("roles/blank/tasks/main.yml"),
        "- debug:\n    msg: ok\n",
    )
    .unwrap();
    let path = dir.join("site.yml");
    std::fs::write(
        &path,
        "- hosts: localhost\n  gather_facts: false\n  roles:\n    - blank\n",
    )
    .unwrap();
    let out = volant(&["playbook", &path.display().to_string()]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(0), "stdout={text}\nstderr={err}");
    assert!(
        text.contains("ok: [localhost] => {\n    \"msg\": \"ok\"\n}"),
        "{text}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A role applied with `tags: [t]` and run with `--tags t` runs the tasks of a file its
/// `tasks/main.yml` includes (ansible-core 2.19.12).
#[test]
fn a_role_s_tags_reach_the_tasks_of_a_file_it_includes() {
    let dir = probe_dir("include-role-tags");
    let tasks = dir.join("roles/r/tasks");
    std::fs::create_dir_all(&tasks).expect("a role");
    std::fs::write(dir.join("inv.ini"), "h1 ansible_connection=local\n").expect("an inventory");
    std::fs::write(tasks.join("main.yml"), "- include_tasks: inc.yml\n").expect("main.yml");
    let marker = dir.join("marker");
    std::fs::write(
        tasks.join("inc.yml"),
        format!("- command: touch {}\n", marker.display()),
    )
    .expect("inc.yml");
    std::fs::write(
        dir.join("play.yml"),
        "- hosts: h1\n  gather_facts: false\n  roles:\n    - { role: r, tags: [t] }\n",
    )
    .expect("the play");
    let out = volant_within(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            dir.join("play.yml").to_str().expect("a path"),
            "--tags",
            "t",
        ],
        std::time::Duration::from_secs(30),
    );
    assert!(
        marker.exists(),
        "the included task never ran under --tags t:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Measured on ansible-core 2.19.12, `connection: local`: a play whose only task is
/// `include_tasks: inc.yml`, the file holding one `stat: path=/`, shows `included:` then
/// `ok: [h1]` for the `stat`. Volant builds its one payload union before the first connection,
/// from the modules the compiled plays name, and a file a dynamic include reads is not among
/// them: the `stat` fails with "module 'stat' needs a python payload, and this run built none
/// for it". Against a real host the same thing stopped `geerlingguy.security` at its first
/// `package` task, which sits in `fail2ban.yml` behind an `include_tasks`.
///
/// What would make this red: a Python module in a dynamically included file refused because the
/// union was built without it.
#[test]
fn a_python_module_in_a_dynamically_included_file_runs() {
    let Ok(python) = std::env::var("VOLANT_PYTHON") else {
        eprintln!(
            "skipped: VOLANT_PYTHON not set. Set it to a real ansible-core python to run this test."
        );
        return;
    };
    let dir = probe_dir("include-python-module");
    std::fs::write(dir.join("inv.ini"), "h1 ansible_connection=local\n").expect("an inventory");
    std::fs::write(dir.join("inc.yml"), "- stat: path=/\n").expect("inc.yml");
    std::fs::write(
        dir.join("play.yml"),
        "- hosts: h1\n  gather_facts: false\n  tasks:\n    - include_tasks: inc.yml\n",
    )
    .expect("the play");
    let out = volant_within_env(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            dir.join("play.yml").to_str().expect("a path"),
        ],
        std::time::Duration::from_secs(60),
        &[("VOLANT_PYTHON", &python)],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && !stdout.contains("needs a python payload"),
        "the included stat did not run:\n{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A module that answers `<name> says <word>`, as a `library/` holds one.
fn library_module(dir: &Path, name: &str, field: &str) {
    std::fs::create_dir_all(dir).expect("a library directory");
    std::fs::write(
        dir.join(format!("{name}.py")),
        format!(
            "from ansible.module_utils.basic import AnsibleModule\n\
             m = AnsibleModule(argument_spec={{'word': {{'type': 'str', 'default': 'x'}}}})\n\
             m.exit_json(changed=False, {field}='{name} says ' + m.params['word'])\n"
        ),
    )
    .expect("the module");
}

/// Measured on ansible-core 2.19.12, `connection: local`: a module in `library/` beside the
/// playbook and one in a role's `library/` both run, `ok` with the value each returns
/// (`add_all_plugin_dirs`, from `Playbook._load_playbook_data` and `Role._load_role_data`).
///
/// What would make this red: the playbook's or the role's directory not handed to the helper,
/// which refuses the task with "couldn't resolve module/action"; or a name no builtin has
/// refused before the controller's ansible-core was asked.
#[test]
fn a_module_in_the_playbook_s_library_or_a_role_s_runs() {
    let Some(python) = python::reference_python().map(|p| p.display().to_string()) else {
        return;
    };
    let dir = probe_dir("library-modules");
    library_module(&dir.join("library"), "pb_probe", "msg");
    let role = dir.join("roles").join("r");
    library_module(&role.join("library"), "role_probe", "msg");
    std::fs::create_dir_all(role.join("tasks")).expect("the role's tasks");
    std::fs::write(
        role.join("tasks").join("main.yml"),
        "- role_probe: word=from-role\n  register: r\n- debug: var=r.msg\n",
    )
    .expect("the role's tasks");
    std::fs::write(
        dir.join("play.yml"),
        "- hosts: localhost\n  gather_facts: false\n  roles: [r]\n  tasks:\n    - pb_probe: word=from-playbook\n      register: p\n    - debug: var=p.msg\n",
    )
    .expect("the play");
    let cache = dir.join("cache");
    let out = volant_within_env(
        &["playbook", dir.join("play.yml").to_str().expect("a path")],
        std::time::Duration::from_secs(60),
        &[
            ("VOLANT_PYTHON", &python),
            ("XDG_CACHE_HOME", cache.to_str().expect("a path")),
        ],
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains(r#""p.msg": "pb_probe says from-playbook""#),
        "{text}"
    );
    assert!(
        text.contains(r#""r.msg": "role_probe says from-role""#),
        "{text}"
    );
    // Measured there too: a word that is not `key=value` fails the task with "Action 'pb_probe'
    // does not support raw params." while the play runs, `ignore_errors` takes it, and the
    // play it fails exits 2.
    std::fs::write(
        dir.join("stray.yml"),
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - pb_probe: word=a stray\n      ignore_errors: true\n    - pb_probe: word=a stray\n",
    )
    .expect("the play");
    let out = volant_within_env(
        &["playbook", dir.join("stray.yml").to_str().expect("a path")],
        std::time::Duration::from_secs(60),
        &[
            ("VOLANT_PYTHON", &python),
            ("XDG_CACHE_HOME", cache.to_str().expect("a path")),
        ],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert_eq!(
        text.matches(r#"fatal: [localhost]: FAILED! => {"changed": false, "msg": "Task failed: Action 'pb_probe' does not support raw params."}"#)
            .count(),
        2,
        "{text}"
    );
    std::fs::remove_dir_all(&dir).expect("the probe directory is removed");
}

/// Sets `path`'s mtime `ago` seconds back, a directory's included. A kept union is only written
/// when every file and directory it depends on is older than two seconds (the racy-timestamp
/// rule), so a test that wants one kept ages what it has just written.
fn backdate(path: &Path, ago: u64) {
    std::fs::File::open(path)
        .and_then(|file| {
            file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(ago))
        })
        .unwrap_or_else(|e| panic!("backdating {}: {e}", path.display()));
}

/// Every file of the kept unions with its mtime: a run that hits the cache rewrites none.
fn kept_unions(cache: &Path) -> Vec<(std::path::PathBuf, std::time::SystemTime)> {
    let mut out: Vec<_> = std::fs::read_dir(cache.join("volant").join("unions"))
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// A `library/` written beside the playbook after a run kept its union: the next run builds
/// again and gets the playbook's `ping`, as ansible-core 2.19.12 does, where it finds the file
/// before its own module.
///
/// The run gets a `HOME` of its own, aged with everything else it wrote: the helper watches
/// `~/.ansible` (or `HOME` itself when that is absent), and on a shared machine another run
/// touching it within two seconds keeps the union out of the cache. A second run with nothing
/// changed has to hit the kept union, rewriting none of its files, or the third proves nothing.
///
/// What would make this red: the kept union's key blind to what the playbook's `library/`
/// holds. No source the first run kept names a directory that was not there, so the third run
/// loads the union holding ansible-core's `ping` and prints `pong`.
#[test]
fn a_module_written_in_the_playbook_s_library_rebuilds_a_kept_union() {
    let Some(python) = python::reference_python().map(|p| p.display().to_string()) else {
        return;
    };
    let dir = probe_dir("library-cache");
    std::fs::write(
        dir.join("play.yml"),
        "- hosts: localhost
  gather_facts: false
  tasks:
    - ping:
      register: p
    - debug: var=p.ping
",
    )
    .expect("the play");
    let home = dir.join("home");
    std::fs::create_dir_all(home.join(".ansible").join("tmp")).expect("a home");
    for path in [
        dir.join("play.yml"),
        home.join(".ansible").join("tmp"),
        home.join(".ansible"),
        home.clone(),
        dir.clone(),
    ] {
        backdate(&path, 60);
    }
    let cache = dir.join("cache");
    let run = || {
        let out = volant_within_env(
            &["playbook", dir.join("play.yml").to_str().expect("a path")],
            std::time::Duration::from_secs(60),
            &[
                ("VOLANT_PYTHON", &python),
                ("XDG_CACHE_HOME", cache.to_str().expect("a path")),
                ("HOME", home.to_str().expect("a path")),
            ],
        );
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.status.code(), Some(0), "{text}");
        text
    };
    let first = run();
    assert!(first.contains(r#""p.ping": "pong""#), "{first}");
    let kept = kept_unions(&cache);
    assert!(
        !kept.is_empty(),
        "the first run kept no union, so nothing is proven:
{first}"
    );
    let again = run();
    assert_eq!(
        kept_unions(&cache),
        kept,
        "a run with nothing changed rebuilt the union, so the next one proves nothing:
{again}"
    );
    let library = dir.join("library");
    library_module(&library, "ping", "ping");
    // Older than two seconds, so the key can vouch for it, and newer than anything the first
    // run saw, so its stamp is its own.
    backdate(&library.join("ping.py"), 10);
    backdate(&library, 10);
    let third = run();
    assert!(third.contains(r#""p.ping": "ping says x""#), "{third}");
    std::fs::remove_dir_all(&dir).expect("the probe directory is removed");
}

/// Measured on ansible-core 2.19.12 with a module calling `module.warn("probe warning")`: over
/// `loop: [1, 2]`, then once more in a later task, `[WARNING]: probe warning` is printed once for
/// the whole run (`Display._deduplicate`), while `w.warnings | length` is `2` and the later task
/// registers its own copy.
///
/// What would make this red: every copy printed, one line per item and per task; or the repeat
/// taken off the registered values along with the line.
#[test]
fn a_warning_repeated_by_two_items_prints_once() {
    let Some(python) = python::reference_python().map(|p| p.display().to_string()) else {
        return;
    };
    let dir = probe_dir("warning-once");
    let library = dir.join("library");
    std::fs::create_dir_all(&library).expect("a library directory");
    std::fs::write(
        library.join("warn_probe.py"),
        "from ansible.module_utils.basic import AnsibleModule\n\
         m = AnsibleModule(argument_spec={})\n\
         m.warn('probe warning')\n\
         m.exit_json(changed=False)\n",
    )
    .expect("the module");
    std::fs::write(
        dir.join("play.yml"),
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - warn_probe:\n      loop: [1, 2]\n      register: w\n    - debug: msg=\"{{ w.warnings | length }}\"\n    - warn_probe:\n      register: v\n    - debug: msg=\"{{ v.warnings | length }}\"\n",
    )
    .expect("the play");
    let cache = dir.join("cache");
    let out = volant_within_env(
        &["playbook", dir.join("play.yml").to_str().expect("a path")],
        std::time::Duration::from_secs(60),
        &[
            ("VOLANT_PYTHON", &python),
            ("XDG_CACHE_HOME", cache.to_str().expect("a path")),
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert_eq!(
        stderr.matches("[WARNING]: probe warning").count(),
        1,
        "{stderr}"
    );
    assert!(stdout.contains("\"msg\": 2\n}"), "{stdout}");
    assert!(stdout.contains("\"msg\": 1\n}"), "{stdout}");
    std::fs::remove_dir_all(&dir).expect("the probe directory is removed");
}

/// A `ping` module answering `tag` in `dir/ping.py`.
fn ping_module(dir: &Path, tag: &str) {
    std::fs::create_dir_all(dir).expect("a library directory");
    std::fs::write(
        dir.join("ping.py"),
        format!(
            "from ansible.module_utils.basic import AnsibleModule\n\
             m = AnsibleModule(argument_spec={{}})\n\
             m.exit_json(changed=False, ping='{tag}')\n"
        ),
    )
    .expect("the module");
}

/// A role `name` under `base/roles` whose task prints what `ping` answers, shipping its own
/// `ping` answering `tag` when there is one.
fn ping_role(base: &Path, name: &str, tag: Option<&str>) {
    let role = base.join("roles").join(name);
    std::fs::create_dir_all(role.join("tasks")).expect("the role's tasks");
    std::fs::write(
        role.join("tasks").join("main.yml"),
        format!("- ping:\n  register: q\n- debug: msg=\"{{{{ q.ping }}}} in role {name}\"\n"),
    )
    .expect("the role's tasks");
    if let Some(tag) = tag {
        ping_module(&role.join("library"), tag);
    }
}

/// A play printing what `ping` answers, as `label`, with `roles` in front of it.
fn ping_play(label: &str, roles: &str) -> String {
    format!(
        "- hosts: localhost\n  gather_facts: false\n{roles}  tasks:\n    - ping:\n      register: q\n    - debug: msg=\"{{{{ q.ping }}}} in {label}\"\n"
    )
}

/// Every `msg` a run printed, in order, and its exit code and error output.
fn ping_run(python: &str, cache: &Path, playbooks: &[&Path]) -> (Option<i32>, Vec<String>, String) {
    let mut args = vec!["playbook".to_string()];
    args.extend(playbooks.iter().map(|p| p.display().to_string()));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = volant_within_env(
        &args,
        std::time::Duration::from_secs(60),
        &[
            ("VOLANT_PYTHON", python),
            ("XDG_CACHE_HOME", cache.to_str().expect("a path")),
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let msgs = stdout
        .split(r#""msg": ""#)
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .map(str::to_string)
        .collect();
    (
        out.status.code(),
        msgs,
        format!("{stdout}{}", String::from_utf8_lossy(&out.stderr)),
    )
}

/// Measured on ansible-core 2.19.12, a `library/ping.py` in each position, the `msg` lines in
/// order:
/// - one playbook, a plain play then a play whose role ships `ping`: the role's `ping` answers
///   in the first play too (`r2lib in play1`, `r2lib in role r2`, `r2lib in play2`), every play's
///   roles being read before the first one runs;
/// - two named playbooks, the first's role shipping `ping`, the second's own directory shipping
///   one: the second directory wins everywhere (`blib in role ra`, `blib in A`, `blib in B`), as
///   every named playbook's directory is added first;
/// - `import_playbook` of a file whose directory ships `ping`, after a play whose role ships one:
///   the role wins everywhere (`ralib in role ra`, `ralib in main`, `ralib in sub`);
/// - two named playbooks, only the second's role shipping `ping`: `pong in A`, then the role's.
///   One union cannot hold both, and Volant refuses the run, naming the directory;
/// - `ping`, then an `include_role` of a role shipping it, then `ping`: `pong before`, then the
///   role's. Refused the same way.
///
/// What would make this red: the directories added in another order (every play's directory
/// before every role, which gives `sublib` in the third case), or a module a later playbook's
/// role ships run for an earlier playbook's task.
#[test]
fn library_directories_join_the_search_in_the_reference_s_order() {
    let Some(python) = python::reference_python().map(|p| p.display().to_string()) else {
        return;
    };
    let root = probe_dir("library-order");
    let cache = root.join("cache");

    let one = root.join("one");
    ping_role(&one, "r2", Some("r2lib"));
    std::fs::write(
        one.join("p.yml"),
        ping_play("play1", "") + &ping_play("play2", "  roles: [r2]\n"),
    )
    .expect("the play");
    let (code, msgs, text) = ping_run(&python, &cache, &[&one.join("p.yml")]);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(
        msgs,
        ["r2lib in play1", "r2lib in role r2", "r2lib in play2"],
        "{text}"
    );

    let (a, b) = (root.join("two").join("a"), root.join("two").join("b"));
    ping_role(&a, "ra", Some("ralib"));
    ping_module(&b.join("library"), "blib");
    std::fs::write(a.join("a.yml"), ping_play("A", "  roles: [ra]\n")).expect("the play");
    std::fs::write(b.join("b.yml"), ping_play("B", "")).expect("the play");
    let (code, msgs, text) = ping_run(&python, &cache, &[&a.join("a.yml"), &b.join("b.yml")]);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(
        msgs,
        ["blib in role ra", "blib in A", "blib in B"],
        "{text}"
    );

    let imp = root.join("import");
    ping_role(&imp, "ra", Some("ralib"));
    ping_module(&imp.join("sub").join("library"), "sublib");
    std::fs::write(
        imp.join("main.yml"),
        ping_play("main", "  roles: [ra]\n") + "- import_playbook: sub/b.yml\n",
    )
    .expect("the play");
    std::fs::write(imp.join("sub").join("b.yml"), ping_play("sub", "")).expect("the play");
    let (code, msgs, text) = ping_run(&python, &cache, &[&imp.join("main.yml")]);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(
        msgs,
        ["ralib in role ra", "ralib in main", "ralib in sub"],
        "{text}"
    );

    let (a, b) = (root.join("late").join("a"), root.join("late").join("b"));
    std::fs::create_dir_all(&a).expect("a directory");
    ping_role(&b, "rb", Some("rblib"));
    std::fs::write(a.join("a.yml"), ping_play("A", "")).expect("the play");
    std::fs::write(b.join("b.yml"), ping_play("B", "  roles: [rb]\n")).expect("the play");
    let (code, _, text) = ping_run(&python, &cache, &[&a.join("a.yml"), &b.join("b.yml")]);
    assert_eq!(code, Some(4), "{text}");
    assert!(
        text.contains("module 'ping' is found in ") && text.contains("rb/library only once"),
        "{text}"
    );

    let inc = root.join("include");
    ping_role(&inc, "ri", Some("rilib"));
    std::fs::write(
        inc.join("p.yml"),
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - ping:\n    - include_role: name=ri\n",
    )
    .expect("the play");
    let (code, _, text) = ping_run(&python, &cache, &[&inc.join("p.yml")]);
    assert_eq!(code, Some(4), "{text}");
    assert!(text.contains("ri/library only once"), "{text}");
    std::fs::remove_dir_all(&root).expect("the probe directory is removed");
}

/// Measured on ansible-core 2.19.12 with a `library/ping.py` beside the playbook answering
/// `lib`: `ping` and `ansible.legacy.ping` run it, `ansible.builtin.ping` runs ansible-core's and
/// answers `pong` (`_find_fq_plugin` searches only the package for `ansible.builtin`, while
/// `ansible.legacy` goes through the `library` search). The same run's gathering, which the
/// reference runs as `ansible.legacy.setup`, gets a `library/setup.py`'s facts.
///
/// What would make this red: `ansible.builtin.ping` filed under the short name, which hands it
/// the library's module; or the gathering named `ansible.builtin.setup`, which skips the
/// playbook's `setup`.
#[test]
fn a_library_module_answers_its_short_name_and_never_ansible_builtin() {
    let Some(python) = python::reference_python().map(|p| p.display().to_string()) else {
        return;
    };
    let dir = probe_dir("library-builtin");
    let library = dir.join("library");
    ping_module(&library, "lib");
    std::fs::write(
        library.join("setup.py"),
        "from ansible.module_utils.basic import AnsibleModule\n\
         m = AnsibleModule(argument_spec={}, supports_check_mode=True)\n\
         m.exit_json(changed=False, ansible_facts={'probe_from': 'lib'})\n",
    )
    .expect("the module");
    std::fs::write(
        dir.join("p.yml"),
        "- hosts: localhost\n  tasks:\n    - ping:\n      register: a\n    - ansible.builtin.ping:\n      register: b\n    - ansible.legacy.ping:\n      register: c\n    - debug: msg=\"{{ a.ping }} {{ b.ping }} {{ c.ping }} {{ probe_from | default('core') }}\"\n",
    )
    .expect("the play");
    let (code, msgs, text) = ping_run(&python, &dir.join("cache"), &[&dir.join("p.yml")]);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(msgs, ["lib pong lib lib"], "{text}");
    std::fs::remove_dir_all(&dir).expect("the probe directory is removed");
}

/// `retries` on a task an action plugin backs, through the real driver on the local connection.
/// Measured on ansible-core 2.19.12 with the same two tasks on `localhost`: a `copy` that
/// succeeds under `retries: 3` reports `attempts=1` and no retry line; a `copy` into a directory
/// that does not exist under `retries: 2` prints `FAILED - RETRYING ... (2 retries left)`, then
/// `(1 retries left)`, and fails with `"attempts": 2`.
///
/// What would make this red: the driver's plugin branch running each item once whatever the
/// retry plan says, which gives no retry line and no `attempts` at all; or a passing attempt
/// retried.
#[test]
fn a_task_an_action_plugin_backs_retries_through_the_driver() {
    let Some(python) = python::reference_python().map(|p| p.display().to_string()) else {
        return;
    };
    let dir = probe_dir("plugin-retries");
    let (code, text) = {
        let body = format!(
            "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: copy that succeeds\n      copy:\n        content: \"x\\n\"\n        dest: {dir}/ok.txt\n      retries: 3\n      register: ok\n    - debug: var=ok.attempts\n    - name: copy into a missing directory\n      copy:\n        content: \"x\\n\"\n        dest: {dir}/nosuch/x.txt\n      retries: 2\n      delay: 0\n      ignore_errors: true\n    - name: shell that appends\n      shell: \"echo run >> {dir}/runs.txt; exit 1\"\n      retries: 2\n      delay: 0\n      register: s\n      ignore_errors: true\n    - debug: var=s.attempts\n",
            dir = dir.display()
        );
        let file = dir.join("retries.yml");
        std::fs::write(&file, body).expect("the playbook is written");
        let out = volant_within_env(
            &["playbook", file.to_str().expect("a path")],
            std::time::Duration::from_secs(60),
            &[("VOLANT_PYTHON", &python)],
        );
        (
            out.status.code(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    };
    assert_eq!(code, Some(0), "{text}");
    assert!(text.contains(r#""ok.attempts": 1"#), "{text}");
    assert!(
        !text.contains("FAILED - RETRYING: [localhost]: copy that succeeds"),
        "{text}"
    );
    let failing = section(&text, "copy into a missing directory");
    for left in [2, 1] {
        assert!(
            failing.contains(&format!(
                "FAILED - RETRYING: [localhost]: copy into a missing directory ({left} retries left)."
            )),
            "{failing}"
        );
    }
    assert!(
        failing.contains(r#""attempts": 2"#) && failing.contains("...ignoring"),
        "{failing}"
    );
    // A module under the same rule runs once and then twice more: measured on ansible-core
    // 2.19.12 with this very task, three lines in the file and `"attempts": 2`.
    let runs = std::fs::read_to_string(dir.join("runs.txt")).expect("the shell ran");
    assert_eq!(runs.lines().count(), 3, "{runs}\n{text}");
    assert!(text.contains(r#""s.attempts": 2"#), "{text}");
    std::fs::remove_dir_all(&dir).expect("the probe directory is removed");
}

/// A task an action plugin backs runs over the link the host already has, and leaves it for
/// the next task: the agent that answers before a `copy` is the one that answers after it.
///
/// What would make this red: the driver not putting the link back once the plugin is done with
/// it, which connects a new agent for the next task, with a new process id.
#[test]
fn a_task_an_action_plugin_backs_leaves_the_link_for_the_next_task() {
    let Some(python) = python::reference_python().map(|p| p.display().to_string()) else {
        return;
    };
    let dir = probe_dir("plugin-link");
    let file = dir.join("link.yml");
    std::fs::write(
        &file,
        format!(
            "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - command: sh -c 'echo $PPID'\n      register: a\n    - copy:\n        content: \"x\\n\"\n        dest: {dir}/x.txt\n    - command: sh -c 'echo $PPID'\n      register: b\n    - assert:\n        that: a.stdout == b.stdout\n",
            dir = dir.display()
        ),
    )
    .expect("the playbook is written");
    let out = volant_within_env(
        &["playbook", file.to_str().expect("a path")],
        std::time::Duration::from_secs(60),
        &[("VOLANT_PYTHON", &python)],
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("All assertions passed"), "{text}");
    std::fs::remove_dir_all(&dir).expect("the probe directory is removed");
}

/// `ansible_run_tags` and `ansible_skip_tags` are the run's own tag options. Measured on
/// ansible-core 2.19.12: `run=['all'] skip=[]` with no option, `run=['kubeconfig']` under
/// `--tags kubeconfig`, `skip=['foo']` under `--skip-tags foo`.
///
/// What would make this red: the options never reaching the variables, which leaves every run
/// reading `['all']` and `[]` whatever it was asked for.
#[test]
fn the_tag_options_reach_ansible_run_tags_and_ansible_skip_tags() {
    let dir = probe_dir("run-tags");
    let body = "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - name: Show\n      debug:\n        msg: \"run={{ ansible_run_tags | join(',') }} skip={{ ansible_skip_tags | join(',') }}\"\n      tags: always\n";
    for (args, want) in [
        (&[][..], "run=all skip="),
        (&["--tags", "x"][..], "run=x skip="),
        (&["--skip-tags", "y"][..], "run=all skip=y"),
    ] {
        let (code, text) = run_probe(&dir, "run-tags", body, args, None);
        assert_eq!(code, 0, "{args:?}: {text}");
        assert!(
            text.contains(&format!("\"msg\": \"{want}\"")),
            "{args:?}: {text}"
        );
    }
    std::fs::remove_dir_all(&dir).expect("the probe directory is removed");
}

/// A word that is not `key=value` on a collection module, or on a `library/` module, fails that
/// task while the play runs, as the reference does, rather than stopping the run before it
/// starts. Measured on
/// ansible-core 2.19.12 with a collection beside the playbook: the `fatal:` line below,
/// `ignore_errors` and `rescue` both take the failure, a `when: false` still skips, and the play
/// ends at exit 2. The helper here is a fake: the refusal comes before anything is sent.
///
/// What would make this red: the pre-flight refusing the word, which exits 4 before `PLAY [`.
#[test]
fn a_raw_argument_on_a_collection_module_fails_only_that_task() {
    let dir = std::env::temp_dir().join(format!("volant-rawparams-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let python = fake_python_answering(
        &dir,
        &[
            br#"{"resolved":{"lib_probe":{"module":"lib_probe","collection":null},"ns.coll.m":{"module":"ns.coll.m","collection":["ns.coll","1.0.0"]}}}"#,
            br#"{"zip_b64":"UEsDBA==","modules":{"lib_probe":{"module_fqn":"ansible.legacy.lib_probe","profile":"legacy","rlimit_nofile":1024,"extensions":{}},"ns.coll.m":{"module_fqn":"ansible_collections.ns.coll.plugins.modules.m","profile":"legacy","rlimit_nofile":1024,"extensions":{}}}}"#,
        ],
    );
    std::fs::write(dir.join("inv.ini"), "probe ansible_connection=local\n").expect("an inventory");
    let play = dir.join("play.yml");
    std::fs::write(
        &play,
        "- hosts: all\n  gather_facts: false\n  tasks:\n\
         \x20   - ns.coll.m: v=1 stray\n\
         \x20     register: r1\n\
         \x20     ignore_errors: true\n\
         \x20   - debug: {var: r1.exception}\n\
         \x20   - block:\n\
         \x20       - ns.coll.m: v=1 stray\n\
         \x20     rescue:\n\
         \x20       - debug: {msg: \"rescued {{ ansible_failed_result.msg }}\"}\n\
         \x20   - ns.coll.m: v=1 stray\n\
         \x20     when: false\n\
         \x20   - lib_probe: v=1 stray\n\
         \x20     ignore_errors: true\n\
         \x20   - ns.coll.m: v=1 stray\n\
         \x20   - debug: {msg: never}\n",
    )
    .expect("a playbook");
    let out = volant_within_env(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            play.to_str().expect("a path"),
        ],
        DEFAULT_DEADLINE,
        &[("VOLANT_PYTHON", python.to_str().expect("a path"))],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{text}\n{err}");
    let line = r#"fatal: [probe]: FAILED! => {"changed": false, "msg": "Task failed: Action 'ns.coll.m' does not support raw params."}"#;
    assert_eq!(text.matches(line).count(), 3, "{text}");
    assert!(
        text.contains(r#"fatal: [probe]: FAILED! => {"changed": false, "msg": "Task failed: Action 'lib_probe' does not support raw params."}"#),
        "{text}"
    );
    assert!(text.contains("...ignoring"), "{text}");
    assert!(
        text.contains(r#""r1.exception": "(traceback unavailable)""#),
        "{text}"
    );
    assert!(
        text.contains("rescued Task failed: Action 'ns.coll.m' does not support raw params."),
        "{text}"
    );
    assert!(text.contains("skipping: [probe]"), "{text}");
    assert!(!text.contains("never"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A word that is not `key=value` on a core module that takes no raw parameters fails that task
/// while the play runs. Measured on ansible-core 2.19.12: `ping: foo` fails with `Action
/// 'ansible.builtin.ping' does not support raw params.`, `debug: foo` and `copy: foo` the same
/// way under their own names, `ignore_errors` and `rescue` take it, and the play exits 2. The
/// three cover the three roads a task takes: a Python module, a controller-side module and an
/// action plugin. The helper is a fake: nothing is sent.
///
/// What would make this red: the loader refusing the word (`expected key=value`, exit 4 before
/// `PLAY [`), or any of the three roads running the task anyway.
#[test]
fn a_raw_argument_on_a_core_module_fails_only_that_task() {
    let dir = std::env::temp_dir().join(format!("volant-rawcore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let entry = |name: &str| {
        format!(
            r#""{name}":{{"module_fqn":"ansible.modules.{name}","profile":"legacy","rlimit_nofile":1024,"extensions":{{}}}}"#
        )
    };
    let answer = format!(
        r#"{{"zip_b64":"UEsDBA==","modules":{{{}}}}}"#,
        ["ping", "copy", "stat", "file"].map(entry).join(",")
    );
    let python = fake_python_answering(&dir, &[answer.as_bytes()]);
    std::fs::write(dir.join("inv.ini"), "probe ansible_connection=local\n").expect("an inventory");
    let play = dir.join("play.yml");
    std::fs::write(
        &play,
        "- hosts: all\n  gather_facts: false\n  tasks:\n\
         \x20   - ping: foo\n\
         \x20     register: r1\n\
         \x20     ignore_errors: true\n\
         \x20   - debug: {var: r1.exception}\n\
         \x20   - block:\n\
         \x20       - debug: foo\n\
         \x20     rescue:\n\
         \x20       - debug: {msg: \"rescued {{ ansible_failed_result.msg }}\"}\n\
         \x20   - copy: foo\n\
         \x20     ignore_errors: true\n\
         \x20   - ping: foo\n\
         \x20   - debug: {msg: never}\n",
    )
    .expect("a playbook");
    let out = volant_within_env(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            play.to_str().expect("a path"),
        ],
        DEFAULT_DEADLINE,
        &[("VOLANT_PYTHON", python.to_str().expect("a path"))],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{text}\n{err}");
    let line = |module: &str| {
        format!(
            r#"fatal: [probe]: FAILED! => {{"changed": false, "msg": "Task failed: Action 'ansible.builtin.{module}' does not support raw params."}}"#
        )
    };
    assert_eq!(text.matches(&line("ping")).count(), 2, "{text}");
    assert_eq!(text.matches(&line("copy")).count(), 1, "{text}");
    assert!(
        text.contains(r#""r1.exception": "(traceback unavailable)""#),
        "{text}"
    );
    assert!(
        text.contains(
            "rescued Task failed: Action 'ansible.builtin.debug' does not support raw params."
        ),
        "{text}"
    );
    assert!(!text.contains("never"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}
