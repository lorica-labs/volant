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
#[case::no_module(
    "- hosts: all\n  tasks:\n    - name: Nothing\n",
    &["no module/action detected in task."]
)]
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
    &["duplicate loop in task: first_found"]
)]
#[case::with_after_loop(
    "- hosts: all\n  tasks:\n    - debug:\n      loop: [1]\n      with_list: [2]\n",
    &["duplicate loop in task: list"]
)]
#[case::with_without_value(
    "- hosts: all\n  tasks:\n    - debug:\n      with_flattened:\n",
    &["you must specify a value when using with_flattened"]
)]
#[case::with_alone(
    "- hosts: all\n  tasks:\n    - with_xyz: [1]\n",
    &["no module/action detected in task."]
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
#[case::apply_list(
    "- hosts: all\n  tasks:\n    - include_tasks:\n        file: a.yml\n        apply: [tags]\n",
    &["Expected a dict for apply but got a list instead"]
)]
#[case::apply_text(
    "- hosts: all\n  tasks:\n    - include_role:\n        name: r\n        apply: tags\n",
    &["Expected a dict for apply but got a string instead"]
)]
#[case::apply_null(
    "- hosts: all\n  tasks:\n    - include_tasks:\n        file: a.yml\n        apply:\n",
    &["Expected a dict for apply but got null instead"]
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
#[case::command_creates("command: echo hi creates=/etc/passwd", "command", &[("_raw_params", "echo hi"), ("creates", "/etc/passwd")])]
#[case::command_removes("ansible.builtin.command: echo hi removes=/x", "ansible.builtin.command", &[("_raw_params", "echo hi"), ("removes", "/x")])]
#[case::shell_chdir("shell: pwd chdir=/etc", "shell", &[("_raw_params", "pwd"), ("chdir", "/etc")])]
#[case::shell_executable("ansible.legacy.shell: echo $0 executable=/bin/bash", "ansible.legacy.shell", &[("_raw_params", "echo $0"), ("executable", "/bin/bash")])]
#[case::command_quoted_value("command: echo hi creates=\"/tmp/a b\"", "command", &[("_raw_params", "echo hi"), ("creates", "/tmp/a b")])]
#[case::command_templated_value("command: echo hi creates={{ a ~ ' x' }} chdir={{ d }}", "command", &[("_raw_params", "echo hi"), ("chdir", "{{ d }}"), ("creates", "{{ a ~ ' x' }}")])]
#[case::command_other_word("command: echo foo=bar cmd=x", "command", &[("_raw_params", "echo foo=bar cmd=x")])]
#[case::command_escaped_equals("command: echo a\\=b creates=/x", "command", &[("_raw_params", "echo a=b"), ("creates", "/x")])]
#[case::command_repeated_option("command: echo creates=/a creates=/b", "command", &[("_raw_params", "echo"), ("creates", "/b")])]
#[case::raw_executable("raw: echo $0 creates=/x executable=/bin/bash", "raw", &[("_raw_params", "echo $0"), ("creates", "/x"), ("executable", "/bin/bash")])]
#[case::script_options("script: s.sh a=1 removes=/x", "script", &[("_raw_params", "s.sh a=1"), ("removes", "/x")])]
#[case::win_shell_chdir("ansible.windows.win_shell: dir chdir=C:/", "ansible.windows.win_shell", &[("_raw_params", "dir"), ("chdir", "C:/")])]
#[case::command_line_beats_args(
    "command: echo hi creates=/a\n      args: {creates: /b, chdir: /c}",
    "command",
    &[("_raw_params", "echo hi"), ("chdir", "/c"), ("creates", "/a")]
)]
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

