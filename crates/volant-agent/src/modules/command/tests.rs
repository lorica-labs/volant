// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use crate::testing::{args, done, scratch};
use rstest::rstest;
use serde_json::json;

/// `stdin_add_newline` decides whether the value written to the child ends in a newline, and
/// its default is to add one.
#[test]
fn stdin_add_newline_decides_the_last_byte_written() {
    let bytes = |stdin: Value, add: Option<Value>| {
        let mut a = args(json!({"_raw_params": "wc -c", "stdin": stdin}));
        if let Some(add) = add {
            a.insert("stdin_add_newline".into(), add);
        }
        done(execute(&a, false, &Context::default(), &|| false)).0["stdout"]
            .as_str()
            .unwrap()
            .trim()
            .to_string()
    };
    assert_eq!(bytes(json!("ab"), None), "3");
    assert_eq!(bytes(json!("ab\n"), None), "4", "appended unconditionally");
    assert_eq!(bytes(json!("ab"), Some(json!(false))), "2");
    assert_eq!(bytes(json!("ab\n"), Some(json!(false))), "3");
    for off in [json!("false"), json!("no"), json!("Off"), json!(0)] {
        assert_eq!(bytes(json!("ab"), Some(off.clone())), "2", "{off}");
    }
    assert_eq!(bytes(json!("ab"), Some(json!("yes"))), "3");
}

/// `chdir`, `creates` and `removes` expand `$VAR` from the task's environment, for `shell` too.
#[test]
fn the_three_paths_expand_variables() {
    let dir = scratch();
    std::fs::write(dir.path().join("marker"), b"").unwrap();
    let ctx = Context {
        environment: [("S3D".to_string(), dir.path().to_str().unwrap().to_string())].into(),
        ..Context::default()
    };
    for shell in [false, true] {
        let params = json!({"_raw_params": "pwd", "_uses_shell": shell, "chdir": "$S3D", "creates": "$S3D/marker"});
        let r = done(execute(&args(params), false, &ctx, &|| false));
        let want = format!(
            "Did not run command since '{}/marker' exists",
            dir.path().display()
        );
        assert_eq!(r.0["msg"], want.as_str(), "{:?}", r.0);
    }
}

/// `creates` and `removes` resolve under `chdir`, expand globs per directory component, keep
/// the pattern as written in the message, and an empty `creates` is no guard.
#[rstest]
#[case::relative_creates(
    &["marker"],
    json!({"_raw_params": "echo never", "chdir": "$DIR", "creates": "marker"}),
    false,
    "marker"
)]
#[case::relative_removes(
    &["cache"],
    json!({"_raw_params": "echo ran", "chdir": "$DIR", "removes": "cache"}),
    true,
    "ran"
)]
#[case::empty_creates(
    &[],
    json!({"_raw_params": "echo ran", "chdir": "$DIR", "creates": ""}),
    true,
    "ran"
)]
#[case::glob_in_a_directory_component(
    &["sub/marker"],
    json!({"_raw_params": "echo never", "chdir": "$DIR", "creates": "s*/marker"}),
    false,
    "s*/marker"
)]
#[case::absolute_glob_ignores_chdir(
    &["x-1"],
    json!({"_raw_params": "echo never", "chdir": "$ELSE", "creates": "$DIR/x-*"}),
    false,
    "$DIR/x-*"
)]
fn a_guard_decides_whether_the_command_runs(
    #[case] fixtures: &[&str],
    #[case] params: Value,
    #[case] ran: bool,
    #[case] text: &str,
) {
    let dir_guard = scratch();
    let else_guard = scratch();
    let dir = dir_guard.path().to_str().unwrap();
    for file in fixtures {
        let path = dir_guard.path().join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"").unwrap();
    }
    let fill = |s: &str| {
        s.replace("$DIR", dir)
            .replace("$ELSE", else_guard.path().to_str().unwrap())
    };
    let params: Value = serde_json::from_str(&fill(&params.to_string())).unwrap();
    let r = done(execute(&args(params), false, &Context::default(), &|| {
        false
    }));
    let text = fill(text);
    if ran {
        assert!(r.changed(), "a guard skipped what it should not: {:?}", r.0);
        assert_eq!(r.0["stdout"], text.as_str());
    } else {
        assert!(!r.changed(), "{:?}", r.0);
        assert_eq!(
            r.0["msg"],
            format!("Did not run command since '{text}' exists")
        );
        assert_eq!(r.0["stdout"], format!("skipped, since {text} exists"));
    }
}

