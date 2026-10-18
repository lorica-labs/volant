// SPDX-License-Identifier: GPL-3.0-or-later
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};
use volant_protocol::modules::is_builtin;

use super::boundaries::{
    FACT_MODULES, calls_lookup, is_boundary, reads_across_hosts, strict, writes_facts,
};
use super::{Definitions, first_batch_id, mark_boundaries, plugin_step};
use crate::compile::{Compiled, Step, StepKind};
use crate::playbook::PlayTask;
use crate::testing::{scratch, task, write};
use crate::vars::VarStore;
use rstest::rstest;
use serde_json::json;

/// Each host's run of each play numbers its batches in a range of its own.
#[test]
fn batch_ids_start_in_a_range_of_their_own() {
    let (a, b) = (first_batch_id(), first_batch_id());
    assert!(b >= a + (1 << 32), "{a} {b}");
}

/// A plugin batch is exactly one step, and anything else ends the host naming the steps.
#[test]
fn a_plugin_batch_holds_its_one_step_and_nothing_else() {
    assert_eq!(plugin_step(&[(4, ())]), Ok(&(4, ())));
    let err = plugin_step(&[(4, ()), (5, ())]).expect_err("two steps");
    assert!(err.contains("[4, 5]"), "{err}");
    plugin_step::<()>(&[]).expect_err("no step");
}

fn with_arg(module: &str, key: &str, value: &str) -> PlayTask {
    let mut t = task(module);
    t.args.insert(key.into(), json!(value));
    t
}

/// Whether the text scan finds a cross-host read in each place a task can spell one.
#[rstest]
#[case::nothing(|_: &mut PlayTask| {}, false)]
#[case::arguments(|t: &mut PlayTask| { t.args.insert("cmd".into(), json!("echo {{ hostvars['a'].x }}")); }, true)]
#[case::when(|t: &mut PlayTask| t.when = vec!["inventory_hostname in ansible_play_hosts".into()], true)]
#[case::name(|t: &mut PlayTask| t.name = "count {{ ansible_play_batch | length }}".into(), true)]
#[case::vars(|t: &mut PlayTask| { t.vars.insert("peer".into(), json!("{{ hostvars['a'].y }}")); }, true)]
#[case::loop_items(|t: &mut PlayTask| t.loop_items = Some(json!("{{ ansible_play_hosts }}")), true)]
#[case::changed_when(|t: &mut PlayTask| t.changed_when = vec!["hostvars['a'].rc == 0".into()], true)]
#[case::failed_when(|t: &mut PlayTask| t.failed_when = vec!["play_hosts | length > 1".into()], true)]
#[case::until(|t: &mut PlayTask| t.until = vec!["hostvars['a'].ready".into()], true)]
#[case::delegate_to(|t: &mut PlayTask| t.delegate_to = Some("{{ hostvars['a'].peer }}".into()), true)]
#[case::environment(|t: &mut PlayTask| t.environment = vec![json!({"PEER": "{{ hostvars['a'].ip }}"})], true)]
#[case::templated_ignore_errors(|t: &mut PlayTask| {
    t.ignore_errors = Some(crate::playbook::Flag::Template(
        "{{ hostvars[groups.primary[0]].degraded | default(false) }}".into(),
    ));
}, true)]
#[case::static_list_alone(|t: &mut PlayTask| { t.args.insert("msg".into(), json!("{{ ansible_play_hosts_all | join(',') }}")); }, false)]
#[case::live_list_beside_static(|t: &mut PlayTask| {
    t.args.insert(
        "msg".into(),
        json!("{{ ansible_play_hosts | join(',') }} of {{ ansible_play_hosts_all | join(',') }}"),
    );
}, true)]
#[case::play_batch(|t: &mut PlayTask| { t.args.insert("msg".into(), json!("{{ ansible_play_batch }}")); }, true)]
#[case::run_once_spells_nothing(|t: &mut PlayTask| t.run_once = Some(true), false)]
#[case::delegated_spells_nothing(|t: &mut PlayTask| { t.delegate_to = Some("h3".into()); t.delegate_facts = Some(true); }, false)]
fn a_cross_host_read_is_found_wherever_the_task_spells_it(
    #[case] edit: fn(&mut PlayTask),
    #[case] found: bool,
) {
    let mut t = task("command");
    edit(&mut t);
    assert_eq!(reads_across_hosts(&t), found);
}

