// SPDX-License-Identifier: GPL-3.0-or-later
use super::helpers::{command_reply, plain_path};
use super::*;
use crate::action_plugins::LinkFacts;
use crate::config::HostNamedPaths;

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

/// Starts the plugin `name` with `args`, of which the ones in `named` were named by a managed
/// host, under the mode `mode` and in a playbook at `/pb`.
fn start_naming(
    mode: HostNamedPaths,
    name: &'static str,
    args: Value,
    named: &[&str],
) -> Box<dyn Plugin> {
    let Value::Object(args) = args else {
        unreachable!()
    };
    let untrusted = named.iter().map(ToString::to_string).collect();
    let running = crate::vars::HostVars::default();
    let templar = crate::template::Templar::new(std::path::PathBuf::from("."));
    let origin = crate::compile::Origin::default();
    let mut warnings = Vec::new();
    start(
        name,
        &[],
        Context {
            link: &LinkFacts::default(),
            args: &args,
            args_untrusted: &untrusted,
            running_vars: &running,
            delegated: false,
            escalated: false,
            local: false,
            item_vars: &running,
            templar: &templar,
            origin: &origin,
            playbook_dir: std::path::Path::new("/pb"),
            host_named_paths: mode,
            host_named_dest_dirs: None,
            warnings: &mut warnings,
            environment: None,
            timeout: None,
            check_mode: false,
            diff: false,
            no_log: false,
            async_val: 0,
            action: "probe",
        },
    )
}

/// The message of an item that fails at once.
fn failure(mut plugin: Box<dyn Plugin>) -> String {
    let Step::Done(result) = plugin.next(None) else {
        panic!("the item should fail at once");
    };
    assert!(result.failed());
    result.0["msg"].as_str().unwrap().to_string()
}

/// Under `refuse`, a host-named `script` line or `cmd`, `assemble` or `uri` source fails the
/// item before the helper is asked for anything.
#[rstest::rstest]
#[case::script("script", json!({"_raw_params": "/etc/passwd"}), "_raw_params")]
#[case::script_cmd("script", json!({"cmd": "/etc/passwd"}), "cmd")]
#[case::assemble("assemble", json!({"src": "/etc/passwd", "remote_src": false}), "src")]
#[case::uri("uri", json!({"src": "/etc/passwd"}), "src")]
fn a_host_named_controller_file_is_refused(
    #[case] name: &'static str,
    #[case] args: Value,
    #[case] arg: &str,
) {
    let msg = failure(start_naming(HostNamedPaths::Refuse, name, args, &[arg]));
    assert!(msg.contains(&format!(
        "the '{arg}' of this task was named by a managed host"
    )));
}