/// A command that cannot run fails the task in the reference's shape: its `rc` and message
/// where it has them, before any guard is read when the `chdir` is the problem.
#[rstest]
#[case::non_zero_rc(
    json!({"_raw_params": "false"}), Some(json!(1)), Some("non-zero return code")
)]
#[case::missing_program(
    json!({"_raw_params": "volant-no-such-program"}), Some(json!(2)),
    Some("Error executing command.")
)]
#[case::missing_chdir(
    json!({"_raw_params": "echo never", "chdir": "/definitely/not/here"}), Some(json!(null)),
    Some("Unable to change directory before execution.")
)]
#[case::missing_chdir_before_a_guard(
    json!({"_raw_params": "rm -rf cache", "chdir": "/definitely/not/here", "removes": "cache"}),
    Some(json!(null)), Some("Unable to change directory before execution.")
)]
#[case::chdir_that_is_a_file(
    json!({"_raw_params": "rm -rf cache", "chdir": "$FILE", "removes": "cache"}), None, None
)]
#[case::empty_command(json!({"_raw_params": "   "}), None, Some("no command given"))]
#[case::killed_by_a_signal(
    json!({"_raw_params": "sh -c 'kill -TERM $$'"}), Some(json!(-15)), None
)]
fn a_command_that_cannot_run_fails_in_the_reference_shape(
    #[case] params: Value,
    #[case] rc: Option<Value>,
    #[case] msg: Option<&str>,
) {
    let dir_guard = scratch();
    let file = dir_guard.path().join("config.yml");
    std::fs::write(&file, "").unwrap();
    let params: Value =
        serde_json::from_str(&params.to_string().replace("$FILE", file.to_str().unwrap())).unwrap();
    let r = done(execute(&args(params), false, &Context::default(), &|| {
        false
    }));
    assert!(r.failed(), "{:?}", r.0);
    if let Some(rc) = rc {
        assert_eq!(r.0["rc"], rc, "{:?}", r.0);
    }
    if let Some(msg) = msg {
        assert!(r.0["msg"].as_str().unwrap().starts_with(msg), "{:?}", r.0);
    }
}

/// A boolean argument the reference cannot convert fails the task with its sentence.
#[rstest]
#[case::string(json!("maybe"), "of type str and we were unable to convert to bool: The value 'maybe' is not a valid boolean.")]
#[case::int(json!(2), "of type int and we were unable to convert to bool: The value '2' is not a valid boolean.")]
#[case::list(json!(["a"]), "of type list and we were unable to convert to bool: <class 'list'> cannot be converted to a bool")]
fn an_unreadable_boolean_fails_the_task(#[case] value: Value, #[case] want: &str) {
    let r = done(execute(
        &args(json!({"_raw_params": "echo hi", "expand_argument_vars": value})),
        false,
        &Context::default(),
        &|| false,
    ));
    let msg = r.0["msg"].as_str().unwrap_or_default();
    assert!(
        r.failed() && msg.starts_with("argument 'expand_argument_vars' is ") && msg.contains(want),
        "{:?}",
        r.0
    );
}