/// A boundary is declared by the keyword table or found in the text, and neither covers the other.
#[rstest]
#[case::plain(|_: &mut PlayTask| {}, false, false)]
#[case::run_once(|t: &mut PlayTask| t.run_once = Some(true), true, false)]
#[case::run_once_false(|t: &mut PlayTask| t.run_once = Some(false), false, false)]
#[case::delegated(|t: &mut PlayTask| t.delegate_to = Some("h3".into()), false, false)]
#[case::hostvars_read(|t: &mut PlayTask| { t.args.insert("cmd".into(), json!("echo {{ hostvars['a'].x }}")); }, false, true)]
fn a_keyword_declares_a_barrier_the_text_does_not(
    #[case] edit: fn(&mut PlayTask),
    #[case] barrier: bool,
    #[case] reads: bool,
) {
    let mut t = task("command");
    edit(&mut t);
    assert_eq!(t.barrier(), barrier);
    assert_eq!(reads_across_hosts(&t), reads);
}

/// A task that can write facts ends its batch; a module this release cannot vouch for does too.
#[rstest]
#[case("setup", true)]
#[case("ansible.builtin.setup", true)]
#[case("ansible.legacy.hostname", true)]
#[case("hostname", true)]
#[case("gather_facts", true)]
#[case("dnf", true)]
#[case("mount_facts", true)]
#[case("package_facts", true)]
#[case("ansible.builtin.service_facts", true)]
#[case("getent", true)]
#[case("community.general.listen_ports_facts", true)]
#[case("kubernetes.core.k8s", true)]
#[case("my_role_library_module", true)]
#[case("apt", false)]
#[case("ansible.builtin.stat", false)]
#[case("template", false)]
#[case("command", false)]
fn a_task_that_writes_facts_ends_its_batch(#[case] module: &str, #[case] writes: bool) {
    assert_eq!(writes_facts(&task(module)), writes);
    assert!(
        FACT_MODULES.iter().all(|m| is_builtin(m)),
        "a fact module is not a builtin"
    );
}

/// A lookup reads the controller when its task renders, so the task starts its own batch.
#[rstest]
#[case::file(|t: &mut PlayTask| { t.args.insert("line".into(), json!("{{ lookup('file', '/tmp/k.pub') }}")); }, true)]
#[case::query(|t: &mut PlayTask| { t.args.insert("line".into(), json!("{{ query('pipe', 'date') | first }}")); }, true)]
#[case::short_name(|t: &mut PlayTask| { t.args.insert("line".into(), json!("{{ q('env', 'HOME') }}")); }, true)]
#[case::bare_condition(|t: &mut PlayTask| t.when = vec!["lookup('env', 'CI') == ''".into()], true)]
#[case::with_loop(|t: &mut PlayTask| t.loop_with = Some("fileglob".into()), true)]
#[case::only_a_word(|t: &mut PlayTask| { t.args.insert("line".into(), json!("{{ lookup_table }} lookup q")); }, false)]
fn a_lookup_is_found_wherever_the_task_calls_it(
    #[case] edit: fn(&mut PlayTask),
    #[case] found: bool,
) {
    let mut t = task("lineinfile");
    edit(&mut t);
    assert_eq!(calls_lookup(&t), found);
}

fn step(task: PlayTask) -> Step {
    Step {
        kind: StepKind::Task,
        task,
        block: None,
        section: crate::compile::Section::Body,
        role: None,
        origin: Arc::default(),
        include_params: None,
        hosts: None,
    }
}