/// A command line that does not split fails the load, as it fails the reference's: an unbalanced
/// quote or Jinja block is an error for `command` and `shell`, not a word of the command.
#[rstest]
#[case::quote("command: echo it's")]
#[case::double_quote("shell: echo \"a b")]
#[case::block("command: echo {{ a")]
fn an_unbalanced_command_line_fails_the_load(#[case] line: &str) {
    let err = parse(&format!("- hosts: all\n  tasks:\n    - {line}\n"), "x.yml").expect_err(line);
    assert!(
        format!("{err:#}").contains("failed at splitting arguments"),
        "{line}: {err:#}"
    );
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
#[case::module_equals("action: module=command cmd=hostname", "module=command", &[], None)]
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

/// `apply:` of an include statement is read as a block's keywords. A key a block does not have
/// is kept as the sentence the reference stops the run with, and an empty mapping is no `apply`.
#[rstest]
#[case::tags("include_tasks", "tags: [t]", "keywords")]
#[case::several(
    "include_role",
    "{tags: [t], become: true, when: x, vars: {a: 1}}",
    "keywords"
)]
#[case::empty("include_tasks", "{}", "none")]
#[case::unknown_key(
    "include_tasks",
    "{nosuch: 1}",
    "'nosuch' is not a valid attribute for a Block"
)]
#[case::task_only_key(
    "include_role",
    "{register: z}",
    "'register' is not a valid attribute for a Block"
)]
#[case::bad_value("include_tasks", "{vars: 5}", "'vars' must be a mapping")]
#[case::not_an_include("debug", "none", "none")]
fn apply_is_read_as_the_keywords_of_a_block(
    #[case] module: &str,
    #[case] apply: &str,
    #[case] expected: &str,
) {
    use crate::playbook::Apply;
    let apply = if apply.starts_with('{') || apply == "none" {
        apply.to_string()
    } else {
        format!("{{{apply}}}")
    };
    let line = match module {
        "debug" => "debug: {msg: x}".to_string(),
        "include_role" => format!("include_role: {{name: r, apply: {apply}}}"),
        _ => format!("{module}: {{file: a.yml, apply: {apply}}}"),
    };
    let pb = parse(&format!("- hosts: all\n  tasks:\n    - {line}\n"), "x.yml").unwrap();
    match (&first(&pb).apply, expected) {
        (None, "none") => {}
        (Some(Apply::Keywords(k)), "keywords") => {
            assert!(!k.tags.is_empty(), "{k:?}");
        }
        (Some(Apply::Refused(sentence)), want) => assert!(sentence.contains(want), "{sentence}"),
        (got, want) => panic!("{want}: {got:?}"),
    }
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

/// A playbook of `files` on disk, loaded by its first entry.
fn load_files(files: &[(&str, &str)]) -> anyhow::Result<Playbook> {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in files {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    load(&dir.path().join(files[0].0))
}

const SUB: &str =
    "- hosts: all\n  tags: [own]\n  vars: {v: own, w: 1}\n  tasks:\n    - debug: {msg: x}\n";

/// An `import_playbook` entry hands its tags, variables and conditions to every imported play;
/// a `tags` among the variables is taken out, and the fact gathering keeps its own conditions.
#[test]
fn an_import_playbook_entry_hands_when_tags_and_vars_to_the_imported_plays() {
    let pb = load_files(&[
        (
            "main.yml",
            "- import_playbook: sub.yml\n  when: [a, b]\n  tags: x,y\n  vars: {v: imp, tags: z}\n",
        ),
        ("sub.yml", SUB),
    ])
    .unwrap();
    let play = &pb.plays[0];
    assert_eq!(play.tags, ["own", "x", "y", "z"]);
    assert_eq!(play.vars["v"], "imp");
    assert_eq!(play.vars["w"], 1);
    assert!(!play.vars.contains_key("tags"));
    assert_eq!(play.import_when, ["a", "b"]);
    assert_eq!(play.gather_when, ["a", "b"]);
}

/// The names of a `tags` among the vars join only the tags a play wrote, as the reference does.
#[test]
fn tags_among_the_vars_of_an_import_need_a_tagged_play() {
    let sub = "- hosts: all\n  tasks:\n    - debug: {msg: x}\n";
    let pb = load_files(&[
        (
            "main.yml",
            "- import_playbook: sub.yml\n  vars: {tags: z}\n",
        ),
        ("sub.yml", sub),
    ])
    .unwrap();
    assert!(pb.plays[0].tags.is_empty());
    assert!(!pb.plays[0].vars.contains_key("tags"));
}

/// Nested imports: every condition reaches the tasks, outermost first, and the gathering takes
/// the outermost one only.
#[test]
fn nested_imports_put_the_outer_condition_first_and_gather_on_the_outermost() {
    let pb = load_files(&[
        ("main.yml", "- import_playbook: mid.yml\n  when: outer\n"),
        ("mid.yml", "- import_playbook: sub.yml\n  when: inner\n"),
        ("sub.yml", SUB),
    ])
    .unwrap();
    assert_eq!(pb.plays[0].import_when, ["outer", "inner"]);
    assert_eq!(pb.plays[0].gather_when, ["outer"]);
    let pb = load_files(&[
        ("main.yml", "- import_playbook: mid.yml\n"),
        ("mid.yml", "- import_playbook: sub.yml\n  when: inner\n"),
        ("sub.yml", SUB),
    ])
    .unwrap();
    assert_eq!(pb.plays[0].gather_when, ["inner"]);
}

/// What an import entry is refused for, in the reference's words and with its exit code.
#[rstest]
#[case::unknown(
    "  hosts: h1\n",
    4,
    "'hosts' is not a valid attribute for a PlaybookInclude"
)]
#[case::unknown_gather(
    "  gather_facts: false\n",
    4,
    "'gather_facts' is not a valid attribute for a PlaybookInclude"
)]
#[case::vars_list(
    "  vars: [1]\n",
    4,
    "Vars in a PlaybookInclude must be specified as a dictionary."
)]
#[case::tags_mapping("  tags: {a: 1}\n", 1, "tags must be specified as a list")]
#[case::templated_tags(
    "  tags: ['{{ t }}']\n",
    4,
    "'tags' of an 'import_playbook' holds a template"
)]
#[case::templated_tags_string("  tags: '{{ t }}'\n", 4, "holds a template")]
#[case::statement_in_tags("  tags: ['{% if t %}a{% endif %}']\n", 4, "holds a template")]
#[case::comment_in_tags("  tags: ['a{# b #}']\n", 4, "holds a template")]
#[case::templated_tags_in_vars("  vars: {tags: '{{ t }}'}\n", 4, "holds a template")]
#[case::tags_null("  tags:\n", 1, "tags must be specified as a list")]
#[case::nested_tags(
    "  tags: [a, [b]]\n",
    4,
    "Keyword 'tags' items must be of type 'str' or 'int', not 'list'"
)]
#[case::templated_when("  when: '{{ x }}'\n", 4, "holds a template")]
fn an_import_entry_with_a_bad_keyword_is_refused(
    #[case] keywords: &str,
    #[case] code: i32,
    #[case] message: &str,
) {
    let main = format!("- import_playbook: sub.yml\n{keywords}");
    let err = load_files(&[("main.yml", &main), ("sub.yml", SUB)]).unwrap_err();
    let refusal = err
        .chain()
        .find_map(|e| e.downcast_ref::<crate::stats::Refusal>())
        .expect("a refusal with an exit code");
    assert_eq!(refusal.code, code, "{err:#}");
    assert!(format!("{err:#}").contains(message), "{err:#}");
}