/// A directory the agent may not search fails as an unusable `chdir`, not as a program that
/// would not start.
#[test]
fn a_chdir_without_search_permission_is_a_chdir_failure() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Root searches any directory, so there is nothing to see.
    if unsafe { libc::geteuid() } != 0 {
        let r = done(execute(
            &args(json!({"_raw_params": "echo hi", "chdir": locked.to_str().unwrap()})),
            false,
            &Context::default(),
            &|| false,
        ));
        assert_eq!(
            r.0["msg"], "Unable to change directory before execution.",
            "{:?}",
            r.0
        );
        assert_eq!(r.0["rc"], Value::Null);
    }
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// Each form runs the way the reference runs it: the shell form through `sh`, `argv`
/// and `cmd` directly, an `argv` on the shell path as the list, and a fed standard input.
#[rstest]
#[case::shell_form(json!({"_raw_params": "echo $((6 * 7))"}), true, "42", json!("echo $((6 * 7))"))]
#[case::argv(
    json!({"argv": ["printf", "%s-%s", "a", "b"]}), false, "a-b", json!(["printf", "%s-%s", "a", "b"])
)]
#[case::cmd(json!({"cmd": "echo cmd-form"}), false, "cmd-form", json!(["echo", "cmd-form"]))]
#[case::argv_on_the_shell_path(json!({"argv": ["echo", "hi"]}), true, "hi", json!(["echo", "hi"]))]
#[case::stdin_is_fed(
    json!({"_raw_params": "cat", "stdin": "from stdin"}), false, "from stdin", json!(["cat"])
)]
fn a_form_runs_the_way_the_reference_runs_it(
    #[case] params: Value,
    #[case] shell: bool,
    #[case] stdout: &str,
    #[case] cmd: Value,
) {
    let r = done(execute(&args(params), shell, &Context::default(), &|| {
        false
    }));
    assert_eq!(r.0["stdout"], stdout, "{:?}", r.0);
    assert_eq!(r.0["cmd"], cmd);
}

#[test]
fn free_form_command_reports_stdout_and_rc() {
    let r = done(execute(
        &args(json!({"_raw_params": "echo hello world"})),
        false,
        &Context::default(),
        &|| false,
    ));
    assert_eq!(r.0["rc"], 0);
    assert_eq!(r.0["stdout"], "hello world");
    assert_eq!(r.0["stdout_lines"], json!(["hello world"]));
    assert_eq!(r.0["stderr"], "");
    assert_eq!(r.0["cmd"], json!(["echo", "hello", "world"]));
    assert_eq!(r.0["msg"], "");
    assert!(r.changed() && !r.failed());
    assert!(r.0["start"].as_str().unwrap().len() == 26);
    assert!(r.0["delta"].as_str().unwrap().starts_with("0:00:0"));
}

/// A guard that skips reports `ok` with the reference's keys, key for key, and no `skipped`.
#[test]
fn a_skipped_guard_reports_the_reference_shape() {
    {
        let r = done(execute(
            &args(json!({"_raw_params": "true", "creates": "/"})),
            false,
            &Context::default(),
            &|| false,
        ));
        assert_eq!(
            Value::Object(r.0),
            json!({
                "changed": false, "cmd": ["true"], "delta": null, "end": null,
                "msg": "Did not run command since '/' exists", "rc": 0, "start": null,
                "stderr": "", "stderr_lines": [], "stdout": "skipped, since / exists",
                "stdout_lines": ["skipped, since / exists"],
            })
        );
    }
    {
        let r = done(execute(
            &args(json!({"_raw_params": "true", "removes": "/definitely/not/here"})),
            false,
            &Context::default(),
            &|| false,
        ));
        assert_eq!(
            Value::Object(r.0),
            json!({
                "changed": false, "cmd": ["true"], "delta": null, "end": null,
                "msg": "Did not run command since '/definitely/not/here' does not exist",
                "rc": 0, "start": null, "stderr": "", "stderr_lines": [],
                "stdout": "skipped, since /definitely/not/here does not exist",
                "stdout_lines": ["skipped, since /definitely/not/here does not exist"],
            })
        );
    }
}

