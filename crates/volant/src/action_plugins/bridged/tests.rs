// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

/// A command's reply carries its return code and both streams, and one with none is an error.
#[test]
fn a_raw_result_is_a_command_reply() {
    let ran = TaskResult(
        json!({"rc": 3, "stdout": "o\n", "stderr": "e\n", "failed": true})
            .as_object()
            .unwrap()
            .clone(),
    );
    assert_eq!(
        command_reply(&ran),
        Reply::Value(json!({"rc": 3, "stdout": "o\n", "stderr": "e\n"}))
    );
    assert_eq!(
        command_reply(&super::super::gone("link lost")),
        Reply::Error("link lost".into())
    );
}

/// Only a path the shell reads as one word inside single quotes is taken as a directory.
#[rstest::rstest]
#[case("/home/u/.ansible/tmp/ansible-tmp-a1", true)]
#[case("/", false)]
#[case("relative", false)]
#[case("/tmp/x'; rm -rf /", false)]
#[case("/tmp/a\n/tmp/b", false)]
fn a_temporary_directory_is_one_plain_absolute_path(#[case] path: &str, #[case] ok: bool) {
    assert_eq!(plain_path(path), ok);
}

/// The arguments of a free-form `script` and an `assemble` source are never a controller file
/// a managed host named: the item fails before the helper is asked for anything.
#[rstest::rstest]
#[case::script("script", "_raw_params")]
#[case::assemble("assemble", "src")]
fn a_host_named_controller_file_is_refused(#[case] name: &'static str, #[case] arg: &str) {
    let mut args = Map::new();
    args.insert(arg.into(), json!("/etc/passwd"));
    let untrusted = std::iter::once(arg.to_string()).collect();
    let running = crate::vars::HostVars::default();
    let templar = crate::template::Templar::new(std::path::PathBuf::from("."));
    let origin = crate::compile::Origin::default();
    let mut warnings = Vec::new();
    let mut plugin = start(
        name,
        &[],
        Context {
            args: &args,
            args_untrusted: &untrusted,
            running_vars: &running,
            delegated: false,
            escalated: false,
            local: false,
            item_vars: &running,
            templar: &templar,
            origin: &origin,
            playbook_dir: std::path::Path::new("."),
            warnings: &mut warnings,
            environment: None,
            timeout: None,
            check_mode: false,
            diff: false,
        },
    );
    let Step::Done(result) = plugin.next(None) else {
        panic!("the item should fail at once");
    };
    assert!(result.failed());
    assert!(
        result.0["msg"].as_str().unwrap().contains(arg),
        "{:?}",
        result.0
    );
}

/// A ping whose link went reaches the plugin as an error, and the next ping is asked over a
/// fresh link, whose answer is the plugin's.
#[test]
fn a_ping_that_lost_its_link_is_retried_over_a_new_one() {
    let (to_plugin, asks) = channel();
    let (replies, from_plugin) = channel();
    let ping = || Ask::Module {
        name: "ansible.legacy.ping".into(),
        args: json!({"data": "x"}),
        files: Vec::new(),
    };
    let mut plugin = Bridged {
        name: "wait_for_connection",
        modules: &["ping"],
        hostvars: Arc::default(),
        asks,
        replies: Some(replies),
        ready: Arc::new(Notify::new()),
        pending: None,
        tmps: Vec::new(),
        _vault_src: None,
        ending: None,
        timeout: None,
        deadline: Instant::now() + Duration::from_secs(600),
        stale: false,
        connect_timeout: Duration::from_secs(5),
    };
    to_plugin.send(FromHelper::Ask(ping())).unwrap();
    assert!(matches!(plugin.next(None), Step::RunDropping(_)));
    to_plugin.send(FromHelper::Ask(ping())).unwrap();
    let step = plugin.next(Some(super::super::gone("connection refused")));
    assert_eq!(
        from_plugin.try_recv().unwrap(),
        Reply::Error("connection refused".into())
    );
    assert!(matches!(step, Step::Reconnect { .. }), "{step:?}");
    // The fresh link's answer is the ping's.
    let pong = TaskResult(json!({"ping": "pong"}).as_object().unwrap().clone());
    to_plugin.send(FromHelper::Ask(ping())).unwrap();
    let step = plugin.next(Some(pong));
    assert_eq!(
        from_plugin.try_recv().unwrap(),
        Reply::Value(json!({"ping": "pong"}))
    );
    assert!(matches!(step, Step::RunDropping(_)), "{step:?}");
}

/// A command the plugin marks as not to escalate still runs, over the task link (the escalated one under `become`).
#[rstest::rstest]
#[case::sudoable(true, Some(true))]
#[case::housekeeping(false, None)]
fn a_command_that_must_not_escalate_runs_over_the_task_link(
    #[case] sudoable: bool,
    #[case] terminal: Option<bool>,
) {
    let (_, asks) = channel();
    let (replies, _) = channel();
    let mut plugin = Bridged {
        name: "script",
        modules: &[],
        hostvars: Arc::default(),
        asks,
        replies: Some(replies),
        ready: Arc::new(Notify::new()),
        pending: None,
        tmps: Vec::new(),
        _vault_src: None,
        ending: None,
        timeout: None,
        deadline: Instant::now() + Duration::from_secs(600),
        stale: false,
        connect_timeout: Duration::from_secs(5),
    };
    let ask = Ask::Command {
        cmd: "true".into(),
        stdin: None,
        sudoable,
    };
    let Ok((Step::Run(sub), Pending::Command)) = plugin.step_for(ask) else {
        panic!("the command should become a sub-task");
    };
    assert_eq!(sub.module, "raw");
    // Only a command the reference sends through `ssh -tt` asks for a terminal.
    assert_eq!(
        sub.args.get("_volant_tty").and_then(Value::as_bool),
        terminal
    );
}