/// A name the mode lets through reaches the plugin, which here fails for want of the helper;
/// `contained` stops a name outside the files and templates directories.
#[rstest::rstest]
#[case::script_allowed("allow", "script", json!({"_raw_params": "/etc/passwd --flag"}), "_raw_params", None)]
#[case::script_inside("contained", "script", json!({"_raw_params": "tool.sh --flag ../x"}), "_raw_params", None)]
#[case::script_outside("contained", "script", json!({"_raw_params": "/etc/passwd --flag"}), "_raw_params", Some("'/etc/passwd' is not inside"))]
#[case::script_quoted("contained", "script", json!({"_raw_params": "'../up.sh' arg"}), "_raw_params", Some("'../up.sh' is not inside"))]
#[case::script_tilde("contained", "script", json!({"_raw_params": "~/hs.sh"}), "_raw_params", Some("'~/hs.sh' is not inside"))]
#[case::script_cmd_outside("contained", "script", json!({"cmd": "/etc/passwd"}), "cmd", Some("'/etc/passwd' is not inside"))]
#[case::script_cmd_inside("contained", "script", json!({"cmd": "tool.sh"}), "cmd", None)]
#[case::assemble_local("contained", "assemble", json!({"src": "../parts", "remote_src": false}), "src", Some("'../parts' is not inside"))]
#[case::assemble_local_tilde("contained", "assemble", json!({"src": "~/.ssh", "remote_src": "no"}), "src", Some("'~/.ssh' is not inside"))]
#[case::assemble_on_the_host("contained", "assemble", json!({"src": "/etc/parts"}), "src", None)]
#[case::assemble_on_the_host_said("contained", "assemble", json!({"src": "/etc/parts", "remote_src": "yes"}), "src", None)]
#[case::copy_on_the_host("contained", "copy", json!({"src": "/etc/hosts", "remote_src": true}), "src", None)]
#[case::copy_local("contained", "copy", json!({"src": "/etc/hosts", "remote_src": false}), "src", Some("'/etc/hosts' is not inside"))]
#[case::template_tilde("contained", "template", json!({"src": "~/t.j2"}), "src", Some("'~/t.j2' is not inside"))]
#[case::script_comment_word("contained", "script", json!({"_raw_params": "#dir/x.sh --flag"}), "_raw_params", Some("'#dir/x.sh' is not inside"))]
#[case::script_cmd_comment_word("contained", "script", json!({"cmd": "  #x a"}), "cmd", Some("'#x' is not inside"))]
#[case::script_quoted_hash("contained", "script", json!({"_raw_params": "'#x.sh' a"}), "_raw_params", None)]
#[case::script_hash_inside("contained", "script", json!({"_raw_params": "a#b.sh"}), "_raw_params", None)]
#[case::copy_on_the_host_y("contained", "copy", json!({"src": "/etc/hosts", "remote_src": "y"}), "src", None)]
#[case::copy_on_the_host_capital_t("contained", "copy", json!({"src": "/etc/hosts", "remote_src": "T"}), "src", None)]
#[case::copy_on_the_host_one("contained", "copy", json!({"src": "/etc/hosts", "remote_src": 1}), "src", None)]
#[case::copy_on_the_host_one_point_zero("contained", "copy", json!({"src": "/etc/hosts", "remote_src": 1.0}), "src", None)]
#[case::copy_on_the_host_string_one("contained", "copy", json!({"src": "/etc/hosts", "remote_src": " 1 "}), "src", None)]
#[case::copy_on_the_host_on("contained", "copy", json!({"src": "/etc/hosts", "remote_src": "ON"}), "src", None)]
#[case::copy_local_n("contained", "copy", json!({"src": "/etc/hosts", "remote_src": "n"}), "src", Some("is not inside"))]
#[case::copy_local_string_one_point_zero("contained", "copy", json!({"src": "/etc/hosts", "remote_src": "1.0"}), "src", Some("is not inside"))]
#[case::copy_local_two("contained", "copy", json!({"src": "/etc/hosts", "remote_src": 2}), "src", Some("is not inside"))]
#[case::unarchive_copy_no("contained", "unarchive", json!({"src": "/etc/a.tgz", "copy": "no"}), "src", None)]
#[case::unarchive_copy_n("contained", "unarchive", json!({"src": "/etc/a.tgz", "copy": "n"}), "src", None)]
#[case::unarchive_copy_yes("contained", "unarchive", json!({"src": "/etc/a.tgz", "copy": "yes"}), "src", Some("is not inside"))]
#[case::assemble_local_f("contained", "assemble", json!({"src": "/etc/parts", "remote_src": "F"}), "src", Some("is not inside"))]
#[case::assemble_on_the_host_t("contained", "assemble", json!({"src": "/etc/parts", "remote_src": "t"}), "src", None)]
#[case::uri_body("contained", "uri", json!({"body_format": "form-multipart", "body": {"f": {"filename": "~/.ssh/id_rsa"}}}), "body", Some("'~/.ssh/id_rsa' is not inside"))]
#[case::uri_body_content("contained", "uri", json!({"body_format": "form-multipart", "body": {"f": {"filename": "~/x", "content": "text"}}}), "body", None)]
#[case::uri_body_inside("contained", "uri", json!({"body_format": "form-multipart", "body": {"f": {"filename": "up.bin"}}}), "body", None)]
#[case::uri_on_the_host("contained", "uri", json!({"src": "/etc/hosts", "remote_src": true}), "src", None)]
#[case::template_ignores_remote_src("contained", "template", json!({"src": "/etc/hosts", "remote_src": true}), "src", Some("'/etc/hosts' is not inside"))]
#[case::template_ignores_remote_src_refuse("refuse", "template", json!({"src": "/etc/hosts", "remote_src": true}), "src", Some("a controller file a host chose is never sent"))]
#[case::collection_ignores_remote_src("contained", "ansible.posix.synchronize", json!({"src": "/etc/hosts", "remote_src": true}), "src", Some("'/etc/hosts' is not inside"))]
#[case::fetch_reads_the_host("refuse", "fetch", json!({"src": "/etc/hosts", "dest": "out"}), "src", None)]
fn a_host_named_controller_file_is_read_as_the_mode_says(
    #[case] mode: &str,
    #[case] name: &'static str,
    #[case] args: Value,
    #[case] arg: &str,
    #[case] refused: Option<&str>,
) {
    let mode = HostNamedPaths::parse(mode).unwrap();
    let msg = failure(start_naming(mode, name, args, &[arg]));
    match refused {
        Some(says) => assert!(
            msg.contains("named by a managed host") && msg.contains(says),
            "{msg}"
        ),
        None => assert!(
            msg.contains("needs the controller's Python helper"),
            "{msg}"
        ),
    }
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
        job: None,
    };
    let mut plugin = Bridged {
        name: "wait_for_connection",
        modules: &["ping"],
        hostvars: Arc::default(),
        untrusted_hosts: Default::default(),
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

/// A `sudoable` command goes over the task link with a terminal; a `sudoable=False` one goes over
/// the link that does not escalate, with none.
#[rstest::rstest]
#[case::sudoable(true, Some(true))]
#[case::housekeeping(false, None)]
fn a_command_that_must_not_escalate_runs_as_the_connection_user(
    #[case] sudoable: bool,
    #[case] terminal: Option<bool>,
) {
    let (_, asks) = channel();
    let (replies, _) = channel();
    let mut plugin = Bridged {
        name: "script",
        modules: &[],
        hostvars: Arc::default(),
        untrusted_hosts: Default::default(),
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
    let (sub, plain) = match plugin.step_for(ask) {
        Ok((Step::Run(sub), Pending::Command)) => (sub, false),
        Ok((Step::Plain(step), Pending::Command)) => match *step {
            Step::Run(sub) => (sub, true),
            other => panic!("the command should run as the connection user: {other:?}"),
        },
        _ => panic!("the command should become a sub-task"),
    };
    assert_eq!(sub.module, "raw");
    assert_eq!(plain, !sudoable);
    // Only a command the reference sends through `ssh -tt` asks for a terminal.
    assert_eq!(
        sub.args.get("_volant_tty").and_then(Value::as_bool),
        terminal
    );
}

/// The play context a plugin reads names the connection, address and user of the host the module
/// runs on, and the key and escalation of the link: `synchronize` builds `rsync` from them.
#[rstest::rstest]
#[case::ssh(
    json!({"ansible_host": "192.0.2.4", "ansible_user": "deploy"}),
    LinkFacts { private_key: Some("/k/id".into()), ..LinkFacts::default() },
    false,
    json!({"connection": "ssh", "remote_addr": "192.0.2.4", "remote_user": "deploy",
           "check_mode": false, "diff": false, "private_key_file": "/k/id"}),
)]
#[case::the_link_s_user_when_the_host_names_none(
    json!({"inventory_hostname": "h1"}),
    LinkFacts { remote_user: Some("ops".into()), ..LinkFacts::default() },
    false,
    json!({"connection": "ssh", "remote_addr": "h1", "remote_user": "ops",
           "check_mode": false, "diff": false}),
)]
#[case::escalation(
    json!({"inventory_hostname": "h1"}),
    LinkFacts { escalation: Some(("sudo".into(), "postgres".into())), ..LinkFacts::default() },
    false,
    json!({"connection": "ssh", "remote_addr": "h1", "remote_user": null,
           "check_mode": false, "diff": false,
           "become": true, "become_method": "sudo", "become_user": "postgres"}),
)]
#[case::local(
    json!({"inventory_hostname": "h1", "ansible_connection": "ssh"}),
    LinkFacts::default(),
    true,
    json!({"connection": "local", "remote_addr": "h1", "remote_user": null,
           "check_mode": false, "diff": false}),
)]
fn the_play_context_describes_the_link_the_module_runs_over(
    #[case] running: Value,
    #[case] link: LinkFacts,
    #[case] local: bool,
    #[case] want: Value,
) {
    let args = Map::new();
    let untrusted = std::collections::BTreeSet::new();
    let vars = crate::vars::HostVars::default();
    let templar = crate::template::Templar::new(std::path::PathBuf::from("."));
    let origin = crate::compile::Origin::default();
    let mut warnings = Vec::new();
    let ctx = Context {
        link: &link,
        args: &args,
        args_untrusted: &untrusted,
        running_vars: &vars,
        delegated: false,
        escalated: false,
        local,
        item_vars: &vars,
        templar: &templar,
        origin: &origin,
        playbook_dir: std::path::Path::new("."),
        warnings: &mut warnings,
        host_named_paths: HostNamedPaths::Allow,
        host_named_dest_dirs: None,
        environment: None,
        timeout: None,
        check_mode: false,
        diff: false,
        no_log: false,
        async_val: 0,
        action: "probe",
    };
    assert_eq!(play_context_of(&ctx, running.as_object().unwrap()), want);
}