/// A list of plain steps, marked against no variable definitions at all.
fn marked(tasks: Vec<PlayTask>) -> Compiled {
    let mut c = Compiled {
        steps: tasks.into_iter().map(step).collect(),
        ..Compiled::default()
    };
    mark_boundaries(&mut c, &Definitions::default(), Path::new("."));
    c
}

/// What `is_boundary` answers for each step under `batching`.
fn boundaries(c: &Compiled) -> Vec<bool> {
    (0..c.steps.len())
        .map(|pos| is_boundary(c, pos, false))
        .collect()
}

/// `site.yml` under `dir` compiled with `roles/` beside it, gathering nothing, and marked
/// against `base`: the task names in order, each with its answer under `batching`.
fn marked_in(dir: &Path, base: &Definitions) -> Vec<(String, bool)> {
    let site = dir.join("site.yml");
    let mut pb = crate::playbook::load(&site).unwrap_or_else(|e| panic!("{e:#}"));
    pb.plays[0].gather_facts = false;
    let search = crate::roles::RoleSearch {
        paths: vec![dir.join("roles")],
        collections: Vec::new(),
    };
    let mut c = crate::compile::compile(
        &pb.plays[0],
        &search,
        &crate::compile::TagSelection::default(),
    )
    .unwrap_or_else(|e| panic!("{e:#}"));
    mark_boundaries(&mut c, base, dir);
    (0..c.steps.len())
        .filter(|&pos| c.steps[pos].kind == StepKind::Task)
        .map(|pos| (c.steps[pos].task.name.clone(), is_boundary(&c, pos, false)))
        .collect()
}

/// A role with one task file, the tasks written as a YAML list under it.
fn role(dir: &Path, tasks: &str) {
    write(dir, "site.yml", "- hosts: all\n  roles:\n    - prereq\n");
    write(dir, "roles/prereq/tasks/main.yml", tasks);
}

fn named(marks: &[(String, bool)], name: &str) -> bool {
    marks
        .iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no task {name} in {marks:?}"))
        .1
}

