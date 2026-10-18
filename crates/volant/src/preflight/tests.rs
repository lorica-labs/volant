// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use crate::compile::META_ACTIONS;
use crate::playbook::parse;
use crate::stats::error_code;
use rstest::rstest;

/// A `pause` with a prompt and a duration on a terminal is refused before anything connects
/// when the step is sure to run, and left to the run when something may leave it out.
#[test]
fn a_pause_that_would_prompt_on_a_terminal_is_refused_only_when_sure_to_run() {
    let verdict_under = |tasks: &str, interactive: bool, run: Vec<String>| {
        let pb = parse(
            &format!("- hosts: all\n  gather_facts: false\n  tasks:\n{tasks}"),
            "x.yml",
        )
        .unwrap();
        let compiled = crate::compile::compile(
            &pb.plays[0],
            &crate::roles::RoleSearch::default(),
            &crate::compile::TagSelection::new(run, Vec::new()),
        )
        .unwrap();
        compiled
            .steps
            .iter()
            .try_for_each(|step| check_pause(&compiled, step, interactive))
            .map_err(|e| format!("{e:#}"))
    };
    let verdict = |tasks: &str, interactive: bool| verdict_under(tasks, interactive, Vec::new());
    for pause in ["{prompt: Go?, seconds: 1}", "{prompt: Go?, minutes: 1}"] {
        let tasks = format!("    - name: Wait\n      pause: {pause}\n");
        assert_eq!(
            verdict(&tasks, true),
            Err(format!("task 'Wait': {PROMPT_REFUSED}")),
            "{pause}"
        );
        assert_eq!(verdict(&tasks, false), Ok(()), "{pause}");
    }
    for tasks in [
        "    - pause: {seconds: 1}\n",
        "    - pause: {minutes: 1}\n",
        "    - pause: {prompt: Go?}\n      when: confirm | default(false)\n",
        "    - pause: {prompt: Go?}\n      tags: [never]\n",
        "    - pause: {prompt: Go?}\n      loop: \"{{ pending | default([]) }}\"\n",
        "    - block:\n        - pause: {prompt: Go?}\n      when: confirm | default(false)\n",
        "    - block:\n        - debug: msg=hi\n      rescue:\n        - pause: {prompt: Go?}\n",
        "    - block:\n        - debug: msg=hi\n      rescue:\n        - block:\n            - pause: {prompt: Go?}\n",
    ] {
        assert_eq!(verdict(tasks, true), Ok(()), "{tasks}");
    }
    // A `never` task that `--tags never` selects is in the compiled steps and runs.
    assert!(
        verdict_under(
            "    - name: Wait\n      pause: {prompt: Go?, seconds: 1}\n      tags: [never]\n",
            true,
            vec!["never".into()],
        )
        .is_err()
    );
}

fn refusal(text: &str) -> String {
    let pb = parse(text, "x.yml").expect("the loader accepts the whole grammar");
    let err = check(&pb).unwrap_err();
    assert_eq!(error_code(&err), 4, "{err:#}");
    format!("{err:#}")
}

/// A one-task play around `body`, for the rows that only differ in the task.
fn task_play(body: &str) -> String {
    format!("- hosts: all\n  tasks:\n    - name: T\n{body}")
}

/// Everything the pre-flight refuses is refused at exit 4, naming what it refuses.
#[rstest]
#[case::until_on_include_tasks(task_play("      include_tasks: inc.yml\n      until: false\n"), &["keyword 'until' is not supported yet on 'include_tasks'"])]
#[case::retries_on_include_tasks(task_play("      include_tasks: inc.yml\n      retries: 2\n"), &["keyword 'retries' is not supported yet on 'include_tasks'"])]
#[case::delay_on_include_tasks(task_play("      include_tasks: inc.yml\n      delay: 1\n"), &["keyword 'delay' is not supported yet on 'include_tasks'"])]
#[case::until_on_include_role(task_play("      include_role:\n        name: r\n      until: false\n"), &["keyword 'until' is not supported yet on 'include_role'"])]
#[case::retries_on_include_role(task_play("      include_role:\n        name: r\n      retries: 2\n"), &["keyword 'retries' is not supported yet on 'include_role'"])]
#[case::delay_on_include_role(task_play("      include_role:\n        name: r\n      delay: 1\n"), &["keyword 'delay' is not supported yet on 'include_role'"])]
#[case::shell_true(task_play("      ansible.legacy.shell: /bin/echo $HOME\n      args:\n        expand_argument_vars: true\n"), &["argument 'expand_argument_vars' is not supported yet on 'ansible.legacy.shell'"])]
#[case::shell_false(task_play("      ansible.legacy.shell: /bin/echo $HOME\n      args:\n        expand_argument_vars: false\n"), &["argument 'expand_argument_vars' is not supported yet on 'ansible.legacy.shell'"])]
#[case::shell_no(task_play("      ansible.legacy.shell: /bin/echo $HOME\n      args:\n        expand_argument_vars: \"no\"\n"), &["argument 'expand_argument_vars' is not supported yet on 'ansible.legacy.shell'"])]
#[case::strategy_unknown("- hosts: all\n  strategy: mitogen_free\n  tasks:\n    - command: echo hi\n".to_string(), &["strategy 'mitogen_free' is not supported yet"])]
#[case::collection_splat(task_play("      ns.coll.mod: \"{{ d }}\"\n"), &["task 'T': module 'ns.coll.mod' takes its arguments from one template"])]
#[case::builtin_splat(task_play("      ping: \"{{ d }}\"\n"), &["task 'T': module 'ping' takes its arguments from one template"])]
#[case::builtin_no_collection_can_supply(task_play("      ansible.builtin.nosuch: x=1\n"), &["couldn't resolve module/action 'ansible.builtin.nosuch'"])]
fn the_preflight_refuses_by_name(#[case] text: String, #[case] needles: &[&str]) {
    let got = refusal(&text);
    for needle in needles {
        assert!(got.contains(needle), "{needle:?} not in {got}");
    }
}

