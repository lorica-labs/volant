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
#[case::more_verbosity(&["h1", "-i", "inv.ini", "-vv", "-a", "true"], &[], 4, "ERROR! -vv and above are not supported yet for ad hoc commands\n")]
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
