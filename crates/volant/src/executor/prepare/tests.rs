// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};
use tokio::sync::watch;

use super::connect::connect_for;
use super::escalation::{become_for, remove_omit};
use super::item::{Item, Prepared};
use super::loops::flatten_once;
use super::vars::host_vars;
use super::{PlayPlan, Strategy, display, prepare};
use crate::action_plugins::Kind;
use crate::compile::{Compiled, IncludeParams, Origin, Step};
use crate::executor::coordinator::Progress;
use crate::playbook::{Flag, PlayTask};
use crate::template::{Templar, TemplateError};
use crate::testing::{defaults, hvars, play, scratch, task, templar, write};
use crate::transport::{BecomeMethod, ConnectionDefaults, Escalation};
use crate::vars::{HostVars, VarStore, omit_token};

/// The connection keywords may be templates over a host's facts, and what they render to
/// reaches `ssh` as one argument each: a value shaped like an option or a line is refused.
#[rstest::rstest]
#[case::fact_user("remote_user", "{{ who }}", json!({"who": "deploy"}), Ok(("deploy", None, None)))]
#[case::number_port("port", "{{ p }}", json!({"p": 2222}), Ok(("", Some(2222), None)))]
#[case::string_port("port", "{{ p }}", json!({"p": " 2222 "}), Ok(("", Some(2222), None)))]
#[case::port_zero("port", "{{ p }}", json!({"p": 0}), Err("'port' must be a port number"))]
#[case::port_too_big("port", "{{ p }}", json!({"p": 70000}), Err("'port' must be a port number"))]
#[case::option_user("remote_user", "{{ who }}", json!({"who": "-oProxyCommand=touch MARKER"}), Err("'remote_user' must be a user name"))]
#[case::spaced_user("remote_user", "{{ who }}", json!({"who": "a b"}), Err("'remote_user' must be a user name"))]
#[case::quoted_user("remote_user", "{{ who }}", json!({"who": "a\"b"}), Err("'remote_user' must be a user name"))]
#[case::newline_user("remote_user", "{{ who }}", json!({"who": "a\nb"}), Err("'remote_user' must be a user name"))]
#[case::odd_connection("connection", "{{ c }}", json!({"c": "ssh -o x"}), Err("'connection' must be a plugin name"))]
fn the_connection_keywords_render_to_names_ssh_takes_as_one_argument(
    #[case] key: &str,
    #[case] raw: &str,
    #[case] facts: Value,
    #[case] want: Result<(&str, Option<u16>, Option<&str>), &str>,
) {
    let mut t = task("command");
    match key {
        "remote_user" => t.connect.remote_user = Some(raw.into()),
        "port" => t.connect.port = Some(raw.into()),
        _ => t.connect.connection = Some(raw.into()),
    }
    let got = connect_for(&t, &hvars(facts), &templar());
    match (got, want) {
        (Ok(k), Ok((user, port, connection))) => assert_eq!(
            (k.user.unwrap_or_default(), k.port, k.connection),
            (user.to_string(), port, connection.map(str::to_string))
        ),
        (Err(err), Err(says)) => assert!(err.0.contains(says), "{}", err.0),
        (got, want) => panic!("{got:?} against {want:?}"),
    }
}

/// Each shape was run through ansible-core 2.19.12 as a `debug: msg=` argument: an `omit` value
/// goes and every emptied container stays.
#[test]
fn omit_leaves_containers_behind_but_never_its_own_value() {
    let omit = json!(omit_token());
    let mut args = json!({
        "toplevel_keep": 9,
        "toplevel_drop": omit,
        "nested": {"keep": 1, "drop": omit, "deeper": {"dropme": omit, "stay": 2}},
        "alist": [omit, 1, {"inlist": omit, "other": 3}],
        "nested_list": [[1, omit], 3],
        "only_omit_list": [omit],
        "empty_after": {"k": omit},
    });
    remove_omit(&mut args);
    assert_eq!(
        args,
        json!({
            "toplevel_keep": 9,
            "nested": {"keep": 1, "deeper": {"stay": 2}},
            "alist": [1, {"other": 3}],
            "nested_list": [[1], 3],
            "only_omit_list": [],
            "empty_after": {},
        })
    );
}

