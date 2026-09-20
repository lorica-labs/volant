// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(unix)]
use std::fmt::Write as _;
use std::path::Path;
use std::process::{Command, Output};

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
        .env_remove("VOLANT_BATCHING");
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
    if stdin_line.is_some() {
        command.stdin(std::process::Stdio::piped());
    }
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("volant starts");
    if let Some(line) = stdin_line {
        use std::io::Write as _;
        child
            .stdin
            .take()
            .expect("volant's standard input")
            .write_all(line.as_bytes())
            .expect("the password reaches volant");
    }
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
    let config = cfg.display().to_string();
    let list = |run_tags: Option<&str>, args: &[&str]| -> String {
        let mut argv = vec!["playbook", "-i", "inv.ini", "--list-tasks"];
        argv.extend_from_slice(args);
        argv.push("tags.yml");
        let mut envs = vec![("ANSIBLE_CONFIG", config.as_str())];
        if let Some(tags) = run_tags {
            envs.push(("ANSIBLE_RUN_TAGS", tags));
        }
        let out = volant_within_full(
            &argv,
            DEFAULT_DEADLINE,
            None,
            None,
            Some(&listing),
            None,
            &envs,
        );
        String::from_utf8_lossy(&out.stdout).to_string()
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
    let out = volant_within_env(
        &["playbook", &fixture("cfg/site.yml")],
        DEFAULT_DEADLINE,
        &[("ANSIBLE_CONFIG", &fixture("cfg/bad-forks.cfg"))],
    );
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

/// A `remote_tmp` whose `~user` part holds anything but a user name is refused when the
/// configuration is read, before a connection is opened.
///
/// The assertion is the exit code and the value in the message: exit 5 is what an unusable
/// configuration value gets, measured in the 1.4 plan, and naming the value is what lets an
/// operator find it.
///
/// What would make this red: `shell_word` leaving the segment bare and nothing checking it,
/// which is what this release does -- the substitution reaches the remote shell.
#[test]
fn a_remote_tmp_with_a_substitution_in_its_home_part_is_refused() {
    let out = volant_within_env(
        &["playbook", &fixture("cfg/site.yml")],
        DEFAULT_DEADLINE,
        &[("ANSIBLE_CONFIG", &fixture("cfg/bad-remote-tmp.cfg"))],
    );
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(5), "{stdout}\n{stderr}");
    assert!(stderr.contains("~$(id)/x"), "{stderr}");
    assert!(!stdout.contains("PLAY"), "a refusal runs no play: {stdout}");
}

