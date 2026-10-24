// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant adhoc` and the `ansible` program name, run on local hosts. The outputs are
//! ansible-core 2.19.12's, measured with the same inventory; the scenario goldens hold the rest.
#![cfg(unix)]
use rstest::rstest;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[path = "common/process.rs"]
mod process;

#[path = "common/python.rs"]
mod python;

const DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/adhoc")
}

/// `program args...` in the fixture directory, with no configuration file, no result format and
/// the machine's Python unless `envs` names one.
fn run(program: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(fixtures())
        .stdin(std::process::Stdio::null())
        .env("NO_COLOR", "1")
        .env_remove("COLUMNS")
        .env_remove("ANSIBLE_CALLBACK_RESULT_FORMAT")
        .env_remove("VOLANT_PYTHON")
        .env("VOLANT_RUNTIME", "system")
        .env(
            "ANSIBLE_CONFIG",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/reference/ansible.cfg"),
        );
    for (name, value) in envs {
        command.env(name, value);
    }
    process::run_within(&mut command, DEADLINE, None)
}

fn adhoc(args: &[&str], envs: &[(&str, &str)]) -> Output {
    let argv: Vec<&str> = ["adhoc"].into_iter().chain(args.iter().copied()).collect();
    run(Path::new(env!("CARGO_BIN_EXE_volant")), &argv, envs)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

const FALSE_BLOCK: &str = "[ERROR]: Task failed: Module failed: non-zero return code
Origin: <adhoc 'command' task>

{'action': 'command', 'args': {'_raw_params': 'false'}, 'timeout': 0, 'async_val': 0, 'poll': 15}

";

#[rstest]
#[case::command_form(
    &["web", "-i", "inv.ini", "-a", "echo hi", "-f", "1"],
    &[],
    0,
    "h1 | CHANGED | rc=0 >>\nhi\nh2 | CHANGED | rc=0 >>\nhi\n"
)]
#[case::extra_vars_template_the_args(
    &["h1", "-i", "inv.ini", "-m", "debug", "-a", "msg={{ foo }}", "-e", "foo=bar"],
    &[],
    0,
    "h1 | SUCCESS => {\n    \"msg\": \"bar\"\n}\n"
)]
#[case::json_args(
    &["h1", "-i", "inv.ini", "-m", "debug", "-a", r#"{"msg": "j"}"#],
    &[],
    0,
    "h1 | SUCCESS => {\n    \"msg\": \"j\"\n}\n"
)]
#[case::failed_command(
    &["h1", "-i", "inv.ini", "-m", "command", "-a", "false"],
    &[],
    2,
    "[ERROR]: Task failed: Module failed: non-zero return code
Origin: <adhoc 'command' task>

{'action': 'command', 'args': {'_raw_params': 'false'}, 'timeout': 0, 'async_val': 0, 'poll': 15}

h1 | FAILED | rc=1 >>
non-zero return code
"
)]
#[case::one_block_for_hosts_failing_alike(
    &["web", "-i", "inv.ini", "-a", "false", "-f", "1"],
    &[],
    2,
    "BLOCKh1 | FAILED | rc=1 >>\nnon-zero return code\nh2 | FAILED | rc=1 >>\nnon-zero return code\n"
)]
#[case::failed_json_form(
    &["h1", "-i", "inv.ini", "-m", "fail", "-a", "msg=boom"],
    &[],
    2,
    "[ERROR]: Task failed: Action failed: boom
Origin: <adhoc 'fail' task>

{'action': 'fail', 'args': {'msg': 'boom'}, 'timeout': 0, 'async_val': 0, 'poll': 15}