/// A task naming a module the run built a payload for is prepared with that payload's module
/// half; a native task is prepared with none.
#[test]
fn a_task_is_prepared_with_the_payload_built_for_its_module() {
    let mut facts = BTreeMap::new();
    facts.insert(
        "lineinfile".to_string(),
        crate::python::ModuleFacts {
            module_fqn: "ansible.modules.lineinfile".into(),
            profile: "legacy".into(),
            rlimit_nofile: 0,
            extensions: Map::new(),
            core: true,
        },
    );
    // A collection's module beside one of the same short name: each task gets its own.
    for (key, fqn) in [
        ("sysctl", "ansible.modules.sysctl"),
        (
            "ansible.posix.sysctl",
            "ansible_collections.ansible.posix.plugins.modules.sysctl",
        ),
    ] {
        facts.insert(
            key.to_string(),
            crate::python::ModuleFacts {
                module_fqn: fqn.into(),
                profile: "legacy".into(),
                rlimit_nofile: 0,
                extensions: Map::new(),
                core: true,
            },
        );
    }
    let union = Arc::new(crate::python::Union {
        hash: "ab".into(),
        zip_b64: "UEsDBA==".into(),
        modules: facts,
        refused: BTreeMap::new(),
        natives: crate::python::Natives::default(),
    });
    let payload_of = |module: &str| {
        let mut plan = plan();
        plan.python = Some(Arc::clone(&union));
        let inventory = crate::inventory::Inventory::parse_ini(
            "h1
",
        )
        .expect("an inventory");
        let store = Mutex::new(
            VarStore::new(&inventory, None, Path::new("."), Map::new()).expect("a var store"),
        );
        let mut step = Step {
            kind: crate::compile::StepKind::Task,
            task: task(module),
            block: None,
            section: crate::compile::Section::Body,
            role: None,
            origin: Arc::new(Origin::default()),
            include_params: None,
            hosts: None,
        };
        step.task.args.insert("path".into(), json!("/tmp/x"));
        let mut warnings = Vec::new();
        let prepared = prepare(
            &step,
            "h1",
            &plan,
            &Progress::default(),
            &Templar::new(PathBuf::from(".")),
            &store,
            &defaults(),
            &mut warnings,
        )
        .expect("the task renders");
        match prepared {
            Prepared::Remote(_, _, _, payload, kind, _) => (payload, kind),
            _ => panic!("{module} is a remote task"),
        }
    };
    let (built, kind) = payload_of("lineinfile");
    let built = built.expect("the run built one for it");
    assert_eq!(built.blob, "ab");
    assert_eq!(built.facts.module_fqn, "ansible.modules.lineinfile");
    assert_eq!(kind, None);
    assert_eq!(
        payload_of("ansible.posix.sysctl")
            .0
            .expect("the run built one for it")
            .facts
            .module_fqn,
        "ansible_collections.ansible.posix.plugins.modules.sysctl"
    );
    assert!(
        payload_of("command").0.is_none(),
        "a module this release runs itself travels without a payload"
    );
    // A plugin's sub-tasks carry the payloads, so the task itself carries none: it says
    // which plugin runs it, and that is all.
    assert_eq!(
        payload_of("ansible.builtin.package"),
        (None, Some(Kind::Package))
    );
}

/// A task goes out with `force_python` wherever a native must not answer: natives off, a
/// non-core module, or ansible-core's `setup`.
#[test]
fn a_task_is_sent_to_python_where_a_native_must_not_answer() {
    use crate::python::{Facts, ModuleFacts, Natives};
    let facts = |name: &str, core: bool| ModuleFacts {
        module_fqn: format!("ansible.modules.{name}"),
        profile: "legacy".into(),
        rlimit_nofile: 0,
        extensions: Map::new(),
        core,
    };
    let modules = BTreeMap::from([
        ("stat".to_string(), facts("stat", true)),
        ("setup".to_string(), facts("setup", true)),
        // Built under their own names: `ansible.builtin.` never takes a `library/` module.
        ("ansible.builtin.stat".to_string(), facts("stat", true)),
        ("ansible.builtin.setup".to_string(), facts("setup", true)),
        // The module a `library/lineinfile.py` builds.
        ("lineinfile".to_string(), facts("lineinfile", false)),
        // A collection's name `runtime.yml` redirects to `ansible.builtin.setup`: the helper
        // builds ansible-core's own file, under its own name.
        ("my.coll.facts".to_string(), facts("setup", true)),
    ]);
    let forced = |natives: Natives, module: &str| -> bool {
        let mut plan = plan();
        plan.python = Some(Arc::new(crate::python::Union {
            hash: "ab".into(),
            zip_b64: "UEsDBA==".into(),
            modules: modules.clone(),
            refused: BTreeMap::new(),
            natives,
        }));
        let step = step_of(task(module), Origin::default());
        let prepared = prepare(
            &step,
            "h1",
            &plan,
            &Progress::default(),
            &Templar::new(PathBuf::from(".")),
            &store_at(Path::new(".")),
            &defaults(),
            &mut Vec::new(),
        )
        .expect("the task renders");
        let Prepared::Remote(items, _, _, Some(payload), _, _) = prepared else {
            panic!("{module} is a python task");
        };
        super::super::run::protocol_task(
            &step.task,
            &items[0],
            Some((&payload, "/usr/bin/python3")),
        )
        .force_python
    };
    let on = Natives {
        enabled: true,
        facts: Facts::Auto,
    };
    let off = Natives {
        enabled: false,
        ..on
    };
    assert_eq!(
        Natives::default(),
        off,
        "a policy nobody set is natives off"
    );
    assert!(!forced(on, "stat"));
    assert!(!forced(on, "ansible.builtin.stat"));
    assert!(forced(off, "stat"), "native_modules = false");
    assert!(
        forced(on, "lineinfile"),
        "a module that is not ansible-core's own"
    );
    assert!(forced(on, "ansible.builtin.setup"), "--facts auto");
    assert!(forced(on, "my.coll.facts"), "setup under another name");
    let python = Natives {
        facts: Facts::Python,
        ..on
    };
    assert!(forced(python, "setup"), "--facts python");
    let native = Natives {
        facts: Facts::Native,
        ..on
    };
    assert!(!forced(native, "ansible.builtin.setup"), "--facts native");
    assert!(forced(
        Natives {
            enabled: false,
            ..native
        },
        "setup"
    ));
}

