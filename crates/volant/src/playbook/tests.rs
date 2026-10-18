// SPDX-License-Identifier: GPL-3.0-or-later
use std::path::Path;

use super::*;
use rstest::rstest;

/// The tasks of a play, for the tests that wrote none of their own blocks. A block here is
/// the test's mistake, not the loader's, so it says so rather than being skipped.
fn tasks(play: &Play) -> Vec<&PlayTask> {
    play.tasks
        .iter()
        .map(|item| match item {
            TaskOrBlock::Task(task) => task,
            TaskOrBlock::Block(_) => panic!("this playbook has no blocks"),
        })
        .collect()
}

/// The first task of the first play.
fn first(pb: &Playbook) -> &PlayTask {
    tasks(&pb.plays[0])[0]
}

/// A task keeps where its mapping starts, as ansible-core's `Origin` gives it: measured,
/// `- name: plain fail` on line 5 is `p.yml:5:7`.
#[test]
fn a_task_keeps_the_line_and_column_it_was_written_at() {
    let pb = parse(
        "- hosts: h1\n  gather_facts: false\n  tasks:\n    - debug: msg=hello\n    - name: plain fail\n      command: \"false\"\n",
        "p.yml",
    )
    .unwrap();
    let origins: Vec<(usize, usize)> = tasks(&pb.plays[0])
        .iter()
        .map(|t| t.origin.as_ref().map(|o| (o.line, o.col)).unwrap())
        .collect();
    assert_eq!(origins, [(4, 7), (5, 7)]);
    let path = tasks(&pb.plays[0])[0].origin.as_ref().unwrap().path.clone();
    assert!(
        path.ends_with("p.yml") && Path::new(&*path).is_absolute(),
        "{path}"
    );
}

const SAMPLE: &str = r"
- name: Smoke test
  hosts: web,db
  gather_facts: false
  tasks:
    - name: Say hello
      command: echo hello
    - shell: echo $HOME
      ignore_errors: yes
    - name: Structured args
      ansible.builtin.command:
        cmd: ls -l
        chdir: /tmp
    - name: Key value form
      raw: uptime
    - name: Args block
      command:
        argv: [true]
      args:
        chdir: /
";

#[test]
fn plays_and_tasks_are_loaded() {
    let pb = parse(SAMPLE, "site.yml").unwrap();
    assert_eq!(pb.plays.len(), 1);
    let play = &pb.plays[0];
    assert_eq!(play.name, "Smoke test");
    assert_eq!(play.hosts, "web,db");
    assert!(!play.gather_facts);
    let all = tasks(play);
    assert_eq!(all.len(), 5);

    let t = all[0];
    assert_eq!(
        (t.name.as_str(), t.module.as_str()),
        ("Say hello", "command")
    );
    assert_eq!(t.args["_raw_params"], "echo hello");
    assert!(!t.ignores_errors());

    let t = all[1];
    assert_eq!(t.name, "shell", "unnamed tasks take the module name");
    assert!(t.ignores_errors());

    let t = all[2];
    assert_eq!(t.module, "ansible.builtin.command");
    assert_eq!(t.args["cmd"], "ls -l");
    assert_eq!(t.args["chdir"], "/tmp");

    let t = all[4];
    assert_eq!(t.args["argv"], serde_json::json!([true]));
    assert_eq!(t.args["chdir"], "/");
}