/// The attributes `PlaybookInclude` has and does nothing with are accepted.
#[test]
fn an_import_entry_accepts_the_keywords_it_does_not_pass_on() {
    load_files(&[
        (
            "main.yml",
            "- name: n\n  import_playbook: sub.yml\n  ignore_errors: true\n  become: true\n  environment: {A: b}\n  no_log: true\n  run_once: true\n",
        ),
        ("sub.yml", SUB),
    ])
    .unwrap();
}

/// The typed value and the waiting template of one keyword, on a task, a block or a role entry.
fn kept(task: &PlayTask, key: &str) -> (serde_json::Value, Option<String>) {
    use serde_json::json;
    let d = &task.deferred;
    match key {
        "become" => (json!(task.r#become), d.r#become.clone()),
        "no_log" => (json!(task.no_log), d.no_log.clone()),
        "run_once" => (json!(task.run_once), d.run_once.clone()),
        "any_errors_fatal" => (json!(task.any_errors_fatal), d.any_errors_fatal.clone()),
        "ignore_unreachable" => (json!(task.ignore_unreachable), d.ignore_unreachable.clone()),
        "delegate_facts" => (json!(task.delegate_facts), d.delegate_facts.clone()),
        "timeout" => (json!(task.timeout), d.timeout.clone()),
        "async" => (json!(task.async_val), d.async_val.clone()),
        "poll" => (json!(task.poll), d.poll.clone()),
        other => panic!("{other}"),
    }
}

/// The same for a play.
fn kept_by_play(play: &Play, key: &str) -> (serde_json::Value, Option<String>) {
    use serde_json::json;
    let d = &play.deferred;
    match key {
        "become" => (json!(play.r#become), d.r#become.clone()),
        "no_log" => (json!(play.no_log), d.no_log.clone()),
        "run_once" => (json!(play.run_once), d.run_once.clone()),
        "any_errors_fatal" => (json!(play.any_errors_fatal), d.any_errors_fatal.clone()),
        "ignore_unreachable" => (json!(play.ignore_unreachable), d.ignore_unreachable.clone()),
        "timeout" => (json!(play.timeout), d.timeout.clone()),
        "gather_facts" => (
            json!(play.gather_written.then_some(play.gather_facts)),
            d.gather_facts.clone(),
        ),
        "force_handlers" => (json!(play.force_handlers), d.force_handlers.clone()),
        other => panic!("{other}"),
    }
}

/// A template waits in `deferred` with its typed field unset, wherever it is written; a literal
/// fills the typed field. A play has no `delegate_facts`, `async` or `poll`; a block no `async`.
#[rstest]
#[case::escalation("become", "true", serde_json::json!(true))]
#[case::no_log("no_log", "true", serde_json::json!(true))]
#[case::run_once("run_once", "true", serde_json::json!(true))]
#[case::any_errors_fatal("any_errors_fatal", "true", serde_json::json!(true))]
#[case::ignore_unreachable("ignore_unreachable", "true", serde_json::json!(true))]
#[case::delegate_facts("delegate_facts", "true", serde_json::json!(true))]
#[case::timeout("timeout", "3", serde_json::json!(3))]
#[case::async_keyword("async", "3", serde_json::json!(3))]
#[case::poll("poll", "3", serde_json::json!(3))]
fn a_keyword_that_may_be_a_template_is_kept_for_the_task_that_runs(
    #[case] key: &str,
    #[case] literal: &str,
    #[case] typed: serde_json::Value,
) {
    let layers: [(&str, &str); 4] = [
        ("task", "tasks:\n    - command: echo\n      KEY: VALUE\n"),
        (
            "block",
            "tasks:\n    - block:\n        - command: echo\n      KEY: VALUE\n",
        ),
        ("role", "roles:\n    - role: r\n      KEY: VALUE\n"),
        ("play", "KEY: VALUE\n  tasks: []\n"),
    ];
    for (layer, body) in layers {
        let allowed = match key {
            "delegate_facts" => layer != "play",
            "async" | "poll" => layer == "task",
            _ => true,
        };
        if !allowed {
            continue;
        }
        for (value, want_typed, want_template) in [
            (
                "\"{{ v }}\"",
                serde_json::Value::Null,
                Some("{{ v }}".to_string()),
            ),
            (literal, typed.clone(), None),
        ] {
            let text = format!(
                "- hosts: all\n  {}",
                body.replace("KEY", key).replace("VALUE", value)
            );
            let pb = parse(&text, "x.yml").unwrap_or_else(|e| panic!("{layer} {key}: {e:#}"));
            let play = &pb.plays[0];
            let (got_typed, got_template) = match layer {
                "task" => kept(first(&pb), key),
                "block" => {
                    let TaskOrBlock::Block(b) = &play.tasks[0] else {
                        panic!("a block");
                    };
                    kept(&b.keywords, key)
                }
                "role" => kept(&play.roles[0].keywords, key),
                _ => kept_by_play(play, key),
            };
            // `poll` and `async` read as unset when absent, which `kept` shows as null or 0.
            let got_typed = if got_typed == serde_json::json!(0) && want_typed.is_null() {
                serde_json::Value::Null
            } else {
                got_typed
            };
            assert_eq!(
                (got_typed, got_template),
                (want_typed, want_template),
                "{layer} {key}: {value}"
            );
        }
    }
}

/// `gather_facts` and `force_handlers` of a play wait the same way, and a play that writes
/// `gather_facts` as a template counts as having written it.
#[test]
fn a_play_keeps_a_templated_gather_facts_and_force_handlers_for_when_it_starts() {
    let pb = parse(
        "- hosts: all\n  gather_facts: \"{{ g }}\"\n  force_handlers: \"{{ f }}\"\n  tasks: []\n",
        "x.yml",
    )
    .unwrap();
    let play = &pb.plays[0];
    assert_eq!(play.deferred.gather_facts.as_deref(), Some("{{ g }}"));
    assert_eq!(play.deferred.force_handlers.as_deref(), Some("{{ f }}"));
    assert!(play.gather_facts, "the play gathers until it renders to no");
    assert!(play.gather_written && play.force_handlers.is_none());
}

/// A waiting `run_once` still meets the hosts in front of it, and an `any_errors_fatal` is fatal
/// whatever it renders to: the strategy reads both as written.
#[test]
fn a_templated_run_once_and_any_errors_fatal_show_before_they_are_rendered() {
    let pb = parse(
        "- hosts: all\n  tasks:\n    - command: echo\n      run_once: \"{{ x }}\"\n    - command: echo\n      any_errors_fatal: \"{{ x }}\"\n    - command: echo\n",
        "x.yml",
    )
    .unwrap();
    let t = tasks(&pb.plays[0]);
    assert!(
        t[0].barrier() && !t[0].runs_once(),
        "a barrier that does not yet run once"
    );
    assert!(t[1].fails_all() && t[1].barrier());
    assert!(!t[2].barrier() && !t[2].fails_all());
}

/// A literal that is no boolean is still refused at load, in the reference's words, and so is a
/// negative or fractional number of seconds; only a template waits.
#[rstest]
#[case::no_log(
    "no_log: maybe",
    "Error processing keyword 'no_log': The value 'maybe' could not be converted to 'bool'."
)]
#[case::escalation(
    "become: maybe",
    "Error processing keyword 'become': The value 'maybe' could not be converted to 'bool'."
)]
#[case::run_once(
    "run_once: 2",
    "Error processing keyword 'run_once': The value 2 could not be converted to 'bool'."
)]
#[case::timeout("timeout: -1", "'timeout' must be a non-negative integer")]
#[case::async_keyword("async: abc", "'async' must be a non-negative integer")]
fn a_literal_that_is_not_what_the_keyword_takes_is_still_refused_at_load(
    #[case] line: &str,
    #[case] says: &str,
) {
    let text = format!("- hosts: all\n  tasks:\n    - command: echo\n      {line}\n");
    let err = parse(&text, "x.yml").expect_err("refused");
    assert!(format!("{err:#}").contains(says), "{err:#}");
}