/// What the pre-flight lets through where the value or the module decides, or the controller
/// answers.
#[rstest]
#[case::check_mode_false_on_a_task(task_play("      command: echo hi\n      check_mode: false\n"))]
#[case::check_mode_false_on_a_play("- hosts: all\n  check_mode: false\n  tasks:\n    - command: echo hi\n".to_string())]
#[case::check_mode_on_a_task(task_play("      command: echo hi\n      check_mode: true\n"))]
#[case::check_mode_on_an_empty_play("- hosts: all\n  check_mode: true\n  diff: true\n  tasks: []\n".to_string())]
#[case::check_mode_on_an_empty_block("- hosts: all\n  tasks:\n    - block: []\n      check_mode: true\n      diff: true\n".to_string())]
#[case::diff_on_a_task(task_play("      command: echo hi\n      diff: true\n"))]
#[case::retry_on_an_ordinary_task(task_play(
    "      command: echo hi\n      until: false\n      retries: 2\n      delay: 1\n"
))]
#[case::retry_on_include_vars(task_play("      include_vars: v.yml\n      retries: 2\n"))]
#[case::linear("- hosts: all\n  strategy: linear\n  tasks:\n    - command: echo hi\n".to_string())]
#[case::free("- hosts: all\n  strategy: free\n  tasks:\n    - command: echo hi\n".to_string())]
#[case::host_pinned("- hosts: all\n  strategy: host_pinned\n  tasks:\n    - command: echo hi\n".to_string())]
#[case::debug("- hosts: all\n  strategy: debug\n  tasks:\n    - command: echo hi\n".to_string())]
#[case::unknown_argument_is_the_agents(task_play(
    "      command: /bin/true\n      args:\n        no_such_arg: 1\n"
))]
#[case::arguments_this_release_honours(task_play(
    "      command: /bin/true\n      args:\n        chdir: /tmp\n        stdin_add_newline: false\n"
))]
#[case::raw_takes_no_such_check(task_play(
    "      raw: /bin/true\n      args:\n        expand_argument_vars: true\n"
))]
#[case::expansion_on(task_play(
    "      command: /bin/echo $HOME
      args:
        expand_argument_vars: true
"
))]
#[case::expansion_off(task_play(
    "      command: /bin/echo $HOME\n      args:\n        expand_argument_vars: false\n"
))]
#[case::shell_without_the_argument(task_play(
    "      shell: /bin/echo $HOME\n      args:\n        chdir: /tmp\n"
))]
#[case::omitted_argument(task_play(
    "      command: /bin/echo $HOME\n      args:\n        expand_argument_vars: \"{{ omit }}\"\n"
))]
#[case::builtin_run_through_a_payload(task_play("      lineinfile: path=/tmp/x\n"))]
#[case::implemented_module(task_play("      command: echo hi\n"))]
#[case::controller_side_module(task_play("      debug: msg=hi\n"))]
#[case::name_left_to_the_controller(task_play("      nosuchmodule: x\n"))]
#[case::builtin_stray_word(task_play("      ping: foo\n"))]
#[case::retry_on_a_plugin(
    "- hosts: all\n  tasks:\n    - name: Copy k3s.yaml to second file\n      copy:\n        src: /etc/rancher/k3s/k3s.yaml\n        dest: /etc/rancher/k3s/k3s-copy.yaml\n        mode: \"0600\"\n        remote_src: true\n      retries: 3\n      register: k3s_server_copy_yaml\n    - name: Again\n      package: name=bash\n      until: false\n      delay: 1\n".to_string()
)]
#[case::static_with_lookups(
    "- hosts: all\n  tasks:\n    - name: Distribute K3s binary\n      copy:\n        src: \"{{ item }}\"\n        dest: /usr/local/bin/k3s\n      with_first_found:\n        - files:\n            - \"{{ airgap_dir }}/k3s\"\n          skip: true\n    - name: Distribute K3s images\n      copy:\n        src: \"{{ item }}\"\n        dest: /var/lib/rancher/k3s/agent/images/\n      with_fileglob:\n        - \"{{ airgap_dir }}/*.tar.gz\"\n    - name: Execute OS related tasks on the Raspberry Pi\n      include_tasks: \"{{ item }}\"\n      with_first_found:\n        - \"setup/{{ ansible_distribution }}.yml\"\n        - setup/default.yml\n".to_string()
)]
fn the_preflight_lets_through(#[case] text: String) {
    let pb = parse(&text, "x.yml").unwrap();
    check(&pb).unwrap_or_else(|e| panic!("{text}: {e:#}"));
}