/// Check mode runs nothing, and a guard says "Would" where it said "Did", as the reference does.
#[rstest::rstest]
#[case::no_guard(json!({"_raw_params": "touch /never"}), "Command would have run if not in check mode", false, true)]
#[case::creates_absent(json!({"_raw_params": "touch /never", "creates": "/definitely/not/here"}), "Command would have run if not in check mode", true, false)]
#[case::removes_present(json!({"_raw_params": "touch /never", "removes": "/"}), "Command would have run if not in check mode", true, false)]
#[case::creates_present(json!({"_raw_params": "touch /never", "creates": "/"}), "Would not run command since '/' exists", false, false)]
#[case::removes_absent(json!({"_raw_params": "touch /never", "removes": "/definitely/not/here"}), "Would not run command since '/definitely/not/here' does not exist", false, false)]
fn check_mode_runs_no_command(
    #[case] params: Value,
    #[case] msg: &str,
    #[case] changed: bool,
    #[case] skipped: bool,
) {
    let ctx = Context {
        check_mode: true,
        ..Context::default()
    };
    let r = done(execute(&args(params), false, &ctx, &|| false));
    assert_eq!(r.0["msg"], msg);
    assert_eq!(r.0["changed"], changed);
    assert_eq!(r.0.get("skipped").is_some(), skipped, "{:?}", r.0);
    assert_eq!(r.0["start"], Value::Null, "nothing ran");
}

/// 4 MiB of standard input does not deadlock the write, and `chdir` sets the working directory.
#[test]
fn a_large_stdin_does_not_deadlock_and_chdir_sets_the_directory() {
    {
        let payload: String = "abcdefghijklmnopqrstuvwxyz0123456789\n"
            .chars()
            .cycle()
            .take(4 * 1_048_576)
            .collect();
        let r = done(execute(
            &args(json!({"_raw_params": "cat", "stdin": payload})),
            true,
            &Context::default(),
            &|| false,
        ));
        assert_eq!(r.0["rc"], 0);
        assert_eq!(r.0["stdout"], payload);
    }
    {
        let dir_guard = scratch();
        let dir = dir_guard.path().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker"), b"").unwrap();
        let r = done(execute(
            &args(json!({"_raw_params": "ls", "chdir": dir.to_str().unwrap()})),
            false,
            &Context::default(),
            &|| false,
        ));
        assert_eq!(r.0["stdout"], "marker");
    }
}

#[test]
fn cancellation_kills_the_program() {
    let started = Instant::now();
    let run = execute(
        &args(json!({"_raw_params": "sleep 30"})),
        false,
        &Context::default(),
        &|| true,
    );
    assert!(matches!(run, Run::Cancelled));
    assert!(started.elapsed().as_secs() < 5);
}

#[test]
fn a_timeout_kills_the_program_and_reports_ansible_shape() {
    let started = Instant::now();
    let r = done(execute(
        &args(json!({"_raw_params": "sleep 30"})),
        false,
        &Context {
            timeout: Some(Duration::from_secs(1)),
            ..Context::default()
        },
        &|| false,
    ));
    assert!(started.elapsed().as_secs() < 5);
    assert!(r.failed());
    assert!(!r.changed());
    assert_eq!(r.0["msg"], "Task failed: Timed out after 1 second(s).");
    assert_eq!(r.0["timedout"]["period"], json!(1));
    assert!(r.0.get("rc").is_none());
    assert!(r.0.get("stdout").is_none());
    assert!(r.0.get("cmd").is_none());
}

#[test]
fn cancellation_kills_the_whole_process_group() {
    // `sh -c` forks a grandchild; killing only the shell would leave `sleep` running.
    // The marker has to live inside `sleep`'s duration argument: GNU `sleep` rejects a
    // second, non-numeric argument outright, and a lone trailing simple command would be
    // exec'd (replacing the shell) rather than forked, leaving no grandchild to kill.
    let marker = format!("30.{}", std::process::id());
    let alive = |marker: &str| {
        Command::new("pgrep")
            .args(["-f", marker])
            .output()
            .is_ok_and(|o| !o.stdout.is_empty())
    };
    // Cancel only once the grandchild is actually running. Cancelling straight away kills the
    // shell before it forks, so nothing would survive however narrow the kill was.
    let deadline = Instant::now() + Duration::from_secs(5);
    let run = execute(
        &args(json!({"_raw_params": format!("sleep {marker} & wait")})),
        true,
        &Context::default(),
        &|| alive(&marker) || Instant::now() > deadline,
    );
    assert!(matches!(run, Run::Cancelled));
    thread::sleep(Duration::from_millis(200));
    let survivors = Command::new("pgrep")
        .args(["-f", &marker])
        .output()
        .unwrap();
    assert!(
        survivors.stdout.is_empty(),
        "grandchild survived: {}",
        String::from_utf8_lossy(&survivors.stdout)
    );
}