#[test]
fn hosts_lists_defaults_and_escalation_keywords_are_read() {
    let pb = parse("- hosts: [web, db]\n  tasks: []\n", "x.yml").unwrap();
    assert_eq!(pb.plays[0].hosts, "web,db");
    assert_eq!(pb.plays[0].name, "web,db", "unnamed plays take the pattern");
    assert!(pb.plays[0].gather_facts);
    let pb = parse(
        "- hosts: all
  become: yes
  become_user: deploy
  become_method: sudo
  strategy: free
  tasks:
    - command: id
      become: 'false'
      become_user: postgres
",
        "x.yml",
    )
    .unwrap();
    let play = &pb.plays[0];
    assert_eq!(play.r#become, Some(true), "'yes' is a boolean to Ansible");
    assert_eq!(play.become_user.as_deref(), Some("deploy"));
    assert_eq!(play.strategy.as_deref(), Some("free"));
    let t = tasks(play)[0];
    assert_eq!(t.r#become, Some(false), "a quoted spelling still reads");
    assert_eq!(t.become_user.as_deref(), Some("postgres"));
    let bare = parse(
        "- hosts: all
  tasks:
    - command: id
",
        "x.yml",
    )
    .unwrap();
    assert_eq!(
        (bare.plays[0].r#become, first(&bare).r#become.is_none()),
        (None, true),
        "silence at both levels leaves the decision to the variables and the defaults"
    );
}

/// Load refusals: the reference's own words where it has them, the engine's where it does not.
#[rstest]
#[case::play_attribute(
    "- hosts: all\n  nosuchplaykeyword: 1\n  tasks: []\n",
    &["'nosuchplaykeyword' is not a valid attribute for a Play"]
)]
#[case::become_method_play_doas(
    "- hosts: all\n  become: true\n  become_method: doas\n  tasks: []\n",
    &["doas", "not supported yet"]
)]
#[case::become_method_play_runas(
    "- hosts: all\n  become: true\n  become_method: runas\n  tasks: []\n",
    &["runas", "not supported yet"]
)]
#[case::become_method_task_doas(
    "- hosts: all\n  tasks:\n    - name: T\n      command: id\n      become_method: doas\n",
    &["doas", "T"]
)]
#[case::become_method_task_runas(
    "- hosts: all\n  tasks:\n    - name: T\n      command: id\n      become_method: runas\n",
    &["runas", "T"]
)]
#[case::no_module("- hosts: all\n  tasks:\n    - name: Nothing\n", &["no module"])]
#[case::two_modules(
    "- hosts: all\n  tasks:\n    - command: a\n      shell: b\n",
    &["conflicting action statements: command, shell"]
)]
#[case::not_a_list("hosts: all\n", &[])]
#[case::empty("", &[])]
#[case::unbalanced_quote("- hosts: all\n  tasks:\n    - ping: \"a='open\"\n", &[])]
#[case::when_scalar(
    "- hosts: all\n  tasks:\n    - command: \"true\"\n      when: 3\n",
    &["when"]
)]
#[case::loop_and_with_items(
    "- hosts: all\n  tasks:\n    - debug:\n      loop: [1]\n      with_items: [2]\n",
    &["loop"]
)]
#[case::two_with_loops(
    "- hosts: all\n  tasks:\n    - debug:\n      with_fileglob: ['*']\n      with_first_found: [a]\n",
    &["'with_fileglob' and 'with_first_found' cannot both be given"]
)]
#[case::hosts_int(
    "- hosts: [web, 3]\n  tasks: []\n",
    &["Hosts list contains an invalid host value: '3'"]
)]
#[case::hosts_float(
    "- hosts: [web, 3.5]\n  tasks: []\n",
    &["Hosts list contains an invalid host value: '3.5'"]
)]
#[case::hosts_sequence(
    "- hosts: [web, [1, 2]]\n  tasks: []\n",
    &["Hosts list contains an invalid host value: '[1, 2]'"]
)]
#[case::hosts_mapping(
    "- hosts: [web, {a: 1}]\n  tasks: []\n",
    &["Hosts list contains an invalid host value: '{'a': 1}'"]
)]
#[case::loop_control_unknown(
    "- hosts: all\n  tasks:\n    - debug:\n        msg: x\n      loop: [a]\n      loop_control:\n        nosuch: 1\n",
    &["'nosuch' is not a valid attribute for a LoopControl"]
)]
#[case::gather_facts_word(
    "- hosts: all\n  gather_facts: maybe\n  tasks:\n    - command: echo hi\n",
    &["Error processing keyword 'gather_facts': The value 'maybe' could not be converted to 'bool'."]
)]
#[case::gather_facts_two(
    "- hosts: all\n  gather_facts: 2\n  tasks:\n    - command: echo hi\n",
    &["The value 2 could not be converted to 'bool'."]
)]
#[case::ignore_errors_word(
    "- hosts: all\n  tasks:\n    - command: echo hi\n      ignore_errors: maybe\n",
    &["Error processing keyword 'ignore_errors': The value 'maybe' could not be converted to 'bool'."]
)]
#[case::become_word(
    "- hosts: all\n  tasks:\n    - command: echo hi\n      become: maybe\n",
    &["Error processing keyword 'become': The value 'maybe' could not be converted to 'bool'."]
)]
#[case::become_float(
    "- hosts: all\n  tasks:\n    - command: echo hi\n      become: 1.5\n",
    &["Error processing keyword 'become': The value 1.5 could not be converted to 'bool'."]
)]
#[case::become_sequence(
    "- hosts: all\n  tasks:\n    - command: echo hi\n      become: [1, 2]\n",
    &["Error processing keyword 'become': The value [1, 2] could not be converted to 'bool'."]
)]
#[case::become_mapping(
    "- hosts: all\n  tasks:\n    - command: echo hi\n      become: {a: 1}\n",
    &["Error processing keyword 'become': The value {'a': 1} could not be converted to 'bool'."]
)]
#[case::block_loop(
    "- hosts: all\n  tasks:\n    - block:\n        - command: echo hi\n      loop: x\n",
    &["'loop' is not a valid attribute for a Block"]
)]
#[case::block_register(
    "- hosts: all\n  tasks:\n    - block:\n        - command: echo hi\n      register: x\n",
    &["'register' is not a valid attribute for a Block"]
)]
#[case::block_until(
    "- hosts: all\n  tasks:\n    - block:\n        - command: echo hi\n      until: x\n",
    &["'until' is not a valid attribute for a Block"]
)]
#[case::block_module(
    "- hosts: all\n  tasks:\n    - block:\n        - command: echo hi\n      debug: msg=x\n",
    &["'debug' is not a valid attribute for a Block"]
)]
#[case::rescue_without_block(
    "- hosts: all\n  tasks:\n    - rescue:\n        - command: echo hi\n",
    &["'rescue' keyword cannot be used without 'block'"]
)]
#[case::always_without_block(
    "- hosts: all\n  tasks:\n    - always:\n        - command: echo hi\n",
    &["'always' keyword cannot be used without 'block'"]
)]
#[case::notify_element(
    "- hosts: all\n  tasks:\n    - name: t\n      command: \"true\"\n      notify: [3]\n",
    &["task 't': 'notify' must be a name or a list of names"]
)]
#[case::when_element(
    "- hosts: all\n  tasks:\n    - name: t\n      command: \"true\"\n      when: [3]\n",
    &["task 't': 'when' must be an expression or a list of expressions"]
)]
#[case::async_text(
    "- hosts: all\n  tasks:\n    - name: t\n      command: \"true\"\n      async: x\n",
    &["task 't': 'async' must be a non-negative integer"]
)]
#[case::poll_negative(
    "- hosts: all\n  tasks:\n    - name: t\n      command: \"true\"\n      poll: -1\n",
    &["task 't': 'poll' must be a non-negative integer"]
)]
fn the_load_refuses(#[case] text: &str, #[case] needles: &[&str]) {
    let err = parse(text, "x.yml").expect_err("the load must refuse");
    let got = format!("{err:#}");
    for needle in needles {
        assert!(got.contains(needle), "{needle:?} not in {got}");
    }
}