/// The shape of the k3s proof's nftables task: a template whose body reads other hosts is a boundary.
/// A variable defined from `hostvars`, a host-local role and a block with a rescue are the other rows.
#[rstest]
#[case::template_body(
    "- name: plain\n  template:\n    src: plain.j2\n    dest: /etc/plain\n\
         - name: nft\n  ansible.builtin.template:\n    src: k3s.nft.j2\n    dest: /etc/nftables.d/k3s.nft\n\
         - name: missing\n  template:\n    src: missing.j2\n    dest: /etc/missing\n\
         - name: rendered src\n  template:\n    src: \"{{ flavour }}.j2\"\n    dest: /etc/x\n\
         - name: pulls another in\n  template:\n    src: pulls.j2\n    dest: /etc/y\n",
    &[
        (
            "roles/prereq/templates/k3s.nft.j2",
            "# Allow inter-node communication (server + agent nodes)\n\
                 {% for host in (groups[server_group] | default([]) + groups[agent_group] | default([])) | unique %}\n\
                 {% if hostvars[host].ansible_default_ipv4 is defined %}\n\
                 insert rule inet filter input ip saddr {{ hostvars[host].ansible_default_ipv4.address }} accept\n\
                 {% endif %}\n\
                 {% endfor %}\n\
                 insert rule inet filter input tcp dport {{ api_port | default(6443) }} accept\n",
        ),
        (
            "roles/prereq/templates/plain.j2",
            "# loaded via an include in nftables.conf\nport {{ api_port | default(6443) }}\n",
        ),
        ("roles/prereq/templates/pulls.j2", "{% include 'k3s.nft.j2' %}\n"),
    ],
    None,
    &[
        ("nft", true),
        ("plain", false),
        ("missing", true),
        ("rendered src", true),
        ("pulls another in", true),
    ],
)]
#[case::variable_defined_from_hostvars(
    "- name: plain\n  debug:\n    msg: \"{{ plain }}\"\n\
         - name: direct\n  debug:\n    msg: \"{{ api_url }}\"\n\
         - name: gap 1\n  debug:\n    msg: gap\n\
         - name: chained\n  debug:\n    msg: \"{{ a }}\"\n\
         - name: gap 2\n  debug:\n    msg: gap\n\
         - name: bare\n  debug:\n    msg: ok\n  when: api_url is defined\n\
         - name: gap 3\n  debug:\n    msg: gap\n\
         - name: from the inventory\n  command: \"curl {{ api_endpoint }}\"\n\
         - name: gap 4\n  debug:\n    msg: gap\n\
         - name: in a template\n  template:\n    src: conf.j2\n    dest: /etc/conf\n",
    &[
        (
            "roles/prereq/defaults/main.yml",
            "api_url: \"{{ hostvars[groups['server'][0]].ansible_host }}\"\n\
                 a: \"{{ b }}\"\n\
                 b: \"{{ c }}/v1\"\n\
                 c: \"https://{{ api_url }}:6443\"\n\
                 plain: \"{{ a_local_value | default('x') }}\"\n",
        ),
        ("roles/prereq/templates/conf.j2", "server: {{ a }}\n"),
    ],
    Some(json!({
        "api_endpoint": "{{ hostvars[groups['server'][0]]['ansible_host'] | default(groups['server'][0]) }}",
    })),
    &[
        ("direct", true),
        ("chained", true),
        ("bare", true),
        ("from the inventory", true),
        ("in a template", true),
        ("plain", false),
    ],
)]
#[case::host_local(
    "- name: Include OS-specific vars\n  include_vars: Debian.yml\n\
         - name: Install {{ package }}\n  apt:\n    name: \"{{ package }}\"\n\
         - name: conf\n  template:\n    src: local.j2\n    dest: /etc/local\n\
         - name: remember\n  set_fact:\n    seen: \"{{ ansible_facts.hostname }}:{{ port }}\"\n\
         - name: all hosts\n  debug:\n    msg: \"{{ ansible_play_hosts_all | length }}\"\n",
    &[
        ("roles/prereq/vars/main.yml", "port: 80\n"),
        ("roles/prereq/vars/Debian.yml", "package: nginx\n"),
        ("roles/prereq/templates/local.j2", "listen {{ port }} on {{ inventory_hostname }}\n"),
    ],
    None,
    &[
        ("Include OS-specific vars", false),
        ("Install {{ package }}", false),
        ("conf", false),
        ("remember", false),
        ("all hosts", false),
    ],
)]
#[case::fence_behind_a_rescue(
    "- block:\n\
         \x20   - name: reader\n      debug:\n        msg: \"{{ hostvars['n1'].state }}\"\n\
         \x20 rescue:\n\
         \x20   - name: rescued\n      debug:\n        msg: r\n\
         - name: past the block\n  debug:\n    msg: p\n\
         - name: further on\n  debug:\n    msg: f\n",
    &[],
    None,
    &[
        ("reader", true),
        ("rescued", true),
        ("past the block", true),
        ("further on", false),
    ],
)]
fn a_role_is_marked_where_it_crosses_hosts(
    #[case] tasks: &str,
    #[case] files: &[(&str, &str)],
    #[case] definitions: Option<Value>,
    #[case] expected: &[(&str, bool)],
) {
    let tree = scratch();
    role(tree.path(), tasks);
    for (rel, text) in files {
        write(tree.path(), rel, text);
    }
    let mut base = Definitions::default();
    if let Some(map) = definitions {
        base.add(&serde_json::from_value(map).expect("a map"));
    }
    let marks = marked_in(tree.path(), &base);
    for (name, boundary) in expected {
        assert_eq!(named(&marks, name), *boundary, "{name}: {marks:?}");
    }
    assert_eq!(
        marks.len(),
        marks
            .iter()
            .filter(|(n, _)| expected.iter().any(|(e, _)| e == n) || n.starts_with("gap"))
            .count(),
        "{marks:?}"
    );
}