/// A command run under a terminal: both streams arrive as stdout with CR LF line ends,
/// stderr holds only the close line when one is asked for, and the return code is the command's.
#[rstest]
#[case::merged("echo out; echo err >&2; exit 3", json!(true), "out\r\nerr\r\n", "", 3)]
#[case::close_line("printf 'a\nb'", json!("Shared connection to h closed."), "a\r\nb", "Shared connection to h closed.\r\n", 0)]
#[case::a_terminal_is_stdin("test -t 0 && test -t 1 && test -t 2", json!(true), "", "", 0)]
#[case::off("echo out", json!(false), "out\n", "", 0)]
fn a_command_with_a_terminal_reports_in_the_reference_shape(
    #[case] line: &str,
    #[case] tty: Value,
    #[case] stdout: &str,
    #[case] stderr: &str,
    #[case] rc: i64,
) {
    let Run::Done(r) = execute(
        &args(json!({"_raw_params": line, "_volant_tty": tty, "strip_empty_ends": false})),
        true,
        &Context::default(),
        &|| false,
    ) else {
        panic!("cancelled")
    };
    assert_eq!(r.0["stdout"], stdout);
    assert_eq!(r.0["stderr"], stderr);
    assert_eq!(r.0["rc"], rc);
}

#[rstest]
#[case::not_a_boolean(json!({"_raw_params": "true", "_volant_tty": 3}), "_volant_tty must be")]
#[case::stdin(json!({"_raw_params": "cat", "_volant_tty": true, "stdin": "x"}), "takes no stdin")]
fn a_terminal_that_cannot_be_given_is_refused(#[case] given: Value, #[case] why: &str) {
    let Run::Done(r) = execute(&args(given), true, &Context::default(), &|| false) else {
        panic!("cancelled")
    };
    assert!(r.failed());
    assert!(
        r.0["msg"].as_str().unwrap().contains(why),
        "{:?}",
        r.0["msg"]
    );
}

#[test]
fn cancellation_kills_the_group_of_a_command_with_a_terminal() {
    let marker = format!("31.{}", std::process::id());
    // Without `pgrep` the survivor check below would pass over nothing.
    Command::new("pgrep")
        .arg("--version")
        .output()
        .expect("this test needs pgrep to look for survivors");
    let alive = |marker: &str| {
        Command::new("pgrep")
            .args(["-f", marker])
            .output()
            .is_ok_and(|o| !o.stdout.is_empty())
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let run = execute(
        &args(json!({"_raw_params": format!("sleep {marker} & wait"), "_volant_tty": true})),
        true,
        &Context::default(),
        &|| alive(&marker) || Instant::now() > deadline,
    );
    assert!(matches!(run, Run::Cancelled));
    thread::sleep(Duration::from_millis(200));
    assert!(!alive(&marker), "the command's child outlived the cancel");
}

/// The cancellation predicate consumes what it reports: in the agent it is a `try_recv` on the
/// control channel, so the `Cancel` is popped and the answer is `true` exactly once.
#[test]
fn a_cancellation_seen_by_one_reader_is_seen_by_the_rest() {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let started = Instant::now();
        let taken = Cell::new(false);
        let run = execute(
            &args(json!({"_raw_params": "sleep 10 & exit 0"})),
            true,
            &Context::default(),
            &|| started.elapsed() > Duration::from_millis(300) && !taken.replace(true),
        );
        let _ = tx.send(matches!(run, Run::Cancelled));
    });
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)),
        Ok(true),
        "the run did not end cancelled: a reader lost the cancellation"
    );
}