/// Every `loop_control` sub-key is read into its field, and a malformed one is refused by name.
#[test]
fn loop_control_sub_keys_are_read() {
    let task = |sub: &str| {
        format!(
            "- hosts: all
  tasks:
    - debug:
        msg: x
      loop: [a]
      loop_control:
        {sub}
"
        )
    };
    let pb = parse(
        &task(
            "loop_var: thing
        label: shown",
        ),
        "x.yml",
    )
    .unwrap();
    let t = first(&pb);
    assert_eq!(
        (t.loop_var.as_str(), t.loop_label.as_deref()),
        ("thing", Some("shown"))
    );
    assert!(t.unsupported.is_empty());
    let pb = parse(
        &task(
            "index_var: i
        extended: true
        extended_allitems: false
        pause: 0.5
        break_when: [a, b]",
        ),
        "x.yml",
    )
    .unwrap();
    let t = first(&pb);
    assert_eq!(t.index_var.as_deref(), Some("i"));
    assert_eq!(
        (&t.loop_extended, &t.loop_allitems),
        (
            &Some(serde_json::json!(true)),
            &Some(serde_json::json!(false))
        )
    );
    assert_eq!(t.loop_pause, Some(serde_json::json!(0.5)));
    assert_eq!(t.break_when, ["a", "b"]);
    assert!(t.unsupported.is_empty());
    for bad in ["index_var: [i]", "break_when: {a: b}"] {
        assert!(parse(&task(bad), "x.yml").is_err(), "{bad}");
    }
}