/// A list of plain steps marked against no definitions: the answer for each step under `batching`.
#[rstest]
#[case::pause(
    || vec![task("debug"), task("pause"), task("ansible.builtin.pause")],
    &[false, true, true],
)]
#[case::dynamic_vars_read(
    || ["{{ vars['peer_' ~ inventory_hostname] }}", "{{ lookup('vars', 'peer') }}",
        "{{ lookup('ansible.builtin.template', 'peer.j2') }}", "{{ q('vars', 'peer') }}"]
        .map(|m| with_arg("debug", "msg", m)).to_vec(),
    &[true; 4],
)]
#[case::delegated(
    || {
        let mut delegated = task("command");
        delegated.delegate_to = Some("{{ groups.db[0] }}".into());
        vec![task("debug"), delegated, task("debug"), task("debug")]
    },
    &[false, true, true, false],
)]
#[case::facts_written_onto_another_host(
    || {
        let mut writer = with_arg("set_fact", "peer", "up");
        writer.delegate_to = Some("h2".into());
        writer.delegate_facts = Some(true);
        vec![task("debug"), writer, task("debug"), with_arg("debug", "msg", "{{ peer }}")]
    },
    &[false, true, true, true],
)]
#[case::facts_not_written_onto_another_host(
    || {
        let mut writer = with_arg("set_fact", "peer", "up");
        writer.delegate_to = Some("h2".into());
        writer.delegate_facts = Some(false);
        vec![task("debug"), writer, task("debug"), with_arg("debug", "msg", "{{ peer }}")]
    },
    &[false, true, true, false],
)]
#[case::fence_behind_a_read(
    || vec![
        with_arg("set_fact", "state", "standby"),
        with_arg("debug", "msg", "{{ groups.all | map('extract', hostvars, 'state') | list }}"),
        with_arg("set_fact", "state", "active"),
        task("debug"),
    ],
    &[false, true, true, false],
)]
#[case::controller_file_behind_a_fetch(
    || vec![
        with_arg("copy", "src", "files/motd"),
        task("fetch"),
        task("debug"),
        with_arg("copy", "src", "files/ca.crt"),
        task("debug"),
        with_arg("lineinfile", "line", "{{ lookup('file', 'files/ca.crt') }}"),
    ],
    &[false, false, false, true, true, true],
)]
#[case::local_connection_writes_nothing(
    || {
        let mut local = task("command");
        local.vars.insert("ansible_connection".into(), json!("local"));
        vec![local, task("debug"), with_arg("copy", "src", "out.txt")]
    },
    &[false, false, true],
)]
fn steps_are_marked_where_they_must_wait(
    #[case] tasks: fn() -> Vec<PlayTask>,
    #[case] expected: &[bool],
) {
    assert_eq!(boundaries(&marked(tasks())), expected);
}

/// A definition that reads another host makes a task naming it a boundary.
#[test]
fn a_dynamic_read_through_a_definition_is_a_boundary() {
    let mut c = Compiled {
        steps: vec![step(with_arg("debug", "msg", "{{ peer }}"))],
        ..Compiled::default()
    };
    let mut defs = Definitions::default();
    defs.add(&serde_json::from_value(json!({"peer": "{{ vars[peer_name] }}"})).expect("a map"));
    mark_boundaries(&mut c, &defs, Path::new("."));
    assert_eq!(boundaries(&c), [true]);
}

/// A fact still holding a template makes every step a boundary; one a host returned does not.
#[test]
fn a_fact_still_holding_a_template_makes_every_step_a_boundary() {
    let store = Mutex::new(
        VarStore::new(
            &crate::inventory::Inventory::parse_ini("h1\n").expect("an inventory"),
            None,
            Path::new("."),
            Map::new(),
        )
        .expect("a store"),
    );
    assert!(!strict(true, &store));
    assert!(strict(false, &store), "batching off is strict");
    store
        .lock()
        .unwrap()
        .set_fact("h1", "plain", json!({"a": ["x"]}));
    store
        .lock()
        .unwrap()
        .set_untrusted_fact("h1", "from_host", json!("{{ hostvars }}"));
    assert!(!strict(true, &store), "neither of these renders again");
    store
        .lock()
        .unwrap()
        .set_fact("h1", "loaded", json!({"url": ["{{ api_url }}"]}));
    assert!(strict(true, &store));
}