fn store_at(playbook_dir: &Path) -> Mutex<VarStore> {
    let inventory = crate::inventory::Inventory::parse_ini("h1\n").expect("an inventory");
    Mutex::new(VarStore::new(&inventory, None, playbook_dir, Map::new()).expect("a var store"))
}

fn step_of(task: PlayTask, origin: Origin) -> Step {
    Step {
        kind: crate::compile::StepKind::Task,
        task,
        block: None,
        section: crate::compile::Section::Body,
        role: None,
        origin: Arc::new(origin),
        include_params: None,
        hosts: None,
    }
}

fn prepared(step: &Step, store: &Mutex<VarStore>) -> Result<Vec<Item>, TemplateError> {
    let prepared = prepare(
        step,
        "h1",
        &plan(),
        &Progress::default(),
        &Templar::new(PathBuf::from(".")),
        store,
        &defaults(),
        &mut Vec::new(),
    )?;
    Ok(match prepared {
        Prepared::Skipped(items)
        | Prepared::Local(items, _)
        | Prepared::Remote(items, _, _, _, _, _) => items,
    })
}

/// `add_host`, `group_by` and `set_stats` never connect, as in the reference: they are
/// prepared as controller-side items, so a host that is down still runs them.
#[rstest::rstest]
#[case("group_by", true)]
#[case("ansible.builtin.add_host", true)]
#[case("set_stats", true)]
#[case("uri", false)]
fn inventory_actions_are_prepared_without_a_link(#[case] module: &str, #[case] local: bool) {
    let step = step_of(task(module), Origin::default());
    let prepared = prepare(
        &step,
        "h1",
        &plan(),
        &Progress::default(),
        &Templar::new(PathBuf::from(".")),
        &store_at(Path::new(".")),
        &defaults(),
        &mut Vec::new(),
    )
    .expect("prepared");
    assert_eq!(matches!(prepared, Prepared::Local(..)), local);
}

fn failing(ignore_errors: &str) -> Step {
    let mut t = task("command");
    t.args.insert("_raw_params".into(), json!("false"));
    t.ignore_errors = Some(Flag::Template(ignore_errors.into()));
    step_of(t, Origin::default())
}

/// `assert`'s `that` is left to the conditional, so a render error in it is the conditional's
/// to report; other arguments still render.
#[test]
fn prepare_leaves_an_assert_s_that_to_the_conditional() {
    let store = store_at(Path::new("."));
    let mut t = task("assert");
    t.args.insert("that".into(), json!("{{ foo.bar == 1 }}"));
    t.args.insert("fail_msg".into(), json!("{{ 1 + 1 }}"));
    let items = prepared(&step_of(t, Origin::default()), &store)
        .expect("the render error is left to the conditional");
    assert!(!items[0].args.contains_key("that"), "{:?}", items[0].args);
    assert_eq!(items[0].args["fail_msg"], json!(2));
}

/// Each item's verdict, as `prepare` rendered it.
fn verdicts(step: &Step, store: &Mutex<VarStore>) -> Vec<Option<bool>> {
    prepared(step, store)
        .unwrap()
        .iter()
        .map(|i| i.ignore_errors)
        .collect()
}

/// A templated `ignore_errors` is read like a strict boolean, once per item, and also in an included file.
#[test]
fn a_templated_ignore_errors_is_rendered_per_item_and_in_includes() {
    {
        let store = store_at(Path::new("."));
        for (template, want) in [
            ("{{ ansible_check_mode }}", false),
            ("{{ true }}", true),
            ("{{ 1 }}", true),
            ("{{ 0.0 }}", false),
            ("{{ ' Y ' }}", true),
            ("{{ 'Off' }}", false),
            ("{{ 't' }}", true),
        ] {
            assert_eq!(
                verdicts(&failing(template), &store),
                [Some(want)],
                "{template}"
            );
        }
        for (template, shown) in [("{{ 'maybe' }}", "'maybe'"), ("{{ 2 }}", "2")] {
            let Err(err) = prepared(&failing(template), &store) else {
                panic!("{template} is not a boolean");
            };
            assert!(
                err.0.contains(&format!(
                    "Error processing keyword 'ignore_errors': The value {shown} could not be converted to 'bool'."
                )),
                "{}",
                err.0
            );
        }

        // A task a `when` left out never renders it, so a template it could not render is not
        // its problem; a task that wrote no template carries no verdict of its own.
        let mut step = failing("{{ nosuch }}");
        step.task.when = vec!["false".into()];
        assert!(prepared(&step, &store).is_ok());
        let step = step_of(task("command"), Origin::default());
        assert_eq!(verdicts(&step, &store), [None]);
    }
    {
        let store = store_at(Path::new("."));
        let mut step = failing("{{ item }}");
        step.task.loop_items = Some(json!([true, false, "yes"]));
        assert_eq!(
            verdicts(&step, &store),
            [Some(true), Some(false), Some(true)]
        );

        let mut values = Map::new();
        values.insert("item".into(), json!(false));
        step.include_params = Some(Arc::new(IncludeParams {
            values,
            untrusted: BTreeSet::new(),
        }));
        step.task.loop_items = Some(json!([true]));
        assert_eq!(verdicts(&step, &store), [Some(true)]);

        let mut step = failing("{{ true }}");
        step.task.loop_items = Some(json!([1, 2]));
        step.task.when = vec!["item == 1".into()];
        assert_eq!(verdicts(&step, &store), [Some(true), None]);
    }
    {
        let store = store_at(Path::new("."));
        let mut step = failing("{{ lenient }}");
        step.origin = Arc::new(Origin {
            depth: 1,
            ..Origin::default()
        });
        let mut values = Map::new();
        values.insert("lenient".into(), json!(true));
        step.include_params = Some(Arc::new(IncludeParams {
            values,
            untrusted: BTreeSet::new(),
        }));
        assert_eq!(verdicts(&step, &store), [Some(true)]);
    }
}