/// A module's argument string is read as the reference's `parse_kv` reads it: `key=value` words
/// become options and the rest is kept in `_raw_params`.
#[rstest]
#[case::set_fact("set_fact: owner=root state=here", "set_fact", &[("owner", "root"), ("state", "here")])]
#[case::free_form_alias("ansible.legacy.shell: echo a=b", "ansible.legacy.shell", &[("_raw_params", "echo a=b")])]
#[case::unknown_builtin("ansible.builtin.nosuch: echo a", "ansible.builtin.nosuch", &[])]
#[case::library_stray("my_library_module: a=1 stray", "my_library_module", &[("_raw_params", "stray"), ("a", "1")])]
#[case::file("file: path=/tmp/x state=touch", "file", &[("path", "/tmp/x"), ("state", "touch")])]
#[case::collection("community.general.ufw: rule=allow port=22", "community.general.ufw", &[("port", "22"), ("rule", "allow")])]
#[case::windows_free_text("ansible.windows.win_shell: Get-Service foo", "ansible.windows.win_shell", &[("_raw_params", "Get-Service foo")])]
#[case::template_words("ns.coll.mod: port={{ x }} {{ a b }}", "ns.coll.mod", &[("_raw_params", "{{ a b }}"), ("port", "{{ x }}")])]
#[case::open_quote("ns.coll.mod: a='open", "ns.coll.mod", &[("_raw_params", "a='open")])]
#[case::builtin_stray_word("ping: data=x foo", "ping", &[("_raw_params", "foo"), ("data", "x")])]
#[case::jinja_block_kept_whole(
    "copy: dest={{ base ~ '/x' }} mode=0644 content=u\"v w\"",
    "copy",
    &[("content", "u\"v w\""), ("dest", "{{ base ~ '/x' }}"), ("mode", "0644")]
)]
fn raw_arguments_are_split_as_the_reference_splits_them(
    #[case] line: &str,
    #[case] module: &str,
    #[case] expect: &[(&str, &str)],
) {
    let pb = parse(&format!("- hosts: all\n  tasks:\n    - {line}\n"), "x.yml")
        .unwrap_or_else(|e| panic!("{line}: {e:#}"));
    let task = first(&pb);
    assert_eq!(task.module, module);
    let mut got: Vec<(&str, &str)> = task
        .args
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str().unwrap_or_default()))
        .collect();
    got.sort_unstable();
    assert_eq!(got, expect, "{line}");
}

/// `action:` and `local_action:` read as the module's own key would, in every spelling the
/// reference accepts; `local_action` runs on `localhost` whatever `delegate_to` says.
#[rstest]
#[case::string("action: command hostname", "command", &[("_raw_params", "hostname")], None)]
#[case::string_options("action: copy src=a dest=b", "copy", &[("dest", "b"), ("src", "a")], None)]
#[case::mapping("action: {module: command, cmd: hostname}", "command", &[("cmd", "hostname")], None)]
#[case::mapping_args("action: {module: command, args: {cmd: hostname}}", "command", &[("cmd", "hostname")], None)]
#[case::args_beside("action: command\n      args: {cmd: hostname}", "command", &[("cmd", "hostname")], None)]
#[case::local("local_action: command hostname", "command", &[("_raw_params", "hostname")], Some("localhost"))]
#[case::local_mapping("local_action: {module: command, cmd: hostname}", "command", &[("cmd", "hostname")], Some("localhost"))]
#[case::local_beats_delegate("local_action: command hostname\n      delegate_to: h1", "command", &[("_raw_params", "hostname")], Some("localhost"))]
fn an_action_statement_names_the_module_in_its_value(
    #[case] line: &str,
    #[case] module: &str,
    #[case] expect: &[(&str, &str)],
    #[case] delegate: Option<&str>,
) {
    let pb = parse(&format!("- hosts: all\n  tasks:\n    - {line}\n"), "x.yml")
        .unwrap_or_else(|e| panic!("{line}: {e:#}"));
    let task = first(&pb);
    assert_eq!(
        (task.module.as_str(), task.delegate_to.as_deref()),
        (module, delegate)
    );
    assert!(task.unsupported.is_empty());
    let mut got: Vec<(&str, &str)> = task
        .args
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str().unwrap_or_default()))
        .collect();
    got.sort_unstable();
    assert_eq!(got, expect, "{line}");
}