/// Steps a splice puts in are boundaries until the list is marked again; their marks move with them.
#[test]
fn a_splice_keeps_the_marks_on_their_steps() {
    let reader = with_arg("debug", "msg", "{{ hostvars['h2'].x }}");
    let mut c = marked(vec![task("debug"), reader, task("debug"), task("debug")]);
    assert_eq!(boundaries(&c), [false, true, true, false]);
    c.splice(1, vec![step(task("debug"))]);
    assert_eq!(boundaries(&c), [false, true, true, true, false]);
    mark_boundaries(&mut c, &Definitions::default(), Path::new("."));
    assert_eq!(boundaries(&c), [false, false, true, true, false]);
    c.crosses.clear();
    assert_eq!(boundaries(&c), [true; 5]);
}

/// What `throttle` converts to, and how it fails, as the reference measured them.
#[rstest]
#[case::int(json!(2), Ok(Some(2)))]
#[case::string(json!("2"), Ok(Some(2)))]
#[case::template(json!("{{ n }}"), Ok(Some(2)))]
#[case::template_string(json!("{{ ns }}"), Ok(Some(3)))]
#[case::zero(json!(0), Ok(None))]
#[case::negative(json!(-1), Ok(None))]
#[case::yes(json!(true), Ok(Some(1)))]
#[case::no(json!(false), Ok(None))]
#[case::whole_float(json!(2.0), Ok(Some(2)))]
#[case::word(json!("abc"), Err("Failed to convert the throttle value to an integer: invalid literal for int() with base 10: 'abc'"))]
#[case::empty(json!(""), Err("Failed to convert the throttle value to an integer: invalid literal for int() with base 10: ''"))]
#[case::suffix(json!("{{ n }}x"), Err("Failed to convert the throttle value to an integer: invalid literal for int() with base 10: '2x'"))]
#[case::undefined(json!("{{ undefined_v }}"), Err("Failed to convert the throttle value to an integer: 'undefined_v' is undefined"))]
#[case::float(json!(1.5), Err("Task failed: Error processing keyword 'throttle': The value 1.5 could not be converted to 'int'."))]
fn throttle_is_converted_as_the_reference_converts_it(
    #[case] value: Value,
    #[case] expected: Result<Option<u64>, &str>,
) {
    use super::throttle::{Unconverted, throttle_of};
    let asked = crate::playbook::Throttle {
        value,
        origin: None,
    };
    let vars = crate::testing::hvars(json!({"n": 2, "ns": "3"}));
    let got = match throttle_of(&asked, &vars, &crate::testing::templar()) {
        Ok(limit) => Ok(limit),
        Err(Unconverted::Fails(e)) => Err(e.0),
        Err(Unconverted::Aborts(why)) => Err(why),
    };
    assert_eq!(got, expected.map_err(str::to_string));
}

/// A gate lets a host in only while fewer than its own limit are running, and hands the turn
/// on when one is dropped.
#[tokio::test]
async fn a_gate_holds_hosts_to_the_limit_and_gives_turns_back() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let gate = Arc::new(crate::executor::prepare::Gate::default());
    let (now, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let mut hosts = Vec::new();
    for _ in 0..6 {
        let (gate, now, most) = (Arc::clone(&gate), Arc::clone(&now), Arc::clone(&most));
        hosts.push(tokio::spawn(async move {
            let turn = super::throttle::wait_for_turn(&gate, 2).await;
            most.fetch_max(now.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            now.fetch_sub(1, Ordering::SeqCst);
            drop(turn);
        }));
    }
    for host in hosts {
        host.await.expect("host");
    }
    assert_eq!(most.load(Ordering::SeqCst), 2);
}