/// An undefined variable in a loop waits for the task's `when`: a false one skips, and a true
/// or absent one fails with the loop's own error.
#[test]
fn an_undefined_loop_waits_for_the_when() {
    let store = store_at(Path::new("."));
    let loops = [
        (json!("{{ nope }}"), false, None),
        (json!("{{ nope }}"), true, None),
        (json!(["{{ nope }}"]), false, Some("fileglob")),
        (json!("{{ nope }}"), false, Some("first_found")),
    ];
    for (raw, with_items, with) in loops {
        let mut t = task("debug");
        t.loop_items = Some(raw.clone());
        t.with_items = with_items;
        t.loop_with = with.map(str::to_string);
        t.when = vec!["true".into(), "nope is defined".into()];
        let items = prepared(&step_of(t.clone(), Origin::default()), &store)
            .unwrap_or_else(|e| panic!("{raw} is skipped: {}", e.0));
        assert_eq!(items.len(), 1, "{raw}: the task, not an item");
        assert_eq!(items[0].element, None);
        let skipped = items[0].skipped.as_ref().expect("skipped").0.clone();
        assert_eq!(
            Value::Object(skipped),
            json!({
                "changed": false,
                "skipped": true,
                "skip_reason": "Conditional result was False",
                "false_condition": "nope is defined",
            }),
            "{raw}"
        );

        for when in [vec![], vec!["true".to_string()], vec!["(".to_string()]] {
            t.when = when.clone();
            let Err(err) = prepared(&step_of(t.clone(), Origin::default()), &store) else {
                panic!("{raw} under {when:?} fails");
            };
            assert!(err.is_undefined(), "{raw} under {when:?}: {}", err.0);
        }
    }

    // A loop that renders is untouched, and an error other than an undefined variable
    // fails the task whatever its `when` says.
    let mut t = task("debug");
    t.loop_items = Some(json!([1, 2]));
    t.when = vec!["false".into()];
    let items = prepared(&step_of(t.clone(), Origin::default()), &store).unwrap();
    assert_eq!(items.len(), 2);
    t.loop_items = Some(json!("{{ ( }}"));
    let Err(err) = prepared(&step_of(t, Origin::default()), &store) else {
        panic!("a syntax error in the loop fails the task");
    };
    assert!(!err.is_undefined(), "{}", err.0);
}

/// A role's task carries its role path and search path, and a host's facts never choose them.
#[test]
fn the_role_path_and_search_path_are_the_engine_s_own() {
    {
        let play = PathBuf::from("/srv/play");
        let role = play.join("roles/probe");
        let store = store_at(&play);
        store.lock().unwrap().set_untrusted_fact(
            "h1",
            "ansible_search_path",
            json!(["/from/a/host"]),
        );
        let in_role = step_of(
            task("command"),
            Origin {
                file_dir: role.join("tasks"),
                role_dir: Some(role.clone()),
                depth: 0,
                inherited: None,
            },
        );
        let vars = &prepared(&in_role, &store).unwrap()[0].vars;
        assert_eq!(vars.map["role_path"], json!("/srv/play/roles/probe"));
        assert_eq!(
            vars.map["ansible_search_path"],
            json!([
                "/srv/play/roles/probe",
                "/srv/play/roles/probe/tasks",
                "/srv/play"
            ])
        );
        assert!(!vars.untrusted.contains("ansible_search_path"));
        assert!(!vars.untrusted.contains("role_path"));

        let in_play = step_of(
            task("command"),
            Origin {
                file_dir: play.clone(),
                role_dir: None,
                depth: 0,
                inherited: None,
            },
        );
        let vars = &prepared(&in_play, &store).unwrap()[0].vars;
        assert_eq!(vars.map["ansible_search_path"], json!(["/srv/play"]));
        assert!(!vars.map.contains_key("role_path"));
    }
    {
        let play = PathBuf::from("/srv/play");
        let role = play.join("roles/probe");
        let store = store_at(&play);
        store.lock().unwrap().gather_facts(
            "h1",
            json!({"role_path": "/tmp/x", "ansible_search_path": ["/tmp/x"]})
                .as_object()
                .unwrap(),
        );
        let in_role = step_of(
            task("command"),
            Origin {
                file_dir: role.join("tasks"),
                role_dir: Some(role.clone()),
                depth: 0,
                inherited: None,
            },
        );
        let vars = &prepared(&in_role, &store).unwrap()[0].vars;
        assert_eq!(vars.get("role_path"), Some(&json!("/srv/play/roles/probe")));
        assert_eq!(
            vars.get("ansible_search_path"),
            Some(&json!([
                "/srv/play/roles/probe",
                "/srv/play/roles/probe/tasks",
                "/srv/play"
            ]))
        );
        let templar = Templar::new(play.clone());
        assert_eq!(
            templar.render("{{ role_path }}", vars).unwrap(),
            json!("/srv/play/roles/probe"),
            "a template reads the engine's value too"
        );
        assert!(!vars.untrusted.contains("role_path"));
    }
}