/// The statements the reference refuses, in its words.
#[rstest]
#[case::module_equals(
    "action: module=command cmd=hostname",
    "Cannot resolve 'module=command' to an action or module."
)]
#[case::no_module("action: {cmd: hostname}", "no module/action detected in task.")]
#[case::empty("action: ''", "no module/action detected in task.")]
#[case::two_modules(
    "local_action: command hostname\n      command: hostname",
    "conflicting action statements: command, command"
)]
#[case::both(
    "action: command a\n      local_action: command b",
    "action and local_action are mutually exclusive"
)]
fn an_action_statement_the_reference_refuses_is_refused(#[case] line: &str, #[case] why: &str) {
    let err = parse(&format!("- hosts: all\n  tasks:\n    - {line}\n"), "x.yml").expect_err(line);
    assert!(format!("{err:#}").contains(why), "{line}: {err:#}");
}

const KEYWORDS: &str = r#"
- name: Keywords
  hosts: all
  gather_facts: false
  vars:
    port: 80
    greeting: "hello {{ name }}"
  vars_files:
    - vars/common.yml
    - "vars/{{ env }}.yml"
  tasks:
    - name: Conditional
      command: echo {{ port }}
      when: port > 1
      register: out
      vars:
        local: 1
      timeout: 5
    - name: Several conditions
      command: "true"
      when:
        - out is defined
        - out.rc == 0
      changed_when: false
      failed_when:
        - out.rc != 0
        - "'oops' in out.stdout"
    - name: Looping
      debug:
        msg: "{{ item }}"
      loop: "{{ ['a', 'b'] }}"
    - name: Legacy loop with control
      debug:
        msg: "{{ server.name }}"
      with_items:
        - {name: a}
        - {name: b}
      loop_control:
        loop_var: server
        label: "{{ server.name }}"
    - name: Facts
      set_fact:
        computed: "{{ port + 1 }}"
"#;

#[test]
fn play_and_task_keywords_are_read_raw() {
    let pb = parse(KEYWORDS, "k.yml").unwrap();
    let play = &pb.plays[0];
    assert_eq!(play.vars["port"], serde_json::json!(80));
    assert_eq!(play.vars["greeting"], serde_json::json!("hello {{ name }}"));
    assert_eq!(play.vars_files, ["vars/common.yml", "vars/{{ env }}.yml"]);
    let t = tasks(play);
    assert_eq!(t[0].when, ["port > 1"]);
    assert_eq!(t[0].register.as_deref(), Some("out"));
    assert_eq!(t[0].vars["local"], serde_json::json!(1));
    assert_eq!(t[0].timeout, Some(5));
    assert_eq!(
        t[0].args["_raw_params"], "echo {{ port }}",
        "arguments stay untemplated here"
    );

    assert_eq!(t[1].when, ["out is defined", "out.rc == 0"]);
    assert_eq!(t[1].changed_when, ["False"]);
    assert_eq!(t[1].failed_when, ["out.rc != 0", "'oops' in out.stdout"]);

    assert_eq!(t[2].loop_items, Some(serde_json::json!("{{ ['a', 'b'] }}")));
    assert_eq!(t[2].loop_var, "item");
    assert!(t[2].loop_label.is_none());

    assert_eq!(
        t[3].loop_items,
        Some(serde_json::json!([{"name": "a"}, {"name": "b"}]))
    );
    assert_eq!(t[3].loop_var, "server");
    assert_eq!(t[3].loop_label.as_deref(), Some("{{ server.name }}"));
    assert!(t[3].with_items && !t[2].with_items);

    assert_eq!(t[4].module, "set_fact");
    assert_eq!(t[4].args["computed"], "{{ port + 1 }}");
}