h1 | FAILED! => {
    \"changed\": false,
    \"msg\": \"boom\"
}
"
)]
#[case::yaml_result_format(
    &["h1", "-i", "inv.ini", "-m", "debug", "-a", r#"{"msg": "a\nb"}"#],
    &[("ANSIBLE_CALLBACK_RESULT_FORMAT", "yaml")],
    0,
    "h1 | SUCCESS => \n    msg: |-\n        a\n        b\n"
)]
#[case::json_result_format(
    &["h1", "-i", "inv.ini", "-m", "debug", "-a", "msg=x"],
    &[("ANSIBLE_CALLBACK_RESULT_FORMAT", "json")],
    0,
    "h1 | SUCCESS => {\n    \"msg\": \"x\"\n}\n"
)]
fn an_adhoc_run_prints_the_minimal_callback(
    #[case] args: &[&str],
    #[case] envs: &[(&str, &str)],
    #[case] code: i32,
    #[case] stdout: &str,
) {
    let out = adhoc(args, envs);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(code), "{args:?}\n{stderr}");
    assert_eq!(
        text(&out.stdout),
        stdout.replace("BLOCK", FALSE_BLOCK),
        "{args:?}"
    );
    assert_eq!(stderr, "", "{args:?}");
}

/// An unreachable host exits 4, and so does a run where another host failed.
#[rstest]
#[case::unreachable(&["all", "-i", "bad.ini", "-a", "true"])]
#[case::failed_and_unreachable(&["all", "-i", "inv.ini", "-i", "bad.ini", "-a", "false", "-f", "1"])]
fn an_unreachable_host_exits_4(#[case] args: &[&str]) {
    let out = adhoc(args, &[]);
    let stdout = text(&out.stdout);
    assert_eq!(out.status.code(), Some(4), "{stdout}{}", text(&out.stderr));
    assert!(stdout.contains("u1 | UNREACHABLE! => {\n    \"changed\": false,\n    \"msg\": \"Task failed: Failed to connect to the host via ssh: "), "{stdout}");
    assert!(
        stdout.contains("Origin: <adhoc 'command' task>\n"),
        "{stdout}"
    );
}

/// Refused before a host is touched, on stderr, with the code the reference exits with.
#[rstest]
#[case::unknown_result_format(
    &["h1", "-i", "inv.ini", "-a", "true"],
    &[("ANSIBLE_CALLBACK_RESULT_FORMAT", "xml")],
    5,
    "[ERROR]: Invalid value 'xml' for config 'result_format' for 'minimal' callback plugin. Valid values are: json, yaml\n"
)]
#[case::empty_result_format(
    &["h1", "-i", "inv.ini", "-a", "true"],
    &[("ANSIBLE_CALLBACK_RESULT_FORMAT", "")],
    5,
    "[ERROR]: Invalid value '' for config 'result_format' for 'minimal' callback plugin. Valid values are: json, yaml\n"
)]
#[case::unbalanced_quote(
    &["h1", "-i", "inv.ini", "-m", "debug", "-a", "x=\"a"],
    &[],
    4,
    "[ERROR]: failed at splitting arguments, either an unbalanced jinja2 block or quotes: x=\"a\n"
)]
fn a_refused_adhoc_run_prints_nothing_on_stdout(
    #[case] args: &[&str],
    #[case] envs: &[(&str, &str)],
    #[case] code: i32,
    #[case] stderr: &str,
) {
    let out = adhoc(args, envs);
    assert_eq!(out.status.code(), Some(code), "{args:?}");
    assert_eq!(text(&out.stderr), stderr, "{args:?}");
    assert_eq!(text(&out.stdout), "", "{args:?}");
}

#[test]
fn json_that_does_not_parse_exits_250() {
    let out = adhoc(&["h1", "-i", "inv.ini", "-m", "debug", "-a", "{bad}"], &[]);
    assert_eq!(out.status.code(), Some(250));
    assert!(
        text(&out.stderr).starts_with("[ERROR]: the module arguments are not valid JSON: "),
        "{}",
        text(&out.stderr)
    );
}

/// Started as `ansible`, through a link, the binary is `volant adhoc`.
#[test]
fn the_ansible_name_runs_adhoc() {
    let dir = std::env::temp_dir().join(format!("volant-adhoc-name-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let link = dir.join("ansible");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_volant"), &link).unwrap();
    let args = ["h1", "-i", "inv.ini", "-m", "debug", "-a", "msg=x"];
    let named = run(&link, &args, &[]);
    let _ = std::fs::remove_dir_all(&dir);
    let sub = adhoc(&args, &[]);
    assert_eq!(named.status.code(), Some(0), "{}", text(&named.stderr));
    assert_eq!(
        text(&named.stdout),
        "h1 | SUCCESS => {\n    \"msg\": \"x\"\n}\n"
    );
    assert_eq!(named.stdout, sub.stdout);
}