/// A task's own `vars:` built from a lookup that needs the role's search path to find its file.
#[test]
fn a_tasks_own_vars_resolve_with_the_role_s_search_path_already_in_place() {
    let play_guard = scratch();
    let play = play_guard.path().to_path_buf();
    let role = play.join("roles/probe");
    std::fs::create_dir_all(role.join("templates")).expect("role dir");
    std::fs::write(role.join("templates/t.j2"), "{{ '{{ 1 + 1 }}' }}").expect("template file");

    let store = store_at(&play);
    let mut t = task("command");
    t.vars.insert(
        "via_lookup".into(),
        json!("{{ lookup('template', 't.j2') }}"),
    );
    let step = step_of(
        t,
        Origin {
            file_dir: role.join("tasks"),
            role_dir: Some(role.clone()),
            depth: 0,
            inherited: None,
        },
    );
    let templar = Templar::new(play.clone());
    let prepared = prepare(
        &step,
        "h1",
        &plan(),
        &Progress::default(),
        &templar,
        &store,
        &defaults(),
        &mut Vec::new(),
    )
    .expect("the task renders");
    let items = match prepared {
        Prepared::Skipped(items)
        | Prepared::Local(items, _)
        | Prepared::Remote(items, _, _, _, _, _) => items,
    };
    let vars = &items[0].vars;
    assert_eq!(vars.map["via_lookup"], json!("{{ 1 + 1 }}"));
    assert!(
        vars.untrusted.contains("via_lookup"),
        "{:?}",
        vars.untrusted
    );
}

fn plan() -> PlayPlan {
    PlayPlan {
        plan: watch::channel(Arc::new(Compiled::default())).1,
        force_handlers: false,
        lockstep: false,
        strategy: Strategy::Linear,
        throttles: Mutex::default(),
        queue: Vec::new(),
        turn: watch::channel(0).0,
        play_vars: Map::new(),
        vars_files: HashMap::new(),
        all_play_hosts: Vec::new(),
        r#become: None,
        become_user: None,
        python: None,
        step: None,
    }
}

/// The `become` flag and its target user: host variable, then task keyword, then play keyword, then root.
#[test]
fn become_follows_the_variable_then_the_task_then_the_play() {
    {
        let templar = templar();
        let escalate = |task_become, play_become, host: Value| {
            let mut t = task("command");
            t.r#become = task_become;
            let mut p = plan();
            p.r#become = play_become;
            become_for(&t, &p, &hvars(host), &defaults(), &templar).unwrap()
        };
        assert!(
            escalate(None, Some(true), json!({"ansible_become": false})).is_none(),
            "the variable turns a play's become off"
        );
        assert!(
            escalate(None, Some(false), json!({"ansible_become": true})).is_some(),
            "and turns it on where the play said no"
        );
        assert!(
            escalate(Some(true), None, json!({"ansible_become": false})).is_none(),
            "a task keyword loses to the variable too"
        );
        assert!(
            escalate(Some(false), Some(true), json!({})).is_none(),
            "with no variable, the task keyword beats the play's"
        );
        assert!(
            escalate(None, Some(true), json!({})).is_some(),
            "and the play keyword stands on its own"
        );
        assert!(
            escalate(None, None, json!({"ansible_become": "yes"})).is_some(),
            "a variable spelled the way Ansible content spells it is still a boolean"
        );
    }
    {
        let templar = templar();
        let mut t = task("command");
        t.r#become = Some(true);
        let mut p = plan();
        p.become_user = Some("play".into());
        assert_eq!(
            become_for(&t, &p, &HostVars::default(), &defaults(), &templar)
                .unwrap()
                .unwrap()
                .user,
            "play"
        );
        t.become_user = Some("task".into());
        assert_eq!(
            become_for(&t, &p, &HostVars::default(), &defaults(), &templar)
                .unwrap()
                .unwrap()
                .user,
            "task"
        );
        let host = hvars(json!({"ansible_become_user": "host"}));
        assert_eq!(
            become_for(&t, &p, &host, &defaults(), &templar)
                .unwrap()
                .unwrap()
                .user,
            "host",
            "the variable wins here as well"
        );
        let bare = task("command");
        let mut d = defaults();
        d.r#become = true;
        assert_eq!(
            become_for(&bare, &plan(), &HostVars::default(), &d, &templar)
                .unwrap()
                .unwrap()
                .user,
            "root",
            "nothing said anywhere means root, as in Ansible"
        );
        let templated = hvars(json!({"ansible_become_user": "{{ who }}", "who": "deploy"}));
        assert_eq!(
            become_for(&t, &p, &templated, &defaults(), &templar)
                .unwrap()
                .unwrap()
                .user,
            "deploy",
            "a templated user is resolved before the link is keyed by it"
        );
    }
}