/// The plugins and fact modules this release runs are admitted with their arguments read.
#[rstest]
fn the_modules_this_release_runs_are_admitted(
    #[values(
        "package",
        "service",
        "copy",
        "template",
        "unarchive",
        "fetch",
        "reboot",
        "ansible.builtin.package",
        "setup",
        "package_facts",
        "service_facts",
        "mount_facts",
        "getent"
    )]
    module: &str,
) {
    let pb = parse(
        &format!("- hosts: all\n  tasks:\n    - name: Now\n      {module}: name=bash\n"),
        "x.yml",
    )
    .unwrap();
    check(&pb).unwrap_or_else(|e| panic!("{module}: {e:#}"));
    let TaskOrBlock::Task(task) = &pb.plays[0].tasks[0] else {
        panic!("a task");
    };
    assert_eq!(task.args.get("name"), Some(&serde_json::json!("bash")));
    if [
        "setup",
        "package_facts",
        "service_facts",
        "mount_facts",
        "getent",
    ]
    .contains(&module)
    {
        assert!(crate::python::is_python_module(module), "{module}");
    }
}

/// A collection's module passes both passes and is collected for the controller to resolve; a
/// stray word fails the task when it runs; the controller's answers are refused in its words.
#[test]
#[expect(clippy::too_many_lines)]
fn collection_modules_are_set_aside_and_the_controllers_answers_refused() {
    let text = "- hosts: all\n  gather_facts: false\n  tasks:\n    - name: Forward\n      ansible.posix.sysctl: name=net.ipv4.ip_forward value=1\n    - debug: msg=x\n  handlers:\n    - name: Reload\n      community.general.ufw: state=reloaded\n";
    let pb = parse(text, "x.yml").unwrap();
    check(&pb).expect("a collection's module waits for the controller's answer");
    let compiled = crate::compile::compile(
        &pb.plays[0],
        &crate::roles::RoleSearch::default(),
        &crate::compile::TagSelection::new(Vec::new(), Vec::new()),
    )
    .unwrap();
    check_steps(&compiled, true).expect("and so does the second pass");
    assert_eq!(
        collection_modules(&compiled),
        [
            ("Forward".to_string(), "ansible.posix.sysctl".to_string()),
            ("Reload".to_string(), "community.general.ufw".to_string()),
        ]
    );
    let stray = |module: &str| {
        let pb = parse(
            &format!("- hosts: all\n  tasks:\n    - name: Stray\n      {module}: a=1 stray\n"),
            "x.yml",
        )
        .unwrap();
        check(&pb).expect("set aside before it resolved");
        let compiled = crate::compile::compile(
            &pb.plays[0],
            &crate::roles::RoleSearch::default(),
            &crate::compile::TagSelection::new(Vec::new(), Vec::new()),
        )
        .unwrap();
        let facts = crate::python::ModuleFacts {
            module_fqn: "fqn".into(),
            profile: "legacy".into(),
            rlimit_nofile: 0,
            extensions: serde_json::Map::new(),
            core: false,
        };
        let union = crate::python::Union {
            hash: "ab".into(),
            zip_b64: "UEsDBA==".into(),
            modules: std::collections::BTreeMap::from([(module.to_string(), facts)]),
            refused: std::collections::BTreeMap::new(),
            natives: crate::python::Natives::default(),
        };
        check_built(&compiled, Some(&union)).map_err(|err| format!("{err:#}"))
    };
    for module in [
        "ns.coll.mod",
        "my_library_module",
        "ansible.windows.win_shell",
    ] {
        stray(module).expect("a stray word fails the task at run time");
    }

    // What the controller made of a collection's module, and what a built union holds,
    // refused in words the operator reads, all at the pre-flight's exit 4.
    use crate::python::Resolved;
    let verdict = |answer: Resolved| {
        check_resolved("T", "ns.coll.mod", &answer).map_err(|err| {
            assert_eq!(error_code(&err), 4, "{err:#}");
            format!("{err:#}")
        })
    };
    assert!(
        verdict(Resolved::Module {
            fqcn: "ns.coll.mod".into(),
            collection: Some(("ns.coll".into(), "1.0.0".into())),
        })
        .is_ok()
    );
    assert!(
        verdict(Resolved::ActionPlugin {
            fqcn: "ns.coll.mod".into(),
            with_module: false,
        })
        .is_ok()
    );
    // A plugin that needs Windows or a network device's connection is refused by name.
    let waiting = include_str!("../later_actions.txt").lines().next().unwrap();
    assert_eq!(
        verdict(Resolved::ActionPlugin {
            fqcn: waiting.into(),
            with_module: false,
        })
        .map_err(|text| text.ends_with("which this release does not run")),
        Err(true)
    );
    let typo = "task 'T': couldn't resolve module/action 'ns.coll.mod'. This often indicates a misspelling, missing collection, or incorrect module path.";
    assert_eq!(
        verdict(Resolved::Missing { collection: None }),
        Err(typo.to_string())
    );
    assert_eq!(
        verdict(Resolved::Missing {
            collection: Some("ns.coll".into())
        }),
        Err(format!(
            "{typo} The collection 'ns.coll' is not installed on the controller: install it with volant runtime install --collections ns.coll"
        ))
    );

    // A collection's module an include brings in runs only if the union holds it.
    let pb = parse(
        "- hosts: all\n  gather_facts: false\n  tasks:\n    - name: Forward\n      ansible.posix.sysctl: name=x value=1\n",
        "x.yml",
    )
    .unwrap();
    let compiled = crate::compile::compile(
        &pb.plays[0],
        &crate::roles::RoleSearch::default(),
        &crate::compile::TagSelection::new(Vec::new(), Vec::new()),
    )
    .unwrap();
    let union = |keys: &[&str]| crate::python::Union {
        hash: "ab".into(),
        zip_b64: "UEsDBA==".into(),
        modules: keys
            .iter()
            .map(|k| {
                (
                    k.to_string(),
                    crate::python::ModuleFacts {
                        module_fqn: format!("fqn.{k}"),
                        profile: "legacy".into(),
                        rlimit_nofile: 0,
                        extensions: serde_json::Map::new(),
                        core: true,
                    },
                )
            })
            .collect(),
        refused: std::collections::BTreeMap::new(),
        natives: crate::python::Natives::default(),
    };
    check_built(&compiled, Some(&union(&["ansible.posix.sysctl"]))).expect("the union holds it");
    for refused in [None, Some(union(&["sysctl", "ping"]))] {
        let err = check_built(&compiled, refused.as_ref()).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("task 'Forward': module 'ansible.posix.sysctl' was not resolved"),
            "{text}"
        );
    }
    let mut set_aside = union(&["ping"]);
    set_aside
        .refused
        .insert("ansible.posix.sysctl".into(), "it is gone.".into());
    assert_eq!(
        format!(
            "{:#}",
            check_built(&compiled, Some(&set_aside)).unwrap_err()
        ),
        "task 'Forward': it is gone."
    );
}