/// The two runs that need the controller's ansible-core: a Python module, and a name nothing
/// answers to, which fails every host instead of refusing the run.
#[rstest]
#[case::python_module(
    &["web", "-i", "inv.ini", "-m", "ping", "-f", "1"],
    0,
    "h1 | SUCCESS => {\n    \"changed\": false,\n    \"ping\": \"pong\"\n}\nh2 | SUCCESS => {\n    \"changed\": false,\n    \"ping\": \"pong\"\n}\n"
)]
#[case::unknown_module(
    &["web", "-i", "inv.ini", "-m", "nosuchmodule", "-f", "1"],
    2,
    "[ERROR]: Task failed: Cannot resolve 'nosuchmodule' to an action or module.

Task failed.
Origin: <adhoc 'nosuchmodule' task>

{'action': 'nosuchmodule', 'args': {}, 'timeout': 0, 'async_val': 0, 'poll': 15}

<<< caused by >>>

Cannot resolve 'nosuchmodule' to an action or module.
Origin: <CLI option '-m'>

nosuchmodule

h1 | FAILED! => {
    \"changed\": false,
    \"msg\": \"Task failed: Cannot resolve 'nosuchmodule' to an action or module.\"
}
h2 | FAILED! => {
    \"changed\": false,
    \"msg\": \"Task failed: Cannot resolve 'nosuchmodule' to an action or module.\"
}
"
)]
fn a_run_through_the_controller_python(
    #[case] args: &[&str],
    #[case] code: i32,
    #[case] stdout: &str,
) {
    let Some(python) = python::reference_python() else {
        return;
    };
    let python = python.display().to_string();
    let out = adhoc(args, &[("VOLANT_PYTHON", &python)]);
    assert_eq!(out.status.code(), Some(code), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), stdout);
}

const NOT_MATCHED: &str = "[WARNING]: Could not match supplied host pattern, ignoring: nomatch\n";
const NO_HOSTS: &str = "[ERROR]: Specified inventory, host pattern and/or --limit leaves us with no hosts to target.\n";
const UNPARSED: &str = "[WARNING]: Unable to parse <dir>/nosuch.ini as an inventory source\n[WARNING]: No inventory was parsed, only implicit localhost is available\n[WARNING]: provided hosts list is empty, only localhost is available. Note that the implicit localhost does not match 'all'\n";

/// Listed hosts and the empty sets, with the reference's streams and codes: no match exits 0, a
/// `-l` that leaves nothing exits 1, a command module with no `-a` exits 5.
#[rstest]
#[case::listed(&["web", "-i", "inv.ini", "--list-hosts"], 0, "  hosts (2):\n    h1\n    h2\n", "")]
#[case::listed_and_limited(&["all", "-i", "inv.ini", "--list-hosts", "-l", "h2"], 0, "  hosts (1):\n    h2\n", "")]
#[case::union(&["web:db", "-i", "inv.ini", "--list-hosts"], 0, "  hosts (3):\n    h1\n    h2\n    h3\n", "")]
#[case::no_match(&["nomatch", "-i", "inv.ini", "-a", "true"], 0, "", &format!("{NOT_MATCHED}[WARNING]: No hosts matched, nothing to do\n"))]
#[case::no_match_listed(&["nomatch", "-i", "inv.ini", "--list-hosts"], 0, "  hosts (0):\n", &format!("{NOT_MATCHED}[WARNING]: No hosts matched, nothing to do\n"))]
#[case::limit_leaves_nothing(&["all", "-i", "inv.ini", "-a", "true", "-l", "nomatch"], 1, "", &format!("{NOT_MATCHED}{NO_HOSTS}"))]
#[case::limit_leaves_nothing_listed(&["web", "-i", "inv.ini", "--list-hosts", "-l", "h9"], 1, "", &format!("{}{NO_HOSTS}", NOT_MATCHED.replace("nomatch", "h9")))]
#[case::unreadable_inventory(&["all", "-i", "nosuch.ini", "-a", "true"], 0, "", UNPARSED)]
#[case::no_arguments(&["h1", "-i", "inv.ini"], 5, "", "[ERROR]: No argument passed to command module\n")]
#[case::no_arguments_shell(&["h1", "-i", "inv.ini", "-m", "shell"], 5, "", "[ERROR]: No argument passed to shell module\n")]
fn the_hosts_of_a_pattern_are_listed_or_reported_empty(
    #[case] args: &[&str],
    #[case] code: i32,
    #[case] stdout: &str,
    #[case] stderr: &str,
) {
    let out = adhoc(args, &[]);
    let dir = fixtures().canonicalize().unwrap().display().to_string();
    assert_eq!(out.status.code(), Some(code), "{args:?}");
    assert_eq!(text(&out.stdout), stdout, "{args:?}");
    assert_eq!(text(&out.stderr).replace(&dir, "<dir>"), stderr, "{args:?}");
}