/// A per-item `become_user` and an unsupported method are refused by name.
#[test]
fn a_become_that_cannot_be_honoured_says_why() {
    {
        let templar = templar();
        let mut t = task("command");
        t.r#become = Some(true);
        t.become_user = Some("{{ item }}".into());
        t.loop_items = Some(json!(["root", "deploy"]));
        let err = become_for(&t, &plan(), &HostVars::default(), &defaults(), &templar).unwrap_err();
        assert!(
            err.0.contains("become_user") && err.0.contains("per loop item"),
            "{}",
            err.0
        );
    }
    {
        let templar = templar();
        let mut t = task("command");
        t.r#become = Some(true);
        let err = become_for(
            &t,
            &plan(),
            &hvars(json!({"ansible_become_method": "doas"})),
            &defaults(),
            &templar,
        )
        .unwrap_err();
        assert!(
            err.0.contains("doas") && err.0.contains("not supported"),
            "{}",
            err.0
        );
        assert_eq!(
            become_for(
                &task("command"),
                &plan(),
                &hvars(json!({"ansible_become_method": "doas"})),
                &defaults(),
                &templar,
            )
            .unwrap(),
            None,
            "a task that escalates nowhere is not refused for a method it never runs"
        );
        let d = ConnectionDefaults {
            become_method: "doas".into(),
            ..defaults()
        };
        let err = become_for(&t, &plan(), &HostVars::default(), &d, &templar).unwrap_err();
        assert!(
            err.0.contains("doas") && err.0.contains("not supported"),
            "the defaults are refused for an escalating task the startup pass could not see: {}",
            err.0
        );
        assert_eq!(
            become_for(
                &task("command"),
                &plan(),
                &HostVars::default(),
                &d,
                &templar
            )
            .unwrap(),
            None,
            "and the same defaults leave a task that never escalates alone"
        );
    }
}

/// The method, `become_exe` and `become_flags` one task resolves to.
type Program<'a> = (BecomeMethod, Vec<&'a str>, Option<Vec<&'a str>>);

/// `become_method`, `become_exe` and `become_flags`: the host variable over the keyword over
/// the defaults, templates rendered, words split, and a value that cannot be split refused.
#[rstest::rstest]
#[case::defaults(json!({}), (None, None, None), Ok((BecomeMethod::Sudo, vec![], None)))]
#[case::keywords(json!({}), (Some("su"), Some("/bin/su"), Some("- -m")), Ok((BecomeMethod::Su, vec!["/bin/su"], Some(vec!["-", "-m"]))))]
#[case::host_over_keyword(json!({"ansible_become_method": "sudo", "ansible_become_exe": "/opt/sudo", "ansible_become_flags": "-E"}), (Some("su"), Some("/bin/su"), Some("-")), Ok((BecomeMethod::Sudo, vec!["/opt/sudo"], Some(vec!["-E"]))))]
#[case::templated(json!({"tool": "/usr/bin/sudo -E"}), (None, Some("{{ tool }}"), None), Ok((BecomeMethod::Sudo, vec!["/usr/bin/sudo", "-E"], None)))]
#[case::no_flags(json!({}), (None, None, Some("")), Ok((BecomeMethod::Sudo, vec![], Some(vec![]))))]
#[case::unbalanced(json!({}), (None, None, Some("-H 'x")), Err("'become_flags' has unbalanced quotes"))]
#[case::unknown_keyword(json!({}), (Some("doas"), None, None), Err("become_method 'doas' is not supported yet"))]
fn the_escalation_program_comes_from_the_variable_the_keyword_or_the_defaults(
    #[case] vars: Value,
    #[case] keywords: (Option<&str>, Option<&str>, Option<&str>),
    #[case] want: Result<Program<'static>, &'static str>,
) {
    let mut t = task("command");
    t.r#become = Some(true);
    t.connect.become_method = keywords.0.map(str::to_string);
    t.connect.become_exe = keywords.1.map(str::to_string);
    t.connect.become_flags = keywords.2.map(str::to_string);
    let got = become_for(&t, &plan(), &hvars(vars), &defaults(), &templar());
    match (got, want) {
        (Ok(Some(e)), Ok((method, exe, flags))) => {
            assert_eq!(e.method, method);
            assert_eq!(e.exe, exe);
            assert_eq!(
                e.flags,
                flags.map(|f| f.into_iter().map(str::to_string).collect())
            );
        }
        (Err(err), Err(says)) => assert!(err.0.contains(says), "{}", err.0),
        (got, want) => panic!("got {got:?}, wanted {want:?}"),
    }
}

/// The password never has to be quoted, logged or formatted, so the one place it could still
/// leak is a `Debug` that a `-vvv` diagnostic reaches.
#[test]
fn the_become_password_is_redacted_in_debug_output() {
    let escalation = Escalation {
        method: BecomeMethod::Su,
        user: "root".into(),
        password: Some("s3cret".into()),
        exe: Vec::new(),
        flags: None,
    };
    assert!(!format!("{escalation:?}").contains("s3cret"));
    let mut d = defaults();
    d.become_password = Some("s3cret".into());
    assert!(!format!("{d:?}").contains("s3cret"));
}