/// The other direction, and the one that says the rule is not simply "refuse every tilde": `~`,
/// `~root` and `~some.user-1` are user names and still reach the remote shell bare, which is the
/// only thing that can expand them. Read from `ANSIBLE_REMOTE_TMP` rather than a fixture per
/// value, since only the value under test changes.
///
/// What would make this red: a validator refusing every `~`, which would break the default
/// `remote_tmp` and every inventory that sets one.
#[test]
fn an_ordinary_tilde_user_is_still_passed_through() {
    for value in ["~", "~root", "~some.user-1"] {
        let out = volant_within_env(
            &["playbook", &fixture("cfg/site.yml")],
            DEFAULT_DEADLINE,
            &[
                ("ANSIBLE_CONFIG", &fixture("cfg/ansible.cfg")),
                ("ANSIBLE_REMOTE_TMP", value),
            ],
        );
        assert_eq!(
            out.status.code(),
            Some(0),
            "'{value}': {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
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

fn volant_with_path(args: &[&str], dir: &Path) -> Output {
    volant_within_with_path(args, DEFAULT_DEADLINE, Some(dir), &[])
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

/// Six escalated tasks, each closing its batch with a `register`. With batching asked for, the
/// escalated agent opens once: the link and the fork permit that bounds it stay with the driver
/// while the next step needs nobody else. Strict `linear` is the other half of the same rule and
/// the price of the default -- the permit goes back in front of every task, and the escalated
/// link, which `forks` bounds along with it, goes back with it -- so the same play pays one
/// escalation per task there.
///
/// Both counts are asserted here rather than in two tests, because they are one mechanism read
/// at its two settings and a second copy would only restate the first.
///
/// Counted through a `sudo` that records every invocation and then runs the command it was
/// given as the invoking user: the count is what is being measured, and needing real privileges
/// to measure it would make this a test of the machine.
///
/// What would make this red: the driver handing back its escalated links after every batch with
/// batching asked for, which puts six launches in the log instead of one; or holding them across
/// a barrier without one, which lets an escalated connection outlive the permit that bounds it.
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
        strict, 6,
        "one escalated agent per task under the strict default:\n{recorded}"
    );
}

/// The other half of keeping the fork permit across a batch: one fork for two hosts, and a
/// step that waits for the other host right behind a `register` that ended a batch. The driver
/// has to hand the permit back in front of that wait, or the host holding it waits for a host
/// that can never be given one.
///
/// `-f 1` is what makes it a proof: with a fork per host both of them hold one anyway and a
/// permit held across the wait costs nothing. `volant_within` is what makes it a failure - a
/// deadlock does not print a wrong answer, it prints nothing at all.
///
/// What would make this red: the permit kept across the boundary the second task reads
/// `hostvars` at.
#[test]
fn a_barrier_behind_a_registered_task_opens_with_one_fork() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("vars/inventory.ini"),
            "-f",
            "1",
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
    assert!(text.contains("stamped-alpha"), "beta read alpha: {text}");
    assert_eq!(
        text.matches("failed=0").count(),
        2,
        "both hosts reach the recap: {text}"
    );
}

/// The same proof at the setting where the boundary has to be found rather than assumed. Under
/// the strict default every step is a boundary, so the test above no longer says whether the
/// driver hands its permit back at the boundary the textual scan raised, only that it hands it
/// back somewhere. With batching asked for, the second task's `hostvars` is the only boundary in
/// the play, and the permit has to go back in front of that one.
///
/// What would make this red: the permit kept across the boundary `reads_across_hosts` raised,
/// which deadlocks the host holding it against a host that can never be given one.
#[test]
fn a_barrier_behind_a_registered_task_opens_with_one_fork_with_batching() {
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
        &[("VOLANT_BATCHING", "1")],
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

/// The same rule at the other kind of wait: a flush point, where a host waits for the splice
/// the coordinator makes once every host has reached it. One fork, two hosts, and the flush
/// sits right behind a `register`.
///
/// Under the default this no longer reads the splice rule: every step is a boundary, so the
/// release in front of the wait is unconditional and the two splice disjuncts behind it in the
/// `||` chain are never evaluated. What it still guards is that the run finishes and the
/// handler runs on both hosts. The companion below is what holds the splice rule.
///
/// What would make this red: the permit kept in front of a wait at all.
#[test]
fn a_flush_point_behind_a_registered_task_opens_with_one_fork() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("handlers/inv.ini"),
            "-f",
            "1",
            &fixture("handlers/flush-behind-a-register.yml"),
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

/// The same proof at the setting where the splice point is the only thing that can free the
/// permit. Under the strict default the release above is asked for every step, and
/// `is_boundary` answers first in the `||` chain that guards it, so the two splice disjuncts
/// behind it are never evaluated and the test above no longer says anything about them. With
/// batching asked for, the `register` is not a boundary and the flush point is the only
/// disjunct left that can hand the permit back.
///
/// What would make this red: the permit kept across a splice point, which is the deadlock the
/// splice rule exists to forbid. It hangs rather than printing a wrong answer, so
/// `volant_within` is what sees it.
#[test]
fn a_flush_point_behind_a_registered_task_opens_with_one_fork_with_batching() {
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
        &[("VOLANT_BATCHING", "1")],
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
    let out = volant_feeding_stdin(
        &["playbook", "-K", &fixture("become.yml")],
        &dir,
        "not-the-password\n",
    );
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
    let out = volant_within_full(
        &["playbook", &fixture("become.yml")],
        DEFAULT_DEADLINE,
        None,
        Some(&dir),
        None,
        None,
        &[],
    );
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

/// Measured against the reference: `beta` reads the stamp `alpha` registered at the previous
/// task, and both hosts see each other in the play's live list.
///
/// One run, not ten. The repetition this test used to carry was there to catch a barrier that
/// opened on nothing and passed by luck, and under the default there is no barrier that can do
/// nothing: every step is one. The companion below keeps the ten runs, because that is where a
/// barrier can still go missing. What this one is kept for is the result and the live list,
/// which are what the reference was read for.
///
/// What would make this red: `beta` printing the stamp's template or an empty value, or either
/// host dropping out of `ansible_play_hosts`.
#[test]
fn a_host_reading_hostvars_waits_for_the_others() {
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

/// The same play with batching asked for, which is where the textual scan is the only thing
/// standing between `beta` and a stamp `alpha` has not written yet. Under the strict default the
/// test above passes whatever `reads_across_hosts` answers, so this is the copy that still
/// guards it. Ten runs, because a barrier that does nothing passes this once by luck, and with
/// batching asked for a barrier that does nothing is exactly what a broken scan leaves behind.
///
/// What would make this red: `hostvars` dropped from `CROSS_HOST_NAMES`, or the scan no longer
/// reading the argument a `msg` was written in.
#[test]
fn a_host_reading_hostvars_waits_for_the_others_with_batching() {
    for _ in 0..10 {
        let out = volant_within_env(
            &[
                "playbook",
                "-i",
                &fixture("vars/inventory.ini"),
                &fixture("hostvars-barrier.yml"),
            ],
            std::time::Duration::from_secs(20),
            &[("VOLANT_BATCHING", "1")],
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

/// An escalation the host refuses hands its fork back before the driver carries on.
///
/// The refusal is a task failure, so the host stays in the run and walks to the block's
/// `rescue`, stepping over a flush point on the way and waiting there for the splice. With
/// `-f 1` the fork it still held would be the only one, and the other host could never reach
/// that same flush to open it.
///
/// Measured on ansible-core 2.19.12 on this fixture, with a `sudo` that refuses and `-f 1`:
/// `fatal:` for both hosts under `escalates`, then `recovered` for both, then
/// `RUNNING HANDLER [the handler]` for both at the flush that closes `tasks`; recap
/// `ok=3 changed=1 rescued=1` each, exit 0. The flush written in the body shows no banner: both
/// hosts stepped over it.
///
/// What would make this red: a permit held across the wait that follows the refusal. Only
/// `volant_within` can see it, since the run hangs rather than printing anything wrong.
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

/// A handler notifying a handler defined **after** it: the second one runs, in the same flush.
///
/// Measured on ansible-core 2.19.12 on this fixture: `notifies the first handler` changed, then
/// `RUNNING HANDLER [the first handler]` changed, then `RUNNING HANDLER [the second handler]`
/// with `second-ran`; recap `ok=3 changed=2`, exit 0. The other direction is the one that runs
/// nothing - a handler notified by a handler defined before it runs in neither flush - which is
/// what makes a flush's notifications die with it.
///
/// What would make this red: a notification raised while the host is walking the handler steps
/// being dropped, which is the same mechanism read one step too far.
#[test]
fn a_handler_notifies_a_handler_defined_behind_it() {
    let out = volant_within(
        &["playbook", &fixture("handlers/chained.yml")],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        handler_section(&text, "the second handler").contains("second-ran"),
        "the handler a handler notified runs in that same flush: {text}"
    );
    assert!(
        text.contains(
            "localhost                  : ok=3    changed=2    unreachable=0    failed=0    skipped=0    rescued=0"
        ),
        "{text}"
    );
}

/// `ANSIBLE_FORCE_HANDLERS` in the environment does what `--force-handlers` does, and an
/// unrecognised value leaves it off.
///
/// Measured on ansible-core 2.19.12 with this fixture: `ansible-config list` gives the setting
/// one environment name, `ANSIBLE_FORCE_HANDLERS`, and it is typed `boolean`. Run end to end,
/// `True`, `yes` and `1` each give `h2 ok=7`, while `0` and `maybe` give `ok=6` - the same two
/// recap lines as the flag and its absence.
///
/// What would make this red: the environment arm deleted, or reading a name the reference does
/// not answer to. Both are the accepted-then-ignored shape, and neither reddens anything else.
#[test]
fn the_environment_forces_handlers_the_way_the_flag_does() {
    for (value, recap) in [("yes", "ok=7"), ("maybe", "ok=6")] {
        let out = volant_within_env(
            &[
                "playbook",
                "-i",
                &fixture("handlers/inv.ini"),
                &fixture("handlers/handlers.yml"),
            ],
            std::time::Duration::from_secs(20),
            &[("ANSIBLE_FORCE_HANDLERS", value)],
        );
        let text = String::from_utf8(out.stdout).unwrap();
        assert_eq!(out.status.code(), Some(2), "{text}");
        assert!(
            text.contains(&format!(
                "h2                         : {recap}    changed=3    unreachable=0    failed=1    skipped=1"
            )),
            "ANSIBLE_FORCE_HANDLERS={value} should give {recap}: {text}"
        );
    }
}

/// `[defaults] force_handlers` in `ansible.cfg` does the same.
///
/// Measured on ansible-core 2.19.12 with this fixture: `force_handlers = True` gives
/// `h2 ok=7`, `False` gives `ok=6`. `ansible-config list` puts the key in the `defaults`
/// section.
///
/// What would make this red: the file arm deleted, which leaves a setting the file is allowed to
/// carry and nothing reads.
#[test]
fn ansible_cfg_forces_handlers_the_way_the_flag_does() {
    let out = volant_within_env(
        &[
            "playbook",
            "-i",
            &fixture("handlers/inv.ini"),
            &fixture("handlers/handlers.yml"),
        ],
        std::time::Duration::from_secs(20),
        &[("ANSIBLE_CONFIG", &fixture("handlers/force.cfg"))],
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(
        text.contains(
            "h2                         : ok=7    changed=3    unreachable=0    failed=1    skipped=1"
        ),
        "{text}"
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

/// A `meta: flush_handlers` written inside a dynamic include belongs to the hosts that asked for
/// that include, and to no others.
///
/// Measured on ansible-core 2.19.12 with `h1` alone including the file: `included: ... for h1`,
/// `TASK [flush inside the include]`, then `RUNNING HANDLER [the handler]` with `handler for h1`
/// alone; `TASK [after]` for both; and a **second** `RUNNING HANDLER` at the end of the play
/// carrying `handler for h2`. Recap `h1 ok=4 changed=1`, `h2 ok=3 changed=1 skipped=1`, exit 0.
///
/// This is where the splice and the include mask meet, and nothing combined them before. The
/// handler steps the coordinator splices in behind a flush carry the flush step's own mask, so a
/// host outside it walks them and runs none. And that host never entered the flush, so it keeps
/// the notifications it is carrying for the flush it does reach.
///
/// What would make this red: handler steps spliced in unmasked, which runs `h2`'s handler early,
/// under `h1`'s flush, at a point `h2` never asked to flush at; or a masked-out host counted as
/// having flushed, which throws its notification away and leaves `handler for h2` out of the run
/// altogether.
#[test]
fn a_flush_inside_an_include_runs_the_handlers_of_the_hosts_that_asked_for_it() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("handlers/inv.ini"),
            &fixture("handlers/flush-in-include.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    let banners = text
        .lines()
        .filter(|l| l.starts_with("RUNNING HANDLER ["))
        .count();
    assert_eq!(banners, 2, "one flush each, at different points: {text}");
    let first = handler_section(&text, "the handler");
    assert!(
        first.contains("handler for h1") && !first.contains("handler for h2"),
        "only the host whose include carried the flush runs its handler there: {text}"
    );
    assert!(
        text.contains("handler for h2"),
        "and the other host still runs its own, at the flush that closes the play: {text}"
    );
    assert!(
        text.find("after on h2").unwrap() < text.rfind("handler for h2").unwrap(),
        "h2's handler goes at the end of the play, behind the last task: {text}"
    );
    assert!(
        text.contains(
            "h1                         : ok=4    changed=1    unreachable=0    failed=0    skipped=0    rescued=0"
        ) && text.contains(
            "h2                         : ok=3    changed=1    unreachable=0    failed=0    skipped=1    rescued=0"
        ),
        "{text}"
    );
}

/// A local failure that steps over an include, with fewer forks than hosts.
///
/// `-f 1` and two hosts, so there is exactly one permit in the play. `h1` takes it for the remote
/// task, keeps it - the task ends its batch on its `register`, not on a wait - and then fails at a
/// `debug` the next step, with an empty batch. The jump its failure takes runs from that step to
/// the rescue, and the `include_tasks` sits in between, so `h1` steps over a splice point and
/// waits there for the coordinator. `h2` cannot get to that same index without a permit, and the
/// coordinator cannot splice until it does.
///
/// The permit release at the end of a batch is skipped for an empty one, and the release at the
/// top of the step loop asks `steps_over_a_splice_point`, which reads the step's **success**
/// successor - the include itself, one step along, stepped over by nobody. Neither one looks at
/// the range a failure is about to jump across. So the permit was still in hand at the wait.
///
/// Recap `rescued=1` for both hosts and exit 0, which is what the rescue makes of it.
///
/// Under the default the permit is already back before the `debug` runs, because every step is
/// a boundary and the step loop releases it there, so this one no longer reads the failure
/// path's own release. What it still guards is that both hosts get through the jump and into
/// the rescue. The companion below is what holds the release on the failure path.
///
/// What would make this red: the permit kept in front of a wait at all. The run then hangs and
/// only the deadline sees it - both hosts sit in a wait, print nothing more, and no assertion
/// on the output can fail on that.
#[test]
fn a_failure_that_steps_over_an_include_gives_its_fork_permit_back() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("include/inv.ini"),
            "-f",
            "1",
            &fixture("include/fail-then-splice.yml"),
        ],
        PROBE_DEADLINE,
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

/// The same jump at the setting where the failure path's own release is the only thing that can
/// free the permit. Under the strict default the step loop hands the permit back in front of
/// the `debug`, because every step is a boundary there, so the test above never reaches the
/// jump still holding one and no longer says anything about it. Measured: with the release on
/// the failure path deleted, the test above still passes and this one hangs.
///
/// What would make this red: the permit, or an escalated link, kept across a failure jump. The
/// run then hangs and only the deadline sees it.
#[test]
fn a_failure_that_steps_over_an_include_gives_its_fork_permit_back_with_batching() {
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
        &[("VOLANT_BATCHING", "1")],
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

/// A flush point stepped over from the step immediately in front of it, with that step still in
/// the batch.
///
/// The block's body is emptied by `--skip-tags`, so the rescue's `meta: flush_handlers` is the
/// very next step in the list behind the task that notifies. Measured on ansible-core 2.19.12
/// with `--skip-tags dropped`: `notifies the handler` changed, `after the block`, then
/// `RUNNING HANDLER [the handler]`; recap `ok=3 changed=1`, exit 0. The rescue is never entered,
/// so its flush never runs and the handler goes at the one that closes `tasks`.
///
/// What would make this red: a host that reaches a flush point it steps over while it still owes
/// the coordinator a `TaskDone` for a step in front of it. It would then wait for a splice at an
/// index the coordinator cannot walk to, because the coordinator is itself waiting for that very
/// `TaskDone` - a deadlock neither side can leave. Only `volant_within` can see it; no assertion
/// on printed output can fail on a hang.
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
///
/// Every figure below is the reference's: `retries: R` is **R attempts in all**, the line after
/// failed attempt `i` reads `(R - i + 1 retries left)` so the last one says `(1 retries left)`,
/// `until` with no `retries` gives three attempts, `retries` with no `until` retries while the
/// result is failed, and a loop retries **each item on its own** - one line per item, and
/// `"attempts": 1` in each item's own result.
///
/// What would make this red: `attempts` counting `retries + 1`, the last retry line missing,
/// the count starting one lower, or the loop retrying all its items together instead of one at
/// a time.
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

/// A templated task name renders in the `FAILED - RETRYING` line the way it renders everywhere
/// else, rather than showing its own template braces.
///
/// What would make this red: the retry line sending `task.name` raw instead of the rendered
/// form - `probe {{ n }}` in the line instead of `probe 3`.
#[test]
fn a_templated_task_name_renders_in_the_retry_line() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("inventory.ini"),
            &fixture("until/templated-name.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains("FAILED - RETRYING: [localhost]: probe 3 (1 retries left)."),
        "{text}"
    );
    assert!(
        !text.contains("{{"),
        "the retry line must not show the template's braces: {text}"
    );
}

/// A task that never leaves the controller retries by the same loop as any other.
///
/// Measured on ansible-core 2.19.12 with this fixture: two retry lines, `fatal:` carrying the
/// `debug`'s own cleaned body, `...ignoring`, `attempts: 2` in the registered value and a recap
/// of `ok=2 ignored=1`. Two differences with the reference are deliberate and recorded: it
/// appends `Result was: {...}` to each retry line for a `debug` and puts a `retries` key in the
/// result, and this prints neither.
///
/// What would make this red: the retry loop skipped for `set_fact` and `debug`, which is how a
/// task carrying `until` would quietly run once and pass.
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
        retried.contains(r#"fatal: [localhost]: FAILED! => {"msg": "probe"}"#)
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
///
/// Measured on ansible-core 2.19.12: `fatal:` carrying
/// `Task failed: Error while evaluating conditional: ...`, **no** retry line, no `attempts`, and
/// the run carries on. The sentence after the colon is this engine's own - the reference names
/// the Python type - and the prefix is the reference's, which is what a playbook testing
/// `'Task failed' in result.msg` reads.
///
/// What would make this red: a condition that throws treated as false, which would retry and
/// print the retry lines this asserts are absent.
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

/// `delay` is waited after every failed attempt, the last one included - not only between
/// attempts - proved from the results rather than from the clock: the task before records the
/// epoch second it ran at, the retried task's own `stdout` is the epoch second of its **last**
/// attempt, and the task right after it records its own epoch second in turn. Two attempts with
/// `delay: 2` put at least two seconds both between the two attempts and between the last
/// attempt and the next task, and the run is under `volant_within`, so a `delay` that never ends
/// hangs the test rather than passing it.
///
/// This is the highest-blast-radius fact `until`/`retries`/`delay` measured: a failing playbook
/// costs `retries * delay`, not `(retries - 1) * delay`. A test with only the first gap would
/// stay green if the trailing sleep were dropped - proving only that a sleep happens somewhere,
/// not that the last attempt costs one too.
///
/// What would make this red: either sleep dropped - the two attempts landing in the same second
/// or at worst one apart and never two, or the task after the last attempt starting in the same
/// second as that attempt instead of at least two seconds later.
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
        stamps[1] - stamps[0] >= 2,
        "the second attempt waited {} second(s), not the two `delay` asked for",
        stamps[1] - stamps[0]
    );
    assert!(
        stamps[2] - stamps[1] >= 2,
        "the task after the last attempt started {} second(s) later, not the two `delay` asked \
         for a failing attempt to cost even when it is the last one",
        stamps[2] - stamps[1]
    );
}

/// Every path a `no_log` result can reach the operator by, measured on ansible-core 2.19.12 with
/// this fixture at verbosity 0, `-v` and `-vvv`.
///
/// The one that is **not** censored is the registered variable: `debug: var=s` behind the task
/// shows the real `stdout`, and censoring it too would look safer and be wrong - a playbook
/// reading `s.rc` would stop working.
///
/// What would make this red: a secret on any line of a censored task at any verbosity, or the
/// registered value censored along with the display.
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
        assert!(
            failed.contains(&format!(
                r#"fatal: [localhost]: FAILED! => {{"censored": "{CENSORED}", "changed": true}}"#
            )) && failed.contains("...ignoring"),
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

/// `environment`, measured on ansible-core 2.19.12 with this fixture: the play's layer and the
/// task's are both in force with the task's winning, a templated value is rendered, `42` reaches
/// the process as the string `42`, a block's layer reaches its tasks, and a value that is not a
/// mapping warns on **stderr** and is skipped while the task still runs and the layers around it
/// stay. The `debug` at the end shows that an `environment` on a controller-side task changes
/// nothing for `lookup('env', ...)`.
///
/// The warning quotes the whole layer stack the way the reference quotes it, as a Python list
/// of the **raw** layers: measured, `['{{ secret }}']` for a one-layer stack, so nothing that
/// was rendered from a variable reaches the line.
///
/// What would make this red: the play's layer lost (`" templated"`), `42` sent as a number (the
/// agent refuses the shape), or a layer that is not a mapping failing the task.
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
        ("task", "become" | "become_user") | ("play" | "block", "become_user")
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
}

/// A loop variable named after one of the values the whole inventory shares renders the item,
/// not the inventory. Measured on ansible-core 2.19.12: the loop variable is bound once the
/// host's variables are built, so it is the last write there is and it wins over the play's own
/// host list for as long as the loop runs.
///
/// What would make this red: reading `play_hosts` out of the shared inventory-wide values with
/// no account of what was written into the host's map afterwards. Both items then render the
/// play's host list instead of `1` and `2`.
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
///
/// Measured on ansible-core 2.19.12 on this fixture: `base` runs six times - four entries in
/// `roles:` counting `child`'s dependency, then once for the bare `import_role: { name: base }` in
/// `tasks:` whose entry is identical to the bare one the list already ran, then once for the
/// `include_role` behind it.
///
/// What would make this red: letting either statement consult the play's own list of what it has
/// run, which drops it and leaves five - a run reporting success having done less than the
/// playbook asked for.
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

/// Everything an `include_tasks` does, on the campaign's own fixture.
///
/// Measured on ansible-core 2.19.12, line for line. A static include names one absolute path for
/// both hosts; a name templated per host gives one `included:` line each, and the tasks behind
/// them run for their own host alone; a `loop` gives one line per item with its own `=> (item=)`
/// and the tasks repeat per item; an include a `when` left out for one host is `skipping:` there
/// and runs for the other; and a file that is not there is a `fatal:` carrying `include` as the
/// playbook wrote it and `reason` naming the controller path. Recap `h1 ok=10 skipped=1`,
/// `h2 ok=9 failed=1`, exit 2.
///
/// What would make this red: the host mask ignored, which runs `from a` on h2 in the dynamic
/// include; one `ok` for a two-item loop rather than one per item, which reads `ok=9`; a missing
/// file reported as `UNREACHABLE` instead of a task failure; or a splice that nobody waits for,
/// which hangs and `volant_within` fails rather than reporting an elapsed time.
#[test]
fn include_tasks_splices_what_each_host_asked_for() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("include/inv.ini"),
            &fixture("include/include.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2), "{text}");
    let dir = fixture("include");
    for line in [
        format!("included: {dir}/inc-a.yml for h1, h2"),
        format!("included: {dir}/inc-a.yml for h1"),
        format!("included: {dir}/inc-b.yml for h2"),
        format!("included: {dir}/inc-a.yml for h1, h2 => (item=1)"),
        format!("included: {dir}/inc-a.yml for h1, h2 => (item=2)"),
    ] {
        assert!(text.contains(&line), "missing {line}:\n{text}");
    }
    // The dynamic include ran `from a` for h1 alone and `from b` for h2 alone, each under its
    // own banner: the mask, not the banner, is what decides who shows a line.
    assert_eq!(text.matches("TASK [from a]").count(), 4, "{text}");
    assert_eq!(text.matches("\"msg\": \"a on h2\"").count(), 3, "{text}");
    assert!(text.contains("skipping: [h1]"), "{text}");
    assert!(
        text.contains(
            "fatal: [h2]: FAILED! => {\"changed\": false, \"include\": \"nosuch.yml\", \"reason\": \"Could not find or access '"
        ),
        "{text}"
    );
    assert!(
        text.contains("h1                         : ok=10   changed=0    unreachable=0    failed=0    skipped=1"),
        "{text}"
    );
    assert!(
        text.contains("h2                         : ok=9    changed=0    unreachable=0    failed=1    skipped=0"),
        "{text}"
    );
}

/// The tags written on an `include_tasks` stop at the statement.
///
/// Measured on ansible-core 2.19.12 with the campaign's `tags2.yml`: `--tags inc` runs the
/// statement - the `included:` line is there - and **not** the untagged task it brought in, while
/// the play's own `always` task still runs. Measured the other way round too: `--tags playtag`,
/// the play's own tag, does reach the included task, so what stops at the statement is its own
/// tags and not the play's.
///
/// What would make this red: handing the statement's tags to the tasks it brought in, which runs
/// `from b` under `--tags inc`.
#[test]
fn an_include_does_not_hand_its_tags_to_what_it_brings_in() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("include/inv.ini"),
            "--tags",
            "inc",
            &fixture("include/tags.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("included: "), "{text}");
    assert!(
        !text.contains("TASK [from b]"),
        "the tags stop there: {text}"
    );
    assert!(text.contains("TASK [always]"), "{text}");
}

/// `include_vars` reads a file of variables and they beat the play's own.
///
/// Measured on ansible-core 2.19.12 with the campaign's `include-vars.yml`: the first statement
/// overrides the play's `iv: play`, a `name:` groups the file under that one key, and a file that
/// is nowhere fails with `ansible_included_var_files` empty and every path it looked in listed.
/// With `ignore_errors` the run still exits 0 with `ignored=1`.
///
/// What would make this red: the variables landing under the play's `vars:` instead of over them,
/// which reads `iv=play`; a `name:` ignored, which leaves `ns.iv` undefined; or a file nobody can
/// find reported as found.
#[test]
fn include_vars_sets_the_file_s_variables_over_the_play_s() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("include/inv.ini"),
            &fixture("include/vars.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("\"iv\": \"from-include\""), "{text}");
    assert!(text.contains("\"ns.iv\": \"from-include\""), "{text}");
    assert!(
        text.contains("\"ansible_included_var_files\": []"),
        "{text}"
    );
    assert!(text.contains("Searched in:"), "{text}");
    assert!(text.contains("...ignoring"), "{text}");
    assert!(
        text.contains("localhost") || text.contains("h1                         : ok=5"),
        "{text}"
    );
}

/// A `rescue` around an `include_tasks` takes a failure raised by a task the include brought in.
///
/// Measured on ansible-core 2.19.12: a block whose body includes a file whose task fails runs the
/// block's `rescue`, then its `always`, then the task behind the block, and the recap reads
/// `ok=5 rescued=1` at exit 0. `ansible_failed_task.name` is the included task's own name.
///
/// What would make this red: the spliced steps landing outside the block, so the failure has no
/// rescue to reach and the host leaves the play - `rescued=0 failed=1` and exit 2.
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

/// An include in a `rescue` and an include in an `always`, both reached by a host on its way out
/// of a failed block, plus a `loop` over an empty list.
///
/// Measured on ansible-core 2.19.12, line for line: the empty loop shows one `skipping:` per host
/// and counts one `skipped`, and both includes bring their file in for both hosts. Recap
/// `ok=5 skipped=1 rescued=1` for each host, exit 0.
///
/// What this proves is that an include is reached from inside a `rescue` and from inside an
/// `always` that runs behind one. Both hosts are rescued here, so neither is draining anything:
/// the shape where a splice moves an index a host is already carrying is `draining.yml` below,
/// and the assertions here cannot speak for it. What would make this red: a splice landing
/// outside the section the statement sat in, so one of the two `included:` lines never prints;
/// or an include reached with the batch unreported, which is a deadlock and fails on the
/// deadline rather than on an assertion.
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
///
/// Measured on ansible-core 2.19.12: the body fails, the `always` runs its include and then the
/// task behind it, the step after the block does not run, and the recap reads
/// `ok=3 failed=1` at exit 2 for each host - the `include_tasks` statement itself counting one of
/// those three.
///
/// This is the shape where a splice moves an index a host is already carrying, and it is the
/// only test here that can see it: the end of the `always` section the host is draining sits
/// past the insertion point, and the driver moves it by however much the list grew. What would
/// make this red: that index left where it was, which ends the drain one step early -
/// `second always task` never runs and the recap reads `ok=2`.
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

/// A block written inside an `always` a host is draining rescues its own failure, and the host
/// goes on draining behind it - with an include between the failure and that rescue, so the
/// list grows while the host holds the index the section ends at.
///
/// Measured on ansible-core 2.19.12: `h1` fails the outer body, fails again inside the cleanup's
/// own block, is taken by that block's `rescue`, and then still runs `rest of the always`;
/// `past the block` runs for neither host. Recap `h1 ok=2 failed=1 rescued=1` and
/// `h2 ok=4 failed=1 skipped=1 rescued=0`, exit 2. `h2` skips the failing task and is the host
/// that asks for the include, which is what makes the list grow under `h1`.
///
/// What would make this red: the end of the drained section left where it was while the steps
/// the include brought in went in front of it. The host then reaches its own cleanup's end one
/// step early, `rest of the always` never runs for `h1`, and the recap reads `ok=1` - a cleanup
/// step nobody ran and nothing said so.
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
///
/// Measured on ansible-core 2.19.12: it is not bounded there at all - the file is read again and
/// again until Python's stack is gone, four thousand lines of output later, and
/// `ansible-playbook` exits **250** reporting its own crash. This engine refuses at the depth its
/// other recursions share, in its own words, as an ordinary task failure with a recap behind it.
///
/// What would make this red: the ceiling removed, which grows the step list until the process is
/// killed and `volant_within` fails on the deadline rather than on an assertion.
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
///
/// `include_role: public: true` is measured to export the role's `defaults` and `vars` to
/// everything behind the statement, where the default keeps them to the role's own tasks - which
/// would mean changing the play's variable layers while its hosts stand at different steps of it.
/// `include_vars: dir:` is measured to read every file of a directory in name order. Both are
/// refused rather than read and dropped.
///
/// What would make this red: either option accepted and forgotten, which runs the playbook
/// without what the operator asked for and reports success.
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

/// `include_tasks` takes `file`, `_raw_params` and `apply`, and `apply` itself is refused: this
/// release has no layer to put the keywords it carries on. Both an unknown key and `apply` are
/// caught before the first connection, the same as every other include and import statement.
///
/// What would make this red: `include_options` accepting anything, which would run the include
/// and report success having ignored what the operator wrote.
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

/// `import_playbook` is a play-level statement; written as a task, the reference cannot run it
/// either, and dies with its own traceback rather than a message naming what was wrong.
///
/// Measured on ansible-core 2.19.12: `Task failed: Action 'ansible.builtin.import_playbook' does
/// not support raw params.`, exit 2. This engine refuses it the same way, before anything runs.
///
/// What would make this red: the task falling through to the ordinary module path instead, which
/// would try to run a module named `import_playbook` and fail with a different message.
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

/// `serial: 2` plays three hosts as two batches, each with its own banner, its own host lists
/// and its own handlers.
///
/// Measured on ansible-core 2.19.12 with this playbook: `PLAY [all]` twice; the first batch
/// reads `ansible_play_batch` as `h1, h2`, `ansible_play_hosts` as all three and
/// `ansible_play_hosts_all` as all three; `h1` then fails, and `RUNNING HANDLER [h]` runs for
/// `h2` alone at the end of that batch; the second batch reads `h3`, `h2, h3` and all three, and
/// runs the handler again for `h3`. Recap `h1 failed=1`, exit 2.
///
/// What would make this red: one batch instead of two; `ansible_play_hosts` still carrying `h1`
/// in the second batch; `ansible_play_batch` answering with the play rather than with the batch;
/// or the handlers running once at the end of the play instead of once per batch.
#[test]
fn serial_plays_one_batch_at_a_time_with_its_own_handlers() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("serial/inv.ini"),
            &fixture("serial/two.yml"),
        ],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(2), "{text}");
    let order: Vec<String> = lines(&out)
        .into_iter()
        .filter(|l| l.starts_with("PLAY [") || l.starts_with("RUNNING HANDLER ["))
        .map(|l| l.split('*').next().unwrap_or_default().trim().to_string())
        .collect();
    assert_eq!(
        order,
        [
            "PLAY [all]",
            "RUNNING HANDLER [h]",
            "PLAY [all]",
            "RUNNING HANDLER [h]",
        ],
        "one banner and one handler run per batch: {text}"
    );
    assert!(
        text.contains(r#"ok: [h1] => {"msg": "h1,h2 / h1,h2,h3 / h1,h2,h3"}"#),
        "the first batch is h1 and h2 and nobody has failed yet: {text}"
    );
    assert!(
        text.contains(r#"ok: [h3] => {"msg": "h3 / h2,h3 / h1,h2,h3"}"#),
        "the second batch is h3 alone, and the play has lost h1: {text}"
    );
    assert!(
        text.contains(r#"ok: [h2] => {"msg": "handler on h2"}"#)
            && text.contains(r#"ok: [h3] => {"msg": "handler on h3"}"#),
        "each batch runs the handler its own hosts notified: {text}"
    );
    assert!(
        text.contains(
            "h1                         : ok=2    changed=1    unreachable=0    failed=1"
        ) && text.contains(
            "h3                         : ok=3    changed=1    unreachable=0    failed=0"
        ),
        "{text}"
    );
}

/// A batch that loses every host it had ends the whole run: no next batch, no next play, and the
/// recap prints straight away.
///
/// Measured on ansible-core 2.19.12 with `serial: 1` over three hosts, the first failing: one
/// `PLAY [all]`, a recap naming `h1` alone, exit 2. `h2` and `h3` never run, and the handler `h1`
/// notified never runs either.
///
/// What would make this red: the second batch running, which shows a second `PLAY [all]` and puts
/// `h2` in the recap - a run that carried on past the point the reference stops at.
#[test]
fn a_batch_that_loses_every_host_ends_the_run() {
    one_host_batch_that_fails_stops_everything("serial/one_fails.yml");
}

/// A percentage is a batch size like any other: `"34%"` of three hosts is one host, measured, so
/// this playbook runs exactly as the `serial: 1` one above does.
///
/// What would make this red: a percentage read as a plain number - `34` would be one batch of
/// three, so `h2` and `h3` would run and reach the recap.
#[test]
fn a_percentage_serial_is_a_batch_size() {
    one_host_batch_that_fails_stops_everything("serial/percent.yml");
}

/// The body both rows above share: three hosts cut into batches of one, the first failing.
fn one_host_batch_that_fails_stops_everything(name: &str) {
    let out = volant_within(
        &["playbook", "-i", &fixture("serial/inv.ini"), &fixture(name)],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(2), "{name}: {text}");
    assert_eq!(
        text.matches("PLAY [all]").count(),
        1,
        "{name}: the batch that failed is the last one: {text}"
    );
    assert!(
        text.contains(r#"ok: [h1] => {"msg": "h1 / h1,h2,h3 / h1,h2,h3"}"#),
        "{name}: the batch is one host: {text}"
    );
    assert!(
        !text.contains("[h2]") && !text.contains("[h3]"),
        "{name}: no host of a later batch ran: {text}"
    );
    assert!(
        !text.contains("RUNNING HANDLER"),
        "{name}: the only host that notified failed: {text}"
    );
    assert!(
        text.contains("PLAY RECAP")
            && text.contains(
                "h1                         : ok=2    changed=1    unreachable=0    failed=1"
            ),
        "{name}: the recap prints where the run stopped: {text}"
    );
}

/// The same rule without `serial`: a play whose every host fails is a batch that lost every host,
/// so the play behind it never appears.
///
/// Measured on ansible-core 2.19.12: `h1` and `h2` both fail in the first play, `PLAY [` shows
/// once, the second play's `second` is nowhere, exit 2.
///
/// What would make this red: the second play running, which is this repository's most expensive
/// bug family - a run reporting on hosts it never touched.
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
///
/// Measured on ansible-core 2.19.12: after `h1` fails in the first play, `PLAY [h1]` prints with
/// nothing under it - **no** `skipping: no hosts matched`, which is what a pattern that matched
/// nothing gets - and `PLAY [h2]` then runs its task. Exit 2, from the first play's failure.
///
/// What would make this red: `skipping: no hosts matched` under the banner, the banner missing,
/// or the run stopping there the way it stops for a batch that lost its hosts. The last is the
/// one to watch: that rule and this one are one line apart and read the same two host sets.
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
///
/// Measured on ansible-core 2.19.12: after `h1` fails, a second play over `all` with `serial: 1`
/// cuts `[h1]`, `[h2]`, `[h3]` - the failed host still gets a batch and a banner - and `h2` and
/// `h3` then run theirs, followed by a third play. Exit 2, from the first play's failure.
///
/// What would make this red: cutting the batches from the surviving hosts, which merges them and
/// loses a banner; or reading "no live host in this batch" as "this batch lost every host" and
/// ending the run, which would silently drop two hosts and a whole play.
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
        text.contains(r#"ok: [h2] => {"msg": "h2 / h2,h3 / h1,h2,h3"}"#)
            && text.contains(r#"ok: [h3] => {"msg": "h3 / h2,h3 / h1,h2,h3"}"#),
        "the batches of the surviving hosts run, one host each: {text}"
    );
    assert!(
        !text.contains(r#"[h1] => {"msg""#),
        "the failed host runs nothing: {text}"
    );
    assert!(
        text.contains(r#""msg": "third""#),
        "the play behind the emptied batch runs: {text}"
    );
}

/// `vars_files` is read once per batch, not once for the whole play: a path templated on
/// `ansible_play_batch` must read a different file for each batch of a `serial` play, and must
/// not see a host that already failed in the play before it.
///
/// Measured on ansible-core 2.19.12: after `h1` fails, a second play with `serial: 1` over `all`
/// reads `vars_h1.yml` for `h1`'s own (never-run) batch, `vars_h2.yml` when `h2`'s batch runs and
/// `vars_h3.yml` when `h3`'s batch runs - three separate renders and reads, one per batch, each
/// seeing that batch's own `ansible_play_batch`.
///
/// What would make this red: `vars_files` rendered once before the batches are cut, from the
/// play's unfiltered host list - `ansible_play_batch` would then read `h1,h2,h3` for every host,
/// `vars_h1_h2_h3.yml` does not exist among the fixtures, so the entry is silently skipped and
/// `marker` stays undefined for both `h2` and `h3`.
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
        text.contains(r#"ok: [h2] => {"msg": "batch=h2 marker=from-h2"}"#),
        "h2's own batch reads h2's own vars_files: {text}"
    );
    assert!(
        text.contains(r#"ok: [h3] => {"msg": "batch=h3 marker=from-h3"}"#),
        "h3's own batch reads h3's own vars_files, not h2's: {text}"
    );
}

/// A batch that goes unreachable ends the run exactly as a batch that fails does, but at exit 4
/// rather than 2: `PlayEnd::stop_run`'s doc names this shape, and this pins it. `serial: 1` cuts
/// `h1` into its own batch; `h1`'s connection plugin does not exist, so the batch is unreachable
/// rather than failed - the run still stops there, no `h2`, no `h3`, no second play.
///
/// What would make this red: the run treating an unreachable batch as one that merely lost a
/// host rather than one that ends the run, which would let `h2`'s batch start, or exit 2 instead
/// of 4 where the unknown-connection tests above pin 4 for the same trigger outside `serial`.
#[test]
fn a_batch_that_goes_unreachable_ends_the_run_at_exit_4() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("serial/unreachable_batch.ini"),
            &fixture("serial/unreachable_batch.yml"),
        ],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(4), "{text}");
    assert!(text.contains("fatal: [h1]: UNREACHABLE!"), "{text}");
    assert_eq!(
        text.matches("PLAY [all]").count(),
        1,
        "the batch that went unreachable is the last one: {text}"
    );
    assert!(
        !text.contains("[h2]") && !text.contains("[h3]"),
        "no later batch and no second play ran: {text}"
    );
}

/// `--limit` narrows the resolved hosts **before** `serial` cuts the batches, not after: measured
/// on ansible-core 2.19.12, `--limit h2,h3` with `serial: 2` over a three-host inventory leaves
/// one batch of two rather than cutting the original three-host list and then dropping `h1`
/// (which would still show a two-batch shape for a two-host play). `ansible_play_hosts_all` is
/// `h2,h3`, not `h1,h2,h3`.
///
/// What would make this red: cutting from the inventory's full resolved set and filtering `h1` out
/// afterwards, which for this fixture is invisible in the batch count (both give one batch, since
/// `serial: 2` covers two hosts either way) but would leak `h1` into `ansible_play_hosts_all`.
#[test]
fn limit_narrows_before_the_batches_are_cut() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("serial/inv.ini"),
            "-l",
            "h2,h3",
            &fixture("serial/two.yml"),
        ],
        std::time::Duration::from_secs(20),
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(!text.contains("[h1]"), "h1 is limited out entirely: {text}");
    assert_eq!(
        text.matches("PLAY [all]").count(),
        1,
        "one batch, not two: {text}"
    );
    assert!(
        text.contains(r#"ok: [h2] => {"msg": "h2,h3 / h2,h3 / h2,h3"}"#),
        "ansible_play_hosts_all is the limited set, not the inventory's full one: {text}"
    );
}

/// `run_once`, `delegate_to` and `delegate_facts` on the campaign's own fixture.
///
/// Every line below is what ansible-core 2.19.12 printed for this playbook against this
/// inventory: `changed: [h1]` alone under `once`, `once on h1` **and** `once on h2` from the
/// variable one host registered for both, `changed: [h1 -> h3]` and `changed: [h2 -> h3]` for
/// the delegated task, `"msg": "h1"` and `"msg": "h2"` behind it because `inventory_hostname`
/// stays the host the task was written for, `ok: [h1 -> h3]` for the delegated `set_fact`,
/// `"1 none"` twice because `delegate_facts` put the fact on h3 and not on the host that set
/// it, and a recap of `h1 ok=6 changed=2`, `h2 ok=5 changed=1` with **no h3 line at all**.
///
/// What would make this red: the registered variable not handed to the other host (`once on h2`
/// undefined, so a `fatal:`); `inventory_hostname` taken from the delegate (`"msg": "h3"`);
/// `delegate_facts` writing on the delegating host (`"1 1"`); the arrow missing from a
/// delegated line; or h3 counted in the recap for work it did on somebody else's behalf.
#[test]
fn run_once_and_delegate_to_follow_the_reference() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("delegate/inv.ini"),
            &fixture("delegate/delegate.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert_eq!(
        text.matches("changed: [h1]").count(),
        1,
        "one host runs the run_once task and it is the first of the batch: {text}"
    );
    assert!(
        !text.contains("changed: [h2]"),
        "the other host runs nothing for it: {text}"
    );
    assert!(text.contains(r#""msg": "once on h1""#), "{text}");
    assert!(
        text.contains(r#""msg": "once on h2""#),
        "the registered variable reaches the host that did not run it: {text}"
    );
    assert!(text.contains("changed: [h1 -> h3]"), "{text}");
    assert!(text.contains("changed: [h2 -> h3]"), "{text}");
    assert!(
        text.contains(r#""msg": "h1""#) && text.contains(r#""msg": "h2""#),
        "inventory_hostname stays the delegating host: {text}"
    );
    assert!(text.contains("ok: [h1 -> h3]"), "{text}");
    assert_eq!(
        text.matches(r#""msg": "1 none""#).count(),
        2,
        "delegate_facts writes on h3 and not on the host that set it: {text}"
    );
    assert!(
        text.contains("h1                         : ok=6    changed=2"),
        "{text}"
    );
    assert!(
        text.contains("h2                         : ok=5    changed=1"),
        "{text}"
    );
    assert!(
        !text.contains("h3                         :"),
        "a delegate is not in the recap: {text}"
    );
}

/// A `run_once` task that fails takes every other host of the batch out of the play.
///
/// Measured on ansible-core 2.19.12: `fatal: [h1]: FAILED!`, no `after` line for either host,
/// **no recap line for h2** - it ran nothing, so there is nothing to count - and exit 2. The
/// same holds with a `rescue` around the task: the host that ran it is rescued and carries on
/// (`rescued=1`, its own `after the block` line), the other host still leaves, and the run
/// still exits 2.
///
/// What would make this red: the other host carrying on past a task nobody ran for it, which is
/// this project's own worst failure - a run reporting success having done nothing; or the
/// rescued shape exiting 0, which would report success for a play the reference failed.
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

/// A `run_once` runner nothing can reach ends the play for the hosts waiting on it, and the
/// exit code stays the unreachable one.
///
/// Measured on ansible-core 2.19.12 with an unreachable h1: `fatal: [h1]: UNREACHABLE!` with no
/// arrow, no line and no recap entry for h2, and exit **4** - not 6. So a host that leaves
/// because its runner never answered adds nothing of its own to the exit code, while a host
/// that leaves because its runner's task failed adds 2.
///
/// What would make this red: h2 waiting for a verdict that can never come, which hangs the run
/// and reaches the deadline; or h2 counted as failed, which turns the reference's 4 into 6.
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

/// `serial` cuts the play into batches and each batch elects its own runner.
///
/// Measured on ansible-core 2.19.12 with `serial: 1` over three hosts: `changed: [h1]`,
/// `changed: [h2]` and `changed: [h3]`, one per batch, and each host reads its own batch's
/// result. What would make this red: one runner for the whole play, which leaves two batches
/// with an undefined registered variable.
#[test]
fn each_serial_batch_elects_its_own_run_once_runner() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("delegate/inv.ini"),
            &fixture("delegate/run-once-serial.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(0), "{text}");
    for host in ["h1", "h2", "h3"] {
        assert!(
            text.contains(&format!("changed: [{host}]")),
            "{host} runs the task for its own batch: {text}"
        );
        assert!(
            text.contains(&format!(r#""msg": "once on {host}""#)),
            "{text}"
        );
    }
}

/// A `run_once` step an include splices in is elected for against the list that now exists.
///
/// The splice moves every index behind it, and the election for the step that used to sit at the
/// insertion point was already decided - against the step the splice pushed along, and against
/// its mask. The coordinator drops it there rather than letting it name a step it was never about.
///
/// Measured on ansible-core 2.19.12, `h2` alone including a file whose first task is `run_once`:
/// `skipping: [h1]`, `included: ... for h2`, `changed: [h2]` under `once inside the include`,
/// `inside on h2`, then `changed: [h1]` for the `run_once` behind the include and `behind on h1`
/// and `behind on h2` under it. Recap `h1 ok=2 changed=1 skipped=1`, `h2 ok=4 changed=1`, exit 0.
///
/// What would make this red: an election carried across the splice. It names the host the step
/// behind the include was elected for, that host is outside the included step's mask so it runs
/// nothing, and the host the include belongs to reads it as a runner other than itself and waits -
/// so the `run_once` task inside the include is run by nobody and the run still exits 0, with the
/// registered variable undefined for the host that asked for it.
#[test]
fn a_run_once_step_spliced_in_by_an_include_is_elected_for_afresh() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("handlers/inv.ini"),
            &fixture("delegate/run-once-after-include.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains("changed: [h2]") && text.contains(r#""msg": "inside on h2""#),
        "the host the include belongs to runs its `run_once` task: {text}"
    );
    assert!(
        text.contains(r#""msg": "behind on h1""#) && text.contains(r#""msg": "behind on h2""#),
        "and the step behind the include still runs for the whole batch: {text}"
    );
    assert!(
        text.contains(
            "h1                         : ok=2    changed=1    unreachable=0    failed=0    skipped=1    rescued=0"
        ) && text.contains(
            "h2                         : ok=4    changed=1    unreachable=0    failed=0    skipped=0    rescued=0"
        ),
        "{text}"
    );
}

/// An `include_tasks` under `run_once` is read for the elected host alone, and only that host
/// runs what it brought in.
///
/// Measured on ansible-core 2.19.12: `included: <path> for h1`, the included tasks running for
/// h1 with no h2 line under any of their banners, `after` running for both, and a recap of
/// `h1 ok=4 changed=1`, `h2 ok=1`.
///
/// What would make this red: the other host asking for its own copy of the file, which runs the
/// included tasks twice for a statement the playbook said to run once.
#[test]
fn a_run_once_include_is_read_for_the_elected_host_alone() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("delegate/inv.ini"),
            &fixture("delegate/run-once-include.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("for h1"), "{text}");
    assert!(!text.contains("for h1,h2"), "{text}");
    assert_eq!(
        text.matches(r#""msg": "included on h1""#).count(),
        1,
        "{text}"
    );
    assert!(!text.contains("included on h2"), "{text}");
    assert!(text.contains(r#""msg": "after on h1""#), "{text}");
    assert!(text.contains(r#""msg": "after on h2""#), "{text}");
    assert!(
        text.contains("h1                         : ok=4    changed=1"),
        "{text}"
    );
    assert!(
        text.contains("h2                         : ok=1    changed=0"),
        "{text}"
    );
}

/// A `run_once` step's runner is elected among the hosts the step's mask includes, and every
/// other host of the batch waits for it - the ones the mask leaves out as well.
///
/// Three hosts and two plays, each with an `include_tasks` a `when` keeps one host out of and a
/// `run_once` task inside whose registered value the next task reads. The first play leaves out
/// h2, so the first live host is inside the mask; the second leaves out h1, so it is not.
///
/// Measured on ansible-core 2.19.12. First play: `skipping: [h2]`, `changed: [h1]` alone under
/// the `run_once` banner, both h1 and h3 reading `once on ...` back, all three running `after`,
/// recap `h1 ok=4 changed=1`, `h2 ok=1 skipped=1`, `h3 ok=3`. Second play: `skipping: [h1]`,
/// **`changed: [h2]`** - the first host the mask holds, not the first live host - h2 and h3
/// reading `twice on ...`, recap `h1 ok=1 skipped=1`, `h2 ok=4 changed=1`, `h3 ok=3`.
///
/// What would make this red, in the first play: a host outside the mask reporting the step
/// instead of waiting for the verdict, which releases h3 before h1 has run anything, so h3
/// reads a register that does not exist yet - `fatal: [h3]` on an undefined variable. In the
/// second play: the runner elected from the whole live set, which hands the step to h1, a host
/// that runs nothing - so nobody runs it and both waiting hosts read the same undefined
/// register.
#[test]
fn a_run_once_runner_is_elected_inside_the_steps_mask() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("delegate/inv.ini"),
            &fixture("delegate/run-once-masked.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("skipping: [h2]"), "{text}");
    assert!(text.contains("skipping: [h1]"), "{text}");
    assert!(text.contains(r#""msg": "once on h1""#), "{text}");
    assert!(text.contains(r#""msg": "once on h3""#), "{text}");
    assert!(!text.contains("once on h2"), "{text}");
    assert!(text.contains(r#""msg": "twice on h2""#), "{text}");
    assert!(text.contains(r#""msg": "twice on h3""#), "{text}");
    assert!(!text.contains("twice on h1"), "{text}");
    for host in ["h1", "h2", "h3"] {
        assert!(
            text.contains(&format!(r#""msg": "after on {host}""#)),
            "{text}"
        );
        assert!(
            text.contains(&format!(r#""msg": "after again on {host}""#)),
            "{text}"
        );
    }
    assert!(
        text.contains("h1                         : ok=5    changed=1"),
        "{text}"
    );
    assert!(
        text.contains("h2                         : ok=5    changed=1"),
        "{text}"
    );
    assert!(
        text.contains("h3                         : ok=6    changed=0"),
        "{text}"
    );
}

/// A delegate nothing can reach takes the **delegating** host out of the run, and the line
/// carries the arrow.
///
/// Measured on ansible-core 2.19.12 against an unreachable delegate:
/// `fatal: [h2 -> h1]: UNREACHABLE!`, the delegating host counted `unreachable=1`, the delegate
/// absent from the recap, and exit 4.
///
/// What would make this red: the arrow missing, which hides which host could not be reached; or
/// the delegate counted in the recap, which invents a host line for work it never accepted.
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
    assert!(text.contains("carrier_pigeon"), "{text}");
    assert!(
        text.contains("h1                         : ok=0    changed=0    unreachable=1"),
        "{text}"
    );
    assert!(!text.contains("gone                 "), "{text}");
    assert!(!text.contains(r#""msg": "after""#), "{text}");
}

/// A `delegate_to` naming a host the inventory does not have is an implicit host, and
/// `localhost` is the local one.
///
/// Measured on ansible-core 2.19.12 with no `localhost` in the inventory:
/// `changed: [h1 -> localhost]`, the command's output readable back on h1, and
/// `delegate_to: 127.0.0.1` behaving the same way. A delegated task a `when` left out prints
/// `skipping: [h1]` with **no** arrow, which is the one line the delegate does not reach.
///
/// What would make this red: an unknown delegate refused at load, which refuses a playbook the
/// reference runs; or the arrow printed on a `skipping:` line, which claims a host was
/// contacted for a task that never left the controller.
#[test]
fn an_implicit_localhost_is_a_valid_delegate_and_a_skip_shows_no_arrow() {
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("delegate/inv.ini"),
            &fixture("delegate/delegate-localhost.yml"),
        ],
        PROBE_DEADLINE,
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("changed: [h1 -> localhost]"), "{text}");
    assert!(text.contains(r#""msg": "hi""#), "{text}");
    assert!(
        text.contains("skipping: [h1]") && !text.contains("skipping: [h1 -> h3]"),
        "a skipped task never reached the delegate: {text}"
    );
}

/// A host that has failed is still a valid delegate.
///
/// Measured on ansible-core 2.19.12: h3 fails its own task, h1 then delegates to h3 and the
/// task runs (`changed: [h1 -> h3]`), because the delegating driver opens its own connection to
/// the delegate rather than sharing the one the delegate's own driver had.
///
/// What would make this red: the delegate looked for in the live host list, which would fail
/// h1's task for a host that answers perfectly well.
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

/// A value that arrived in a command's output is data: the engine prints it and never
/// evaluates it, so an expression a managed host put there cannot run on the controller.
///
/// The payload asks for a file to be created. The assertion is that file's absence, not the
/// text on the terminal: a run that printed the right thing while still having run the lookup
/// would pass a snapshot and fail this.
///
/// Both display forms are here because the reference gives them the same verdict on every
/// source it was measured against: `debug: var=` and `debug: msg=` agree, and it is where a
/// value came from that decides, not the shape that shows it.
///
/// What would make this red: `render_in` taking one more pass over a string built from a
/// registered variable, or `resolve_vars` rendering the fact itself, which is what this release
/// did before.
#[test]
fn a_result_that_looks_like_a_template_is_never_evaluated() {
    let (dir, marker) = trust_dir("untrusted", "h1 ansible_connection=local\n");
    let out = volant_within(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            "-e",
            &format!("payload={}", dir.join("payload.txt").display()),
            &fixture("trust/untrusted.yml"),
        ],
        std::time::Duration::from_secs(30),
    );
    assert!(
        !marker.exists(),
        "the lookup in the command output ran on the controller:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.matches("lookup('pipe'").count(),
        3,
        "the raw text should be shown as data by both display forms and after set_fact:\n{stdout}"
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The other direction, and the one that makes the fix a fix rather than the removal of a
/// feature: a variable whose value is itself an author-written template is still rendered
/// through, however many links the chain has.
///
/// What would make this red: `render_in` stopping after its first pass for every string, which
/// is the lazy way to close the hole above and would break `vars: { y: "{{ z }}" }`.
#[test]
fn a_chain_of_author_templates_still_renders() {
    let (dir, _) = trust_dir("chain", "h1 ansible_connection=local\n");
    let out = volant_within(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            &fixture("trust/trusted-chain.yml"),
        ],
        std::time::Duration::from_secs(30),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // `2`, not `"2"`: a template that is one expression keeps the expression's type, and the
    // reference prints the same integer for the same chain.
    assert!(stdout.contains("\"msg\": 2"), "{stdout}");
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A file read at run time is data too: `lookup('file', ...)` hands back the text and the text
/// is never rendered, which is what the reference does by tagging the string it returns.
///
/// The marker again, for the same reason: what matters is that the expression in the file did
/// not run, not that the terminal showed it.
///
/// What would make this red: the `file` lookup returning an ordinary string, which leaves
/// `render_in` free to take another pass over it.
#[test]
fn lookup_file_content_is_never_evaluated() {
    let (dir, marker) = trust_dir("lookup", "h1 ansible_connection=local\n");
    let out = volant_within(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            "-e",
            &format!("payload={}", dir.join("payload.txt").display()),
            &fixture("trust/lookup-file.yml"),
        ],
        std::time::Duration::from_secs(30),
    );
    assert!(
        !marker.exists(),
        "the lookup in the file's text ran on the controller:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("lookup('pipe'"),
        "the raw text should be shown as data:\n{stdout}"
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The same value read from another host through `hostvars` is data on that side of the wire
/// too: one host registers the payload, the other prints it, and the expression does not run.
///
/// What would make this red: the trust travelling with the host that renders rather than with
/// the host the value came from, which is what a per-host set alone would give.
#[test]
fn a_fact_read_through_hostvars_is_never_evaluated() {
    let (dir, marker) = trust_dir(
        "hostvars",
        "h1 ansible_connection=local\nh2 ansible_connection=local\n",
    );
    let out = volant_within(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            "-e",
            &format!("payload={}", dir.join("payload.txt").display()),
            &fixture("trust/hostvars.yml"),
        ],
        std::time::Duration::from_secs(30),
    );
    assert!(
        !marker.exists(),
        "the lookup in the command output ran on the controller:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("lookup('pipe'"),
        "the raw text should be shown as data:\n{stdout}"
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A task variable built from a registered result is data as well. The name the playbook wrote
/// never went through the fact store, so the only place its provenance can be learnt is the
/// resolution that built it, and that is a pass the task's own arguments are rendered after.
///
/// `vars:` on a task feeding a registered value into `debug` is an everyday idiom, and it is a
/// plain playbook: no include, no role, nothing exotic.
///
/// What would make this red: the set of names a resolution pass turned into data staying inside
/// `resolve_vars` instead of travelling out with the map it resolved.
#[test]
fn a_task_variable_built_from_a_result_is_never_evaluated() {
    let (dir, marker) = trust_dir("task-vars", "h1 ansible_connection=local\n");
    let out = volant_within(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            "-e",
            &format!("payload={}", dir.join("payload.txt").display()),
            &fixture("trust/task-vars.yml"),
        ],
        std::time::Duration::from_secs(30),
    );
    assert!(
        !marker.exists(),
        "the lookup a task variable carried ran on the controller:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("lookup('pipe'"),
        "the raw text should be shown as data:\n{stdout}"
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// What an include statement hands down is data when the value it was built from was. The
/// statement's `vars:` are rendered on the driver and travel to the included steps as text, so
/// the names among them that came from a host travel with them.
///
/// The included file need not even read the name: the merged map of every step the statement
/// brought in is resolved as a whole.
///
/// What would make this red: `include_params` arriving with no record of which of its names were
/// rendered from a managed host's output.
#[test]
fn an_include_parameter_built_from_a_result_is_never_evaluated() {
    let (dir, marker) = trust_dir("include-params", "h1 ansible_connection=local\n");
    let out = volant_within(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            "-e",
            &format!("payload={}", dir.join("payload.txt").display()),
            &fixture("trust/include-params.yml"),
        ],
        std::time::Duration::from_secs(30),
    );
    assert!(
        !marker.exists(),
        "the lookup an include parameter carried ran on the controller:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("lookup('pipe'"),
        "the raw text should be shown as data:\n{stdout}"
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The name a `debug: var:` shows is compiled as an expression, not rendered as a template, so
/// a value that arrived from a managed host must never reach it. Every other guard in this
/// release stops a *second* render; this site takes what a render already produced and hands it
/// back as source text, which is a different mechanism and needs a different stop.
///
/// Measured on ansible-core 2.19.12: both the plain form and the loop form fail the task with
/// `Task failed: Error while resolving `var` expression: Encountered untrusted template or
/// expression.`, so the refusal is the reference's answer and not this engine's invention.
///
/// The payload has no `{{ }}` around it, because an expression is what this site compiles. The
/// marker's absence is the assertion; the sentence on the terminal is only the shape of the
/// refusal.
///
/// What would make this red: rendering a task's arguments without keeping which of them read a
/// managed host, which is what `render_value` alone gives.
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
        2,
        "both the plain form and the loop form should be refused:\n{stdout}"
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The other direction of the same guard, and the one a too-wide rule breaks: a play with no
/// managed-host value anywhere in it still prints. The fact names another variable, and the
/// `debug: var:` that shows it compiles that name as an expression.
///
/// Measured on ansible-core 2.19.12: the reference prints `"greeting": "hello"` here, both from
/// a `set_fact` and from a play-level `vars:`. A rule that called every fact data would fail
/// this play, which is a user-visible regression and not a security property.
///
/// What would make this red: writing a `set_fact` value with `set_untrusted_fact` whatever its
/// render read, which is what this release did before.
#[test]
fn a_fact_the_playbook_wrote_can_still_name_a_variable() {
    let (dir, _) = trust_dir("author-fact-name", "h1 ansible_connection=local\n");
    let out = volant_within(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            &fixture("trust/author-fact-name.yml"),
        ],
        std::time::Duration::from_secs(30),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"greeting\": \"hello\""),
        "a play with no host value in it should print:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// And the half the previous test could be made to pass by giving up: a fact whose value came
/// out of a managed host still cannot name the variable a `debug: var:` compiles.
///
/// The set_fact reads a registered value, so its render is the one that has to carry the answer
/// forward; the marker's absence is the assertion.
///
/// What would make this red: writing a `set_fact` value with `set_fact` whatever its render
/// read, which is the lazy way to fix the test above.
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

/// The negative direction of the promotion: a map holding an author variable **and** a
/// registered value promotes only the second. Without it nothing proves the promotion does not
/// over-fire, because a map with no untrusted name in it cannot tell an over-wide set from a
/// right one.
///
/// The author variable names the loop variable, so it cannot resolve until the item is bound
/// and it is the *second* resolution that has to render it. That is the only shape where an
/// over-wide promotion shows: a name already rendered by the pass that promoted it keeps its
/// value whatever the set says.
///
/// What would make this red: promoting any name beyond the ones whose own render read a managed
/// host - the greeting would then stay `greet {{ item }}` instead of reaching `greet a`. The
/// other half of the same assertion is the marker: the registered value is still text.
#[test]
fn an_author_chain_beside_a_result_still_renders() {
    let (dir, marker) = trust_dir("mixed-chain", "h1 ansible_connection=local\n");
    let out = volant_within(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            "-e",
            &format!("payload={}", dir.join("payload.txt").display()),
            &fixture("trust/mixed-chain.yml"),
        ],
        std::time::Duration::from_secs(30),
    );
    assert!(
        !marker.exists(),
        "the lookup beside the author chain ran on the controller:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(r#""msg": "greet a and {{ lookup('pipe'"#),
        "the author variable should reach the item while the result stays text:\n{stdout}"
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The other two lookups that read at run time. `lookup('file', ...)` was already data; a
/// command's output and an environment variable are the same thing arriving by another door, and
/// an engine that renders them again runs whatever they hold.
///
/// Measured on ansible-core 2.19.12: `pipe`, `env` and `file` all hand back a string the engine
/// refuses to template a second time, each showing the `{{ 1 + 1 }}` its source held.
///
/// What would make this red: tainting only the `file` arm of the lookup, which is where this
/// release started.
#[test]
fn what_a_lookup_read_at_run_time_is_never_evaluated() {
    let (dir, marker) = trust_dir("lookup-run-time", "h1 ansible_connection=local\n");
    let payload = std::fs::read_to_string(dir.join("payload.txt")).expect("the payload");
    let out = volant_within_env(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            "-e",
            &format!("payload={}", dir.join("payload.txt").display()),
            &fixture("trust/lookup-run-time.yml"),
        ],
        std::time::Duration::from_secs(30),
        &[("VOLANT_TRUST_PROBE", payload.trim_end())],
    );
    assert!(
        !marker.exists(),
        "the lookup in what was read at run time ran on the controller:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.matches("lookup('pipe'").count(),
        2,
        "both the command's output and the environment value should be shown as data:\n{stdout}"
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

/// The effect asserted directly, since an exit code alone cannot tell a refusal from a
/// connection that failed for some other reason: whatever reaches the fake `ssh`'s recorded
/// command line, none of it carries the `$(` of a command substitution.
///
/// What would make this red: `shell_word` leaving `~$(id)/x` bare and nothing checking a
/// host's own `ansible_remote_tmp`, so the substitution rides the probe command straight into
/// the recorded line.
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
    let _ = std::fs::remove_dir_all(&dir);
}

/// A connection variable is an ordinary variable: an extra var that says `local` beats the
/// inventory that says `ssh`, and nothing reaches the network.
///
/// The proof is a fake `ssh` first on `PATH` that writes a marker and exits 255. Its absence
/// after the run is the assertion; the exit code alone would not tell a local run from an ssh
/// that happened to work.
///
/// What would make this red: the transport reading the inventory object instead of the host's
/// effective variables, which is what this release did.
#[test]
fn an_extra_var_switches_the_connection_to_local() {
    let (dir, marker) = fake_ssh(
        "extra-var-local",
        "node ansible_connection=ssh ansible_host=192.0.2.1\n",
    );
    let out = volant_within_with_path(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            "-e",
            "ansible_connection=local",
            &fixture("connection/local-override.yml"),
        ],
        std::time::Duration::from_secs(30),
        Some(&dir.join("bin")),
        &[],
    );
    assert!(
        !marker.exists(),
        "ssh was called although the run asked for a local connection:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two tasks of one host under one target user, the second carrying its own connection
/// variables. The kept connection belongs to the first task's transport, so the second one has
/// to open its own rather than reuse it.
///
/// The effect asserted is the fake `ssh` having run at all: the inventory makes the host local,
/// so nothing but the second task's own `vars:` can put an `ssh` on the machine. An exit code
/// would not say it - a run that reused the local link finishes at 0 exactly like a run whose
/// ssh was never needed.
///
/// What would make this red: `LinkKey` keeping only the host name and the escalated user, which
/// would make the second task reuse the first link and reach the wrong machine.
#[test]
fn a_task_that_changes_the_connection_opens_its_own_link() {
    let (dir, marker) = fake_ssh("task-vars-switch", "node ansible_connection=local\n");
    let out = volant_within_with_path(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            &fixture("connection/task-vars-switch.yml"),
        ],
        std::time::Duration::from_secs(30),
        Some(&dir.join("bin")),
        &[],
    );
    assert!(
        marker.exists(),
        "the second task reused the first task's local connection instead of opening the ssh one it asked for:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("fatal: [node]: UNREACHABLE!"),
        "the ssh the second task asked for should have failed and said so:\n{stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The same host, the same two tasks, with `[volant] batching` on. Nothing here is a barrier -
/// neither task registers, loops or reads another host - so both land in one batch, and the
/// batch is what has to notice that its second task asked for a different machine.
///
/// What would make this red: the batch-break condition comparing only the escalated user and
/// the delegate. One link is then opened from the first task's view, the inventory's `local`,
/// and the second task runs on the controller although it asked for an ssh to 192.0.2.1 -
/// silently, exit 0.
#[test]
fn a_batch_splits_where_the_connection_changes() {
    let (dir, marker) = fake_ssh(
        "task-vars-switch-batched",
        "node ansible_connection=local\n",
    );
    let out = volant_within_with_path(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            &fixture("connection/task-vars-switch.yml"),
        ],
        std::time::Duration::from_secs(30),
        Some(&dir.join("bin")),
        &[("VOLANT_BATCHING", "1")],
    );
    assert!(
        marker.exists(),
        "the batch carried the second task over the first task's local connection:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("fatal: [node]: UNREACHABLE!"),
        "the ssh the second task asked for should have failed and said so:\n{stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A name the inventory never carried is an implicit `localhost` and runs on the controller. The
/// `local` that says so sits at the inventory host var layer, so a `group_vars/all` file beside
/// the playbook naming `ansible_connection` does not reach it.
///
/// Measured on ansible-core 2.19.12: the same play, the same group file, `changed: [localhost]`
/// and no ssh.
///
/// What would make this red: the rule sitting under the group and role layers, which lets a
/// repository-wide `group_vars/all` send every implicit `localhost` over ssh to an address it
/// never meant for the controller.
#[test]
fn a_group_file_does_not_move_the_implicit_localhost() {
    let (dir, marker) = fake_ssh("implicit-local-group", "node ansible_connection=local\n");
    let out = volant_within_with_path(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            &fixture("connection/implicit-local/play.yml"),
        ],
        std::time::Duration::from_secs(30),
        Some(&dir.join("bin")),
        &[],
    );
    assert!(
        !marker.exists(),
        "a group file sent the implicit localhost over ssh:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The other side of the same boundary, and the one the reference settles rather than intuition:
/// a `host_vars/localhost` file beside the playbook is *above* the inventory host var layer, so
/// it does move the implicit `localhost`.
///
/// Measured on ansible-core 2.19.12 with the same file: `fatal: [localhost]: UNREACHABLE!`,
/// exit 4.
///
/// What would make this red: the rule sitting above `host_vars/<host>` files, which would pin
/// every implicit `localhost` to the controller and swallow the file.
#[test]
fn a_host_file_does_move_the_implicit_localhost() {
    let (dir, marker) = fake_ssh(
        "implicit-local-host-file",
        "node ansible_connection=local\n",
    );
    let out = volant_within_with_path(
        &[
            "playbook",
            "-i",
            dir.join("inv.ini").to_str().expect("a path"),
            &fixture("connection/host-file-wins/play.yml"),
        ],
        std::time::Duration::from_secs(30),
        Some(&dir.join("bin")),
        &[],
    );
    assert!(
        marker.exists(),
        "the host file was swallowed by the implicit localhost rule:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `creates` is resolved where the command would run, not where the agent happens to be. The
/// directory already holds `marker`, so nothing runs and `should-not-run` never appears.
///
/// The assertion is that file's absence. A result that says `skipped` while the command ran
/// would pass a snapshot and fail this; that is the shape this guard has to have, because what
/// it protects is a migration or an initialisation running twice.
///
/// The exact skip text (`Did not run command since 'marker' exists`) is pinned at the module
/// level instead, in `command.rs`'s own tests: the CLI never shows a skipped task's message body
/// at any verbosity (`render.rs`, `Outcome::Skipped` carries no tail), which is a pre-existing
/// divergence from the reference in result classification, unrelated to this guard.
///
/// What would make this red: the guard calling `Path::exists` on the bare relative path, which
/// resolves against the agent's own directory and finds nothing.
#[test]
fn a_relative_creates_is_resolved_against_chdir() {
    let dir = std::env::temp_dir().join(format!("volant-creates-chdir-{}", std::process::id()));
    let work = dir.join("work");
    std::fs::create_dir_all(&work).expect("a work dir");
    std::fs::write(work.join("marker"), "").expect("the marker");
    let inventory = dir.join("inv.ini");
    std::fs::write(&inventory, "h1 ansible_connection=local\n").expect("an inventory");

    let out = volant_within(
        &[
            "playbook",
            "-i",
            inventory.to_str().expect("a path"),
            "-e",
            &format!("workdir={}", work.display()),
            &fixture("guards/creates-chdir.yml"),
        ],
        std::time::Duration::from_secs(30),
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !work.join("should-not-run").exists(),
        "the command ran although its guard said it should not:\n{stdout}"
    );
    assert_eq!(out.status.code(), Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The other half of the same fixture: `creates` is a glob, and the pattern is expanded in the
/// directory the `chdir` names, not in the agent's own directory. `x-1` sits in `work`, matching
/// `x-*`, so this guard must stop `should-not-run-2` from ever running too. Measurement (m)
/// settles the message the module itself produces: it quotes the pattern as written (`x-*`),
/// never the file that matched — pinned in `command.rs`'s own tests, for the reason given above.
///
/// What would make this red: a guard that falls back to a literal `Path::exists` for any pattern
/// it does not resolve, which never matches `x-*` against `x-1` and lets the command run.
#[test]
fn a_pattern_creates_is_resolved_against_chdir() {
    let dir = std::env::temp_dir().join(format!("volant-creates-glob-{}", std::process::id()));
    let work = dir.join("work");
    std::fs::create_dir_all(&work).expect("a work dir");
    std::fs::write(work.join("x-1"), "").expect("the match");
    let inventory = dir.join("inv.ini");
    std::fs::write(&inventory, "h1 ansible_connection=local\n").expect("an inventory");

    let out = volant_within(
        &[
            "playbook",
            "-i",
            inventory.to_str().expect("a path"),
            "-e",
            &format!("workdir={}", work.display()),
            &fixture("guards/creates-chdir.yml"),
        ],
        std::time::Duration::from_secs(30),
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !work.join("should-not-run-2").exists(),
        "the pattern guard let the command run although 'x-1' matches it:\n{stdout}"
    );
    assert_eq!(out.status.code(), Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `no_log` covers every event a task produces, not only its result. A diagnostic that goes
/// straight to stderr is still that task speaking, and the policy has to reach it.
///
/// Both streams are grepped, because the warning this triggers is written to stderr while the
/// censored result is written to stdout: a test reading one of them would pass while the value
/// is on the other. `CANARY-7f3a9c` is a fixture string, not a credential.
///
/// The warning itself is **not** blanked: measured on ansible-core 2.19.12, a task whose
/// `environment` does not render to a mapping warns with the raw source of the whole stack of
/// layers, `['{{ secret }}']`, brackets included, and that text holds nothing the playbook did
/// not already say out loud.
///
/// What would make this red: `prepare` printing the rendered value, which is what it did -- the
/// censorship policy could not reach a write it never saw.
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

/// A timeout covers the task, not the first process of it. A descendant holding stdout open keeps
/// the task running, so the deadline has to reach the readers too.
///
/// The elapsed time is the property under test, so it is asserted -- the one place in this suite
/// where it is. `volant_within` stays around it as the net against a hang: without it a regression
/// that never returns would sit until the harness's own slow-test timeout.
///
/// What would make this red: waiting for the reader threads outside the deadline, which is what
/// the wait did -- the task ended when the descendant did, and the run reported success.
#[test]
fn a_timeout_covers_a_descendant_holding_the_pipes() {
    let started = std::time::Instant::now();
    let out = volant_within(
        &[
            "playbook",
            "-i",
            &fixture("inventory.ini"),
            &fixture("timeout/descendant.yml"),
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
        elapsed < std::time::Duration::from_millis(2000),
        "the run took {elapsed:?}: the deadline did not reach the readers"
    );
}

/// A module argument the reference has and this release does not act on is refused by its own
/// name, before anything connects. A table saying a module runs says nothing about the rest of
/// that module's API, and an argument accepted and then dropped reports success having done
/// something other than what the playbook asked for.
///
/// The assertion is that the refusal comes before the play header: a refusal after `PLAY [` means
/// a host was already reached, and possibly changed.
///
/// What would make this red: the pre-flight not consulting the module's argument list, which lets
/// the task through to an agent that drops the argument and reports the task green.
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

/// The same refusal, for a value the pre-flight could not read. A template is rendered per host,
/// long after the pre-flight is over, so `check_arguments` leaves it alone rather than refusing on
/// a guess -- and the agent, which has the value, refuses it there instead of dropping it.
///
/// Without this the task prints `$HOME`, reports `changed` and exits 0, where the reference prints
/// the home directory: the accepted-then-silently-ignored divergence the argument registry exists
/// to close, surviving inside the registry's own subject matter. The run reaches the host first,
/// so the refusal costs a connection and exits 2 rather than 4.
///
/// What would make this red: the agent reading the registry for names alone, which lets the
/// rendered value through unread; or the refusal borrowing the unknown-argument sentence, which
/// tells an operator waiting on this release that they have a typo.
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

/// The same argument set to `false` asks for exactly what this release does - it expands nothing -
/// so the task runs. A refusal that reads the name and not the value stops a playbook that was
/// byte-identical under both engines, while the value that really diverges is the default, which
/// nobody writes and nothing can catch.
///
/// The assertion is the program's own output, not the exit code: `/bin/echo $HOME` succeeds
/// either way, and only the five characters it printed say which engine expanded anything.
///
/// What would make this red: the refusal firing on the name, which exits 4 before connecting.
#[test]
fn asking_for_the_expansion_this_release_does_not_do_runs() {
    let out = volant(&["playbook", &fixture("module-argument-expansion-off.yml")]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}{stdout}");
    assert!(
        stdout.contains(r#""msg": "printed=$HOME""#),
        "the argument the task asked for is not what the program was given:\n{stdout}"
    );
}

/// An argument *neither* engine has is a different mistake, and gets the reference's own answer
/// for it: the module fails at run time with the reference's sentence and the reference's code,
/// rather than this engine's pre-flight refusal. Measured on ansible-core 2.19.12, through
/// `args:` -- the ad-hoc path validates nothing, because `free_form` swallows the whole line.
///
/// What would make this red: the two paths collapsed into one, which gives the same code and the
/// same words to a playbook waiting on this release and a playbook with a typo in it.
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

/// `executable` selects the shell, which is the point of asking for it.
///
/// The assertion is what the shell printed, not the exit code: the task succeeded in both engines
/// before this, and only the output told them apart.
///
/// What would make this red: the agent building `sh -c` whatever the task asked for, which is
/// what this release did -- `$BASH_VERSION` is then empty and the task still reports success.
#[test]
fn shell_executable_selects_the_interpreter() {
    assert!(
        Path::new("/bin/bash").exists(),
        "this test needs /bin/bash to tell one shell from another; both CI platforms have it"
    );
    let out = volant(&["playbook", &fixture("shell-executable.yml")]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let version = stdout
        .split_once(r#""msg": "version="#)
        .and_then(|(_, rest)| rest.split('"').next())
        .unwrap_or_default();
    assert!(
        version.contains('.'),
        "the shell reported no version, so it was not bash:\n{stdout}"
    );
}