/// A module the plugin wraps as a job starts as one with the plugin's limit, and the plugin polls
/// it with the agent's own `async_status`, which no plugin lists.
#[rstest::rstest]
#[case::wrapped("ansible.builtin.setup", Some(4), "setup", Some(4))]
#[case::unbounded("setup", Some(0), "setup", Some(0))]
#[case::polled("ansible.legacy.async_status", None, "async_status", None)]
#[case::plain("setup", None, "setup", None)]
fn a_wrapped_module_starts_as_a_job(
    #[case] name: &str,
    #[case] job: Option<u64>,
    #[case] module: &str,
    #[case] limit: Option<u64>,
) {
    let (_, asks) = channel();
    let (replies, _) = channel();
    let mut plugin = Bridged {
        name: "gather_facts",
        modules: &["setup"],
        hostvars: Arc::default(),
        untrusted_hosts: Default::default(),
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
    let ask = Ask::Module {
        name: name.into(),
        args: json!({}),
        files: Vec::new(),
        job,
    };
    let (sub, started) = match plugin.step_for(ask) {
        Ok((Step::Run(sub), Pending::Module)) => (sub, None),
        Ok((Step::RunJob { sub, limit }, Pending::Module)) => (sub, Some(limit)),
        other => panic!(
            "the module should become a sub-task: {:?}",
            other.map(|(s, _)| s)
        ),
    };
    assert_eq!((sub.module, started), (module, limit));
}

/// Another host's variables travel with whether the helper may render them: never for a host
/// holding a value a managed host sent, through either host's view.
#[rstest::rstest]
#[case::trusted("h2", json!({"vars": {"v": "{{ w }}"}, "trusted": true}))]
#[case::from_a_host("h3", json!({"vars": {"v": "{{ w }}"}, "trusted": false}))]
#[case::seen_by_the_running_host("h4", json!({"vars": {"v": "{{ w }}"}, "trusted": false}))]
#[case::unknown("nowhere", Value::Null)]
fn another_host_s_variables_say_whether_they_render(#[case] host: &str, #[case] reply: Value) {
    let (_, asks) = channel();
    let (replies, _) = channel();
    let entry = json!({"v": "{{ w }}"});
    let hostvars = ["h2", "h3", "h4"].map(|h| (h.to_string(), Arc::new(entry.clone())));
    let mut plugin = Bridged {
        name: "probe",
        modules: &[],
        hostvars: Arc::new(hostvars.into_iter().collect()),
        untrusted_hosts: [
            Arc::new(["h3".to_string()].into()),
            Arc::new(["h4".to_string()].into()),
        ],
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
    let ask = Ask::HostVar { host: host.into() };
    let Err(answer) = plugin.step_for(ask) else {
        panic!("the controller should answer without the host");
    };
    assert_eq!(answer, Reply::Value(reply));
}