#[test]
fn labels_show_strings_bare_and_the_rest_as_python_does() {
    assert_eq!(display(&json!("one")), "one");
    assert_eq!(
        display(&json!({"r": 1, "d": "nan"})),
        "{'r': 1, 'd': 'nan'}"
    );
    assert_eq!(
        display(&json!([1, "a", true, null])),
        "[1, 'a', True, None]"
    );
    assert_eq!(display(&json!(["it's", "a\\b\n"])), r#"["it's", 'a\\b\n']"#);
    assert_eq!(display(&json!(3)), "3");
}

#[test]
fn with_items_flattens_one_level_only() {
    assert_eq!(
        flatten_once(vec![json!([1, [2]]), json!(3)]),
        vec![json!(1), json!([2]), json!(3)]
    );
}

/// A delegate is the inventory's host, the local host for the implicit spellings, or an ssh
/// target of that name, with its variables merged as any host's.
#[test]
fn a_delegate_is_the_inventory_s_host_or_an_implicit_one() {
    let inventory = crate::inventory::Inventory::parse_ini(
        "h3 ansible_host=10.0.0.3
",
    )
    .expect("an inventory");
    let store = Mutex::new(
        VarStore::new(&inventory, None, Path::new("."), Map::new()).expect("a var store"),
    );
    let templar = templar();
    let live = Progress::default();
    let plan = plan();
    let of = |name: &str| {
        host_vars(
            name,
            &plan,
            &Map::new(),
            None,
            None,
            &live,
            &templar,
            &store,
            &Origin::default(),
            Path::new("."),
        )
        .map
    };

    assert_eq!(of("h3").get("ansible_host"), Some(&json!("10.0.0.3")));

    for name in ["localhost", "127.0.0.1", "::1"] {
        assert_eq!(
            of(name).get("ansible_connection"),
            Some(&json!("local")),
            "{name}"
        );
    }

    let stranger = of("nosuch");
    assert_eq!(stranger.get("ansible_connection"), None);
    assert_eq!(
        stranger.get("ansible_host"),
        None,
        "an unknown name is an ssh target of that name, not a refusal"
    );
}

/// The one task of `yaml`, read by the loader, as a step written in `origin`.
fn loaded(yaml: &str, origin: Origin) -> Step {
    let pb = play(yaml);
    let Some(crate::playbook::TaskOrBlock::Task(t)) = pb.plays[0].tasks.first() else {
        panic!("one task");
    };
    step_of(t.clone(), origin)
}

/// What each item binds the loop variable to, and whether it binds it as data.
fn bound(step: &Step, store: &Mutex<VarStore>) -> Result<Vec<(Value, bool)>, TemplateError> {
    Ok(prepared(step, store)?
        .iter()
        .map(|item| {
            let var = &step.task.loop_var;
            (
                item.vars.map[var].clone(),
                item.vars.untrusted.contains(var),
            )
        })
        .collect())
}

/// The `airgap` role's `with_first_found` copy: the first candidate that exists is the item,
/// `skip` makes none a skipped task, and otherwise it fails.
#[test]
fn with_first_found_walks_the_file_it_found_and_nothing_under_skip() {
    let dir = scratch();
    let task = |skip: &str| {
        format!(
            "- hosts: all\n  tasks:\n    - name: Distribute K3s binary\n      copy:\n        src: \"{{{{ item }}}}\"\n        dest: /usr/local/bin/k3s\n      with_first_found:\n        - files:\n            - \"{{{{ airgap_dir }}}}/k3s-{{{{ k3s_arch }}}}\"\n            - \"{{{{ airgap_dir }}}}/k3s\"\n{skip}      vars:\n        airgap_dir: {}\n        k3s_arch: arm64\n",
            dir.path().display()
        )
    };
    let store = store_at(dir.path());
    let step = loaded(&task("          skip: true\n"), Origin::default());
    assert_eq!(step.task.loop_with.as_deref(), Some("first_found"));

    assert_eq!(bound(&step, &store).unwrap(), []);
    let plain = write(dir.path(), "k3s", "x").display().to_string();
    assert_eq!(bound(&step, &store).unwrap(), [(json!(plain), false)]);
    let arch = write(dir.path(), "k3s-arm64", "x").display().to_string();
    assert_eq!(bound(&step, &store).unwrap(), [(json!(arch), false)]);

    std::fs::remove_file(&arch).expect("the file goes");
    std::fs::remove_file(&plain).expect("the file goes");
    let strict = loaded(&task(""), Origin::default());
    let err = bound(&strict, &store).expect_err("nothing found and no skip");
    assert!(
        err.0.contains("No file was found when using first_found."),
        "{}",
        err.0
    );
}

/// The `airgap` role's `with_fileglob` walks every match, sorted, and one match is still a loop
/// of one.
#[test]
fn with_fileglob_walks_every_match_and_one_match_is_a_list_of_one() {
    let dir = scratch();
    let task = |pattern: &str| {
        format!(
            "- hosts: all\n  tasks:\n    - name: Distribute K3s images\n      copy:\n        src: \"{{{{ item }}}}\"\n        dest: /var/lib/rancher/k3s/agent/images/\n      with_fileglob:\n        - \"{pattern}\"\n      vars:\n        airgap_dir: {}\n",
            dir.path().display()
        )
    };
    let store = store_at(dir.path());
    let step = loaded(&task("{{ airgap_dir }}/*.tar.gz"), Origin::default());
    assert_eq!(step.task.loop_with.as_deref(), Some("fileglob"));
    assert_eq!(bound(&step, &store).unwrap(), []);

    let one = write(dir.path(), "images-2.tar.gz", "x")
        .display()
        .to_string();
    assert_eq!(bound(&step, &store).unwrap(), [(json!(one), false)]);
    let first = write(dir.path(), "images-1.tar.gz", "x")
        .display()
        .to_string();
    write(dir.path(), "images.txt", "x").display().to_string();
    // Directory-listing order, as `glob.glob` answers: no sort.
    let listed: Vec<(Value, bool)> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path().display().to_string())
        .filter(|path| *path == first || *path == one)
        .map(|path| (json!(path), false))
        .collect();
    assert_eq!(listed.len(), 2);
    assert_eq!(bound(&step, &store).unwrap(), listed);

    let undefined = loaded(&task("{{ nosuch }}/*.rpm"), Origin::default());
    let err = bound(&undefined, &store).expect_err("an undefined term fails fileglob");
    assert!(err.is_undefined(), "{}", err.0);
}