/// `when: false` and `1`/`0` are booleans to the reference; a template stays as written.
#[test]
fn booleans_are_read_the_way_the_reference_reads_them() {
    let pb = parse(
        "- hosts: all\n  gather_facts: 1\n  tasks:\n    - command: \"true\"\n      when: false\n      ignore_errors: 0\n",
        "x.yml",
    )
    .unwrap();
    assert!(pb.plays[0].gather_facts && !first(&pb).ignores_errors());
    assert_eq!(first(&pb).when, ["False"]);
    let pb = parse(
        "- hosts: all\n  tasks:\n    - command: \"false\"\n      ignore_errors: \"{{ ansible_check_mode }}\"\n    - block:\n        - command: echo hi\n      ignore_errors: \"{{ true }}\"\n",
        "x.yml",
    )
    .unwrap();
    let TaskOrBlock::Task(t) = &pb.plays[0].tasks[0] else {
        panic!("a task");
    };
    assert_eq!(
        t.ignore_errors,
        Some(Flag::Template("{{ ansible_check_mode }}".into()))
    );
    assert!(
        !t.ignores_errors(),
        "an unrendered template ignores nothing"
    );
    let TaskOrBlock::Block(b) = &pb.plays[0].tasks[1] else {
        panic!("a block");
    };
    assert_eq!(
        b.keywords.ignore_errors,
        Some(Flag::Template("{{ true }}".into()))
    );
}

/// A block takes its three sections and the keywords its tasks inherit, and nests; an empty
/// block loads.
#[test]
fn a_block_takes_its_sections_and_its_inherited_keywords() {
    let pb = parse(
        "- hosts: all\n  tasks:\n    - name: Grouped\n      block:\n        - command: echo hi\n        - block:\n            - command: echo deeper\n      rescue:\n        - command: echo sorry\n      always:\n        - command: echo done\n      when: ready\n      become: true\n      ignore_errors: true\n      vars: {a: 1}\n    - block: []\n",
        "x.yml",
    )
    .unwrap();
    let TaskOrBlock::Block(b) = &pb.plays[0].tasks[0] else {
        panic!("a block");
    };
    assert_eq!(b.body.len(), 2);
    assert!(matches!(b.body[1], TaskOrBlock::Block(_)), "blocks nest");
    assert_eq!(b.rescue.len(), 1);
    assert_eq!(b.always.len(), 1);
    assert_eq!(b.keywords.name, "Grouped");
    assert_eq!(b.keywords.when, ["ready"]);
    assert_eq!(b.keywords.r#become, Some(true));
    assert_eq!(b.keywords.ignore_errors, Some(Flag::Fixed(true)));
    assert_eq!(b.keywords.vars["a"], serde_json::json!(1));
    assert!(
        b.keywords.module.is_empty(),
        "a block names no module of its own"
    );
    let TaskOrBlock::Block(empty) = &pb.plays[0].tasks[1] else {
        panic!("a block");
    };
    assert!(empty.body.is_empty());
}

/// Shapes the loader keeps for the pre-flight: a `set_fact` key
/// typed by YAML, and a `meta` action whatever it names.
#[test]
fn what_the_loader_keeps_for_the_preflight() {
    let pb = parse(
        "- hosts: all\n  tasks:\n    - set_fact: {1: a, 1.5: b, 2.0: f, true: c, ~: d, k: e}\n",
        "x.yml",
    )
    .unwrap();
    let keys: Vec<&str> = first(&pb).args.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "{{ 1 }}",
            "{{ 1.5 }}",
            "{{ 2.0 }}",
            "{{ True }}",
            "{{ None }}",
            "k"
        ]
    );
    for action in ["noop", "end_play", "nosuchaction"] {
        let pb = parse(
            &format!("- hosts: all\n  tasks:\n    - meta: {action}\n"),
            "x.yml",
        )
        .unwrap_or_else(|e| panic!("{action}: {e:#}"));
        let t = first(&pb);
        assert_eq!(t.module, "meta");
        assert_eq!(t.args["_raw_params"], serde_json::json!(action));
    }
}