/// The usage block of the reference, then the error line, exit 2: and the complete help after the
/// errors argparse raises, not after a forks count below 1.
#[rstest]
#[case::no_pattern(&[], &[], "the following arguments are required: pattern", true)]
#[case::forks_not_a_number(&["-f", "x", "h1"], &[], "argument -f/--forks: invalid tag_value value: 'x'", true)]
#[case::background_not_a_number(&["-B", "x", "h1"], &[], "argument -B/--background: invalid tag_value value: 'x'", true)]
#[case::poll_not_a_number(&["-P", "x", "h1"], &[], "argument -P/--poll: invalid tag_value value: 'x'", true)]
#[case::timeout_not_a_number(&["--task-timeout", "1.5", "h1"], &[], "argument --task-timeout: invalid tag_value value: '1.5'", true)]
#[case::forks_zero(&["-f", "0", "h1"], &[], "The number of processes (--forks) must be >= 1", false)]
#[case::forks_negative(&["-f", "-1", "h1"], &[], "The number of processes (--forks) must be >= 1", false)]
#[case::forks_from_the_environment(&["h1", "-i", "inv.ini"], &[("ANSIBLE_FORKS", "0")], "The number of processes (--forks) must be >= 1", false)]
fn a_usage_error_prints_the_usage_block_and_exits_2(
    #[case] args: &[&str],
    #[case] envs: &[(&str, &str)],
    #[case] error: &str,
    #[case] help: bool,
) {
    let help_text = include_str!("../src/cli/adhoc_help.txt");
    let usage = &help_text[..help_text.find("\n\n").unwrap() + 1];
    let out = adhoc(args, envs);
    let expected = if help {
        format!("{usage}ansible: error: {error}\n \n{help_text}")
    } else {
        format!("{usage}ansible: error: {error}\n")
    };
    assert_eq!(out.status.code(), Some(2), "{args:?}");
    assert_eq!(text(&out.stderr), expected, "{args:?}");
    assert_eq!(text(&out.stdout), "", "{args:?}");
}

/// A negative count, and `-C` with a job, are refused by name before a host is touched.
#[rstest]
#[case::background(&["h1", "-i", "inv.ini", "-a", "true", "-B", "-1"], "ERROR! a negative -B is not supported yet for ad hoc commands\n")]
#[case::poll(&["h1", "-i", "inv.ini", "-a", "true", "-P", "-1"], "ERROR! a negative -P is not supported yet for ad hoc commands\n")]
#[case::timeout(&["h1", "-i", "inv.ini", "-a", "true", "--task-timeout", "-1"], "ERROR! a negative --task-timeout is not supported yet for ad hoc commands\n")]
#[case::check_and_background(&["h1", "-i", "inv.ini", "-a", "true", "-C", "-B", "5"], "ERROR! -C with -B is not supported yet for ad hoc commands\n")]
fn a_run_option_this_release_cannot_honour_is_refused(#[case] args: &[&str], #[case] stderr: &str) {
    let out = adhoc(args, &[]);
    assert_eq!(out.status.code(), Some(4), "{args:?}");
    assert_eq!(text(&out.stderr), stderr, "{args:?}");
    assert_eq!(text(&out.stdout), "", "{args:?}");
}