/// The `raspberrypi` role's `include_tasks` over `with_first_found`, which `site.yml` lists
/// statically and runs on every host.
#[test]
fn an_include_over_with_first_found_skips_undefined_terms_and_keeps_the_host_s_choice_as_data() {
    let dir = scratch();
    let role = dir.path().join("roles/raspberrypi");
    let default = write(dir.path(), "roles/raspberrypi/tasks/setup/default.yml", "x")
        .display()
        .to_string();
    let ubuntu = write(dir.path(), "roles/raspberrypi/tasks/setup/Ubuntu.yml", "x")
        .display()
        .to_string();
    let yaml = "- hosts: all\n  tasks:\n    - name: Execute OS related tasks on the Raspberry Pi - {{ action_ }}\n      include_tasks: \"{{ item }}\"\n      with_first_found:\n        - \"{{ action_ }}/{{ detected_distribution }}-{{ detected_distribution_major_version }}.yml\"\n        - \"{{ action_ }}/{{ detected_distribution }}.yml\"\n        - \"{{ action_ }}/{{ ansible_distribution }}-{{ ansible_distribution_major_version }}.yml\"\n        - \"{{ action_ }}/{{ ansible_distribution }}.yml\"\n        - \"{{ action_ }}/default.yml\"\n      vars:\n        state: present\n        action_: >-\n          {% if state == 'present' %}setup{% else %}teardown{% endif %}\n      when:\n        - raspberry_pi | default(false)\n";
    let step = loaded(
        yaml,
        Origin {
            file_dir: role.join("tasks"),
            role_dir: Some(role.clone()),
            depth: 0,
            inherited: None,
        },
    );
    let store = store_at(dir.path());
    let items = prepared(&step, &store).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].vars.map["item"], json!(default));
    assert!(!items[0].vars.untrusted.contains("item"));
    assert!(items[0].skipped.is_some(), "the task's `when` skips it");

    store
        .lock()
        .unwrap()
        .set_untrusted_fact("h1", "ansible_distribution", json!("Ubuntu"));
    store.lock().unwrap().set_untrusted_fact(
        "h1",
        "ansible_distribution_major_version",
        json!("24"),
    );
    assert_eq!(bound(&step, &store).unwrap(), [(json!(ubuntu), true)]);
}

/// A `with_first_found` loop searches the subdirectory its action names (`templates/`, `vars/`,
/// `files/`) before the entry itself.
#[test]
fn with_first_found_searches_the_directory_its_action_names() {
    let dir = scratch();
    let role = dir.path().join("roles/probe");
    let in_role = || Origin {
        file_dir: role.join("tasks"),
        role_dir: Some(role.clone()),
        depth: 0,
        inherited: None,
    };
    let store = store_at(dir.path());
    let template = write(dir.path(), "roles/probe/templates/t.j2", "x")
        .display()
        .to_string();
    let step = loaded(
        "- hosts: all\n  tasks:\n    - template:\n        src: \"{{ item }}\"\n        dest: /tmp/t\n      with_first_found:\n        - t.j2\n",
        in_role(),
    );
    assert_eq!(bound(&step, &store).unwrap(), [(json!(template), false)]);

    let vars = loaded(
        "- hosts: all\n  tasks:\n    - include_vars: \"{{ item }}\"\n      with_first_found:\n        - files:\n            - a.yml\n          skip: true\n",
        in_role(),
    );
    assert_eq!(bound(&vars, &store).unwrap(), []);
    let found = write(dir.path(), "roles/probe/vars/a.yml", "x")
        .display()
        .to_string();
    assert_eq!(bound(&vars, &store).unwrap(), [(json!(found), false)]);
}

/// A file whose name is a template, matched by a glob under the playbook's own directory, is
/// refused by name before anything renders it.
#[test]
fn a_found_path_that_looks_like_a_template_is_refused_and_never_rendered() {
    let root = scratch();
    let dir = root.path().join("files");
    let marker = root.path().join("files.marker");
    let named = write(
        &dir,
        "{{ lookup('pipe', 'touch ' ~ airgap_dir ~ '.marker') }}.tar.gz",
        "x",
    )
    .display()
    .to_string();
    let store = store_at(&dir);
    for looping in [
        "with_fileglob:\n        - \"{{ airgap_dir }}/*.tar.gz\"\n",
        "loop: \"{{ [lookup('fileglob', airgap_dir ~ '/*.tar.gz')] }}\"\n",
    ] {
        let step = loaded(
            &format!(
                "- hosts: all\n  tasks:\n    - copy:\n        src: \"{{{{ item }}}}\"\n        dest: /tmp/images/\n      {looping}      vars:\n        airgap_dir: {}\n",
                dir.display()
            ),
            Origin::default(),
        );
        let Err(err) = prepared(&step, &store) else {
            panic!("{looping}: the path is refused");
        };
        assert!(
            err.0.contains(&named) && err.0.contains("template marker"),
            "{looping}: {}",
            err.0
        );
        assert!(!marker.exists(), "{looping}: the name's command ran");
    }
}