/// Every `meta` action the reference has is accepted, and an unknown one stops the run.
#[test]
fn meta_actions_are_accepted_and_unknown_ones_refused() {
    let play =
        |action: &str| format!("- hosts: all\n  tasks:\n    - name: M\n      meta: {action}\n");
    for action in META_ACTIONS {
        let pb = parse(&play(action), "x.yml").expect("the loader takes every action");
        assert!(check(&pb).is_ok(), "{action} runs");
    }
    // Measured on ansible-core 2.19.12: `invalid meta action requested: nosuchaction`,
    // exit 1 - not the 4 an unloadable playbook gets.
    let pb = parse(&play("nosuchaction"), "x.yml").unwrap();
    let err = check(&pb).unwrap_err();
    assert_eq!(error_code(&err), 1, "{err:#}");
    assert!(
        format!("{err:#}").contains("invalid meta action requested: nosuchaction"),
        "{err:#}"
    );
}

/// `end_role` written where no role is stops the run at exit 4, as the reference stops
/// loading the playbook.
#[test]
fn end_role_outside_a_role_is_refused_at_exit_4() {
    let steps = crate::testing::compiled(
        "- hosts: all
  tasks:
    - meta: end_role
",
    );
    let err = check_steps(&steps, false).unwrap_err();
    assert_eq!(error_code(&err), 4, "{err:#}");
    assert!(
        format!("{err:#}").contains("Cannot execute 'end_role' from outside of a role"),
        "{err:#}"
    );
}