/// `-C`, `--task-timeout` and `-B`, on modules that need no ansible-core.
#[test]
fn check_timeout_and_background_run_without_ansible_core() {
    let skipped = adhoc(&["h1", "-i", "inv.ini", "-a", "true", "-C", "-D"], &[]);
    assert_eq!(text(&skipped.stdout), "h1 | SKIPPED\n");

    // The command touches a marker after its sleep: a task that timed out must be gone by then.
    let marker = std::env::temp_dir().join(format!("volant-adhoc-marker-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let command = format!("sleep 2; touch {}", marker.display());
    let slow = adhoc(
        &[
            "h1",
            "-i",
            "inv.ini",
            "-m",
            "shell",
            "-a",
            &command,
            "--task-timeout",
            "1",
        ],
        &[],
    );
    assert_eq!(slow.status.code(), Some(2), "{}", text(&slow.stderr));
    let shown = text(&slow.stdout);
    let head = "[ERROR]: Task failed: Timed out after 1 second(s).\nOrigin: <adhoc 'shell' task>\n";
    assert!(shown.starts_with(head), "{shown}");
    assert!(shown.contains("h1 | FAILED | rc=-1 >>\n"), "{shown}");
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert!(!marker.exists(), "the timed-out command kept running");

    // The job's status file goes under the home, so the run gets one of its own.
    let home = std::env::temp_dir().join(format!("volant-adhoc-home-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let home = home.display().to_string();
    let started = adhoc(
        &[
            "h1", "-i", "inv.ini", "-m", "shell", "-a", "true", "-B", "5", "-P", "0",
        ],
        &[("HOME", &home)],
    );
    let polled = adhoc(
        &[
            "h1", "-i", "inv.ini", "-m", "shell", "-a", "echo hi", "-B", "5", "-P", "1",
        ],
        &[("HOME", &home)],
    );
    let _ = std::fs::remove_dir_all(&home);
    let (started, polled) = (text(&started.stdout), text(&polled.stdout));
    assert!(
        started.starts_with("h1 | CHANGED => {\n    \"ansible_job_id\": \"j"),
        "{started}"
    );
    assert!(started.contains("\"finished\": false,\n"), "{started}");
    assert!(
        polled.contains("\"finished\": true,\n") && polled.contains("\"stdout\": \"hi\""),
        "{polled}"
    );
}

/// `--version` and `-vv` print the reference's block; with no interpreter the lines about the
/// Python side are left out and nothing is installed.
#[test]
fn the_version_block_is_printed_for_version_and_vv() {
    let envs = [("VOLANT_PYTHON", "/nonexistent/python")];
    let version = adhoc(&["--version"], &envs);
    assert_eq!(version.status.code(), Some(0), "{}", text(&version.stderr));
    let block = text(&version.stdout);
    let lines: Vec<&str> = block.lines().collect();
    assert_eq!(lines.len(), 5, "{block}");
    assert_eq!(lines[0], "ansible [core 2.19.12]");
    assert!(lines[1].starts_with("  config file = "), "{block}");
    assert!(
        lines[2].starts_with("  configured module search path = ['"),
        "{block}"
    );
    assert!(
        lines[3].starts_with("  ansible collection location = "),
        "{block}"
    );
    assert_eq!(
        lines[4],
        format!("  executable location = {}", env!("CARGO_BIN_EXE_volant"))
    );

    let verbose = adhoc(
        &["h1", "-i", "inv.ini", "-m", "debug", "-a", "msg=x", "-vv"],
        &envs,
    );
    let shown = text(&verbose.stdout);
    assert!(shown.starts_with(&block), "{shown}");
    assert!(
        shown.ends_with("Skipping callback 'default', as we already have a stdout callback.\nSkipping callback 'oneline', as we already have a stdout callback.\nh1 | SUCCESS => {\n    \"msg\": \"x\"\n}\n"),
        "{shown}"
    );
}

/// `--version` is acted on as it is read, before the checks that follow the parse.
#[test]
fn version_comes_before_the_forks_check() {
    let out = adhoc(
        &["--version", "-f", "0"],
        &[("VOLANT_PYTHON", "/nonexistent/python")],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).starts_with("ansible [core 2.19.12]\n"));
}
