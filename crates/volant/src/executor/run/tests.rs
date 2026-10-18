// SPDX-License-Identifier: GPL-3.0-or-later
use super::test_support::*;
use super::*;
use crate::compile::StepKind;
use crate::testing::{hvars, scratch, task, templar, vars};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Every row of `LOCAL_MODULES` has an arm in `run_local` behind it.
#[tokio::test]
async fn every_local_module_has_an_arm_in_run_local() {
    let inventory = crate::inventory::Inventory::parse_ini("h1\n").expect("an inventory");
    let store = Mutex::new(
        VarStore::new(&inventory, None, Path::new("."), Map::new()).expect("a var store"),
    );
    let templar = Templar::new(PathBuf::from("."));
    let step = Step {
        kind: StepKind::Task,
        task: PlayTask::empty(),
        block: None,
        section: crate::compile::Section::Body,
        role: None,
        origin: Arc::new(crate::compile::Origin::default()),
        include_params: None,
        hosts: None,
    };
    for spec in volant_protocol::modules::LOCAL_MODULES {
        let item = Item {
            element: None,
            label: None,
            args: Map::new(),
            args_untrusted: BTreeSet::new(),
            vars: HostVars::default(),
            environment: BTreeMap::new(),
            ignore_errors: None,
            check_mode: None,
            diff: None,
            skipped: None,
            set_fact_names: std::sync::OnceLock::new(),
            notify: Vec::new(),
        };
        // Arguments are deliberately empty: what is asserted is that the name is known, not
        // that the module does its work. A module that needs an argument says so in its own
        // words, which is not the catch-all's words.
        let result = run_local(
            &task(spec.name),
            &item,
            &step,
            &templar,
            &store,
            0,
            &mut watch::channel(false).1,
        )
        .await
        .expect("nothing stops this run");
        assert_ne!(
            result.0.get("msg").and_then(Value::as_str),
            Some(format!("{} is not a controller-side module", spec.name).as_str()),
            "{} is in LOCAL_MODULES with no arm in run_local",
            spec.name
        );
    }
}

/// One controller-side module run the way the driver runs it, with nothing interrupting.
async fn local(module: &str, args: Value) -> TaskResult {
    local_with(module, args, HostVars::default()).await
}

/// `local` against the host variables given, the task carrying the arguments as written and
/// the item carrying them as `prepare` renders them: per argument, with the names whose
/// render read a managed host set apart.
async fn local_with(module: &str, args: Value, host_vars: HostVars) -> TaskResult {
    let templar = Templar::new(PathBuf::from("."));
    let mut written = task(module);
    written.args = vars(args);
    let (rendered, untrusted) = templar
        .render_map_tainted(&written.args, &host_vars)
        .expect("the arguments render");
    let item = Item {
        args: rendered,
        args_untrusted: untrusted,
        vars: host_vars,
        ..local_item(json!({}))
    };
    let inventory = crate::inventory::Inventory::parse_ini("h1\n").expect("an inventory");
    let store = Mutex::new(
        VarStore::new(&inventory, None, Path::new("."), Map::new()).expect("a var store"),
    );
    let step = Step {
        kind: StepKind::Task,
        task: PlayTask::empty(),
        block: None,
        section: crate::compile::Section::Body,
        role: None,
        origin: Arc::new(crate::compile::Origin::default()),
        include_params: None,
        hosts: None,
    };
    run_local(
        &written,
        &item,
        &step,
        &templar,
        &store,
        0,
        &mut watch::channel(false).1,
    )
    .await
    .expect("nothing stops this run")
}

/// Measured on ansible-core 2.19.12: every condition true gives `All assertions passed`, and a
/// `success_msg` takes its place.
#[tokio::test]
async fn an_assert_that_holds_passes() {
    let r = local("assert", json!({"that": ["1 == 1", "2 > 1"]})).await;
    assert_eq!(
        Value::Object(r.0),
        json!({"_ansible_verbose_always": true, "changed": false, "msg": "All assertions passed"})
    );
    let r = local("assert", json!({"that": "1 == 1", "success_msg": "fine"})).await;
    assert_eq!(
        Value::Object(r.0),
        json!({"_ansible_verbose_always": true, "changed": false, "msg": "fine"})
    );
}

/// Measured on ansible-core 2.19.12: the failing element comes back as written, a string for
/// `that: 1 == 2` and a boolean for `that: false`, and in a list it is the first false one.
#[tokio::test]
async fn a_failed_assert_names_the_first_false_element_as_written() {
    let r = local("assert", json!({"that": "1 == 2"})).await;
    assert_eq!(
        Value::Object(r.0),
        json!({"_ansible_verbose_always": true, "assertion": "1 == 2", "changed": false, "evaluated_to": false, "exception": "(traceback unavailable)", "failed": true, "msg": "Assertion failed"})
    );
    let r = local("assert", json!({"that": false})).await;
    assert_eq!(r.0["assertion"], json!(false));
    let r = local("assert", json!({"that": ["1 == 1", "2 == 3", "4 == 5"]})).await;
    assert_eq!(r.0["assertion"], json!("2 == 3"));
    assert!(r.failed());
}

/// Measured on ansible-core 2.19.12: `fail_msg`, or its alias `msg`, replaces `msg` and leaves
/// `assertion` and `evaluated_to` in place.
#[tokio::test]
async fn fail_msg_and_its_alias_replace_the_message_and_quiet_only_hides_the_body() {
    for key in ["fail_msg", "msg"] {
        let r = local("assert", json!({"that": "1 == 2", key: "custom"})).await;
        assert_eq!(
            Value::Object(r.0),
            json!({"_ansible_verbose_always": true, "assertion": "1 == 2", "changed": false, "evaluated_to": false, "exception": "(traceback unavailable)", "failed": true, "msg": "custom"}),
            "{key}"
        );
    }
    // `quiet` only keeps the body off the line at verbosity 0.
    let mut loud = local("assert", json!({"that": "1 == 2"})).await;
    let quiet = local("assert", json!({"that": "1 == 2", "quiet": true})).await;
    assert_eq!(
        loud.0.remove(crate::render::VERBOSE_ALWAYS),
        Some(json!(true))
    );
    assert_eq!(loud.0, quiet.0);
}

/// The three refusals measured on ansible-core 2.19.12, the first two in its own words. The
/// third fails the way every other conditional in this engine fails.
#[tokio::test]
async fn assert_refuses_what_the_reference_refuses() {
    let r = local("assert", json!({})).await;
    assert!(r.failed());
    assert_eq!(r.0["msg"], json!("missing required arguments: that"));
    let r = local("assert", json!({"that": "1 == 1", "nosucharg": 1})).await;
    assert!(r.failed());
    assert_eq!(
        r.0["msg"],
        json!(
            "Unsupported parameters for (assert) module: nosucharg. Supported parameters include: fail_msg, quiet, success_msg, that (msg)."
        )
    );
    assert_eq!(
        r.0["changed"],
        json!(false),
        "the reference's refusal is unchanged"
    );
    // The reference ends this one with `'some_undefined_thing' is undefined`. The prefix is
    // the one every conditional here shares; the sentence behind it is the templar's.
    let r = local("assert", json!({"that": "some_undefined_thing"})).await;
    assert!(r.failed());
    assert!(
        r.0["msg"]
            .as_str()
            .is_some_and(|m| m.starts_with("Task failed: Error while evaluating conditional: ")),
        "{:?}",
        r.0
    );
}

/// A host's variables where `r` is a registered result, as `record_registered` leaves it.
fn with_registered(extra: Value) -> HostVars {
    let mut v = hvars(extra);
    v.insert_untrusted("r".into(), json!({"rc": 0, "stdout": "1 == 2"}));
    v
}

/// Measured on ansible-core 2.19.12, after `register: r` on `command: echo "1 == 2"`: the three
/// forms below pass, the first two with the deprecation warning for the braces.
#[tokio::test]
async fn a_that_reading_a_registered_value_reads_it_as_data() {
    for that in [
        json!("{{ r.rc == 0 }}"),
        json!(["1 == 1", "{{ r.rc == 0 }}"]),
        json!("r.stdout == '1 == 2'"),
    ] {
        let r = local_with("assert", json!({"that": that}), with_registered(json!({}))).await;
        assert_eq!(
            Value::Object(r.0),
            json!({"_ansible_verbose_always": true, "changed": false, "msg": "All assertions passed"}),
            "{that}"
        );
    }
}

/// `that` is evaluated by the code that evaluates `when`, element by element, from the text the
/// playbook wrote.
#[tokio::test]
async fn a_that_is_evaluated_the_way_when_is() {
    let host_vars = with_registered(json!({"healthy": "true"}));
    let templar = Templar::new(PathBuf::from("."));
    for that in [
        "{{ healthy }}",
        "healthy",
        "{{ r.stdout }}",
        "r.stdout",
        "{{ 1 == 2 }}",
        "{{ r.rc == 0 }}",
    ] {
        let asserted = local_with("assert", json!({"that": that}), host_vars.clone()).await;
        match templar.condition(that, &host_vars) {
            Ok(held) => assert_eq!(asserted.failed(), !held, "{that}: {:?}", asserted.0),
            Err(e) => assert_eq!(
                asserted.0["msg"],
                json!(conditional_error(&e)),
                "{that}: {:?}",
                asserted.0
            ),
        }
    }
}

/// Measured on ansible-core 2.19.12: a `that` that is one template naming a list is that list,
/// each element a condition, and the one that fails is reported as the list held it.
#[tokio::test]
async fn a_that_naming_a_list_is_that_list() {
    let host_vars = hvars(json!({"good": ["1 == 1", "2 == 2"], "bad": ["1 == 1", "1 == 2"]}));
    let r = local_with("assert", json!({"that": "{{ good }}"}), host_vars.clone()).await;
    assert!(!r.failed(), "{:?}", r.0);
    let r = local_with("assert", json!({"that": "{{ bad }}"}), host_vars).await;
    assert_eq!(r.0["assertion"], json!("1 == 2"), "{:?}", r.0);
    let mut host_vars = HostVars::default();
    host_vars.insert_untrusted("remote".into(), json!(["1 == 1"]));
    let r = local_with("assert", json!({"that": "{{ remote }}"}), host_vars).await;
    assert_eq!(
        r.0["msg"],
        json!(
            "Task failed: Error while evaluating conditional: Encountered untrusted template or expression."
        ),
        "{:?}",
        r.0
    );
}

/// A set_fact renders its keys and checks the names; a key that renders anew keeps its value data.
#[tokio::test]
#[expect(clippy::too_many_lines)]
async fn a_set_fact_renders_its_keys_checks_the_names_and_keeps_values_data() {
    {
        let host = || {
            let mut v = hvars(json!({"n": "dyn"}));
            v.insert_untrusted("h".into(), json!("from_host"));
            v
        };
        let r = local_with("set_fact", json!({"k_{{ n }}": 1, "if": 2}), host()).await;
        assert_eq!(
            r.0["ansible_facts"],
            json!({"k_dyn": 1, "if": 2}),
            "{:?}",
            r.0
        );
        for (key, msg) in [
            (
                "{{ 'bad-name' }}",
                "Task failed: Invalid variable name 'bad-name'.",
            ),
            ("bad-lit", "Task failed: Invalid variable name 'bad-lit'."),
            ("{{ '' }}", "Task failed: Invalid variable name ''."),
            (
                "{{ 1 }}",
                "Task failed: Invalid variable name '1' of type 'int'.",
            ),
            (
                "{{ 1.5 }}",
                "Task failed: Invalid variable name '1.5' of type 'float'.",
            ),
            (
                "{{ [1] }}",
                "Task failed: Invalid variable name of type 'list'.",
            ),
            (
                "{{ True }}",
                "Task failed: Invalid variable name 'True' of type 'bool'.",
            ),
            (
                "{{ None }}",
                "Task failed: Invalid variable name 'None' of type 'NoneType'.",
            ),
            ("not", "Task failed: Invalid variable name 'not'."),
            ("None", "Task failed: Invalid variable name 'None'."),
        ] {
            let r = local_with("set_fact", json!({ key: 1 }), host()).await;
            assert!(r.failed(), "{key}: {:?}", r.0);
            assert_eq!(r.0["msg"], json!(msg), "{key}");
        }
        let r = local_with("set_fact", json!({"cacheable": true}), host()).await;
        assert_eq!(
            r.0["msg"],
            json!(
                "No key/value pairs provided, at least one is required for this action to succeed"
            )
        );
        let r = local_with("set_fact", json!({"{{ nosuch }}": 1}), host()).await;
        assert!(msg(&r).starts_with("Task failed: "), "{:?}", r.0);

        let templar = Templar::new(PathBuf::from("."));
        let mut t = task("set_fact");
        t.args = vars(json!({"k_{{ h }}": 1, "plain": 2}));
        let item = Item {
            args: t.args.clone(),
            vars: host(),
            ..local_item(json!({}))
        };
        let data = set_fact_data(&t, &item, &templar);
        assert_eq!(data, BTreeSet::from(["k_from_host".to_string()]));
    }
    #[cfg(unix)]
    {
        let dir_guard = scratch();
        let dir = dir_guard.path().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        let counter = dir.join("runs");
        let templar = Templar::new(PathBuf::from("."));
        let key = format!(
            "t_{{{{ lookup('pipe', 'echo x >> {c}; wc -l < {c}') | trim }}}}",
            c = counter.display()
        );
        let mut t = task("set_fact");
        t.args = vars(json!({ key.clone(): "{{ ro.stdout }}" }));
        let item = Item {
            args: vars(json!({ key.clone(): "{{ lookup('pipe', 'id') }}" })),
            args_untrusted: BTreeSet::from([key.clone()]),
            ..local_item(json!({}))
        };
        let names = set_fact_names(&item, &templar).expect("the key renders");
        assert_eq!(names[0].0, "t_1");
        assert!(names[0].2, "the value came from a managed host");
        assert_eq!(set_fact_names(&item, &templar).unwrap()[0].0, "t_1");
        let facts = json!({"t_1": "{{ lookup('pipe', 'id') }}"});
        let result = TaskResult(vars(json!({"ansible_facts": facts, "changed": false})));
        let mut store = one_host_store();
        record_local_facts(
            &mut store,
            &t,
            std::slice::from_ref(&item),
            &["h1".to_string()],
            &[(None, result.clone())],
            &templar,
        );
        assert!(
            store
                .untrusted_of("h1", &crate::vars::Scope::default())
                .contains("t_1")
        );
        let mut judged = t.clone();
        judged.failed_when = vec!["false".into()];
        let vars = condition_vars(&judged, &item, &result, &templar);
        assert!(vars.untrusted.contains("t_1"));
        assert_eq!(
            std::fs::read_to_string(&counter).unwrap().lines().count(),
            1,
            "the key's lookup ran once"
        );
    }
}

/// A failed controller-side task registers `exception: "(traceback unavailable)"` as an agent's
/// does; one that succeeds carries none.
#[tokio::test]
async fn a_failed_controller_side_task_registers_an_exception() {
    for (module, args) in [("fail", json!({})), ("assert", json!({"that": "1 == 2"}))] {
        let r = local(module, args).await;
        assert_eq!(
            r.0["exception"],
            json!("(traceback unavailable)"),
            "{module}"
        );
    }
    let r = local("debug", json!({"msg": "hi"})).await;
    assert!(!r.0.contains_key("exception"), "{:?}", r.0);
}

/// Measured on ansible-core 2.19.12: the default message, a custom one, and a list that stays a
/// list. An unknown argument is refused in a sentence of its own, unlike assert and pause.
#[tokio::test]
async fn fail_fails_with_its_message_as_given() {
    let r = local("fail", json!({})).await;
    assert_eq!(
        Value::Object(r.0),
        json!({"changed": false, "exception": "(traceback unavailable)", "failed": true,
               "msg": "Failed as requested from task"})
    );
    let r = local("fail", json!({"msg": "custom failure"})).await;
    assert_eq!(r.0["msg"], json!("custom failure"));
    let r = local("fail", json!({"msg": ["one", "two"]})).await;
    assert_eq!(r.0["msg"], json!(["one", "two"]));
    assert!(r.failed());
    let r = local("fail", json!({"nosucharg": 1})).await;
    assert_eq!(r.0["msg"], json!("Invalid options for fail: nosucharg"));
}

/// Measured on ansible-core 2.19.12: `seconds: 0` still waits one second and says so.
#[tokio::test]
async fn a_pause_of_zero_seconds_waits_one() {
    let began = std::time::Instant::now();
    let mut r = local("pause", json!({"seconds": 0})).await;
    assert!(
        began.elapsed() >= Duration::from_secs(1),
        "{:?}",
        began.elapsed()
    );
    for stamp in ["start", "stop"] {
        let value = r.0.remove(stamp).expect(stamp);
        assert_eq!(value.as_str().map(str::len), Some(26), "{value}");
    }
    // The seconds in `stdout` are measured, so a slow runner prints 1.01: check the prefix.
    let stdout = r.0.remove("stdout").expect("stdout");
    assert!(
        stdout
            .as_str()
            .is_some_and(|s| s.starts_with("Paused for 1.") && s.ends_with(" seconds")),
        "{stdout}"
    );
    assert_eq!(
        Value::Object(r.0),
        json!({"changed": false, "delta": 1, "echo": true, "rc": 0, "stderr": "", "user_input": ""})
    );
}

/// Measured on ansible-core 2.19.12: `minutes: 153722867280912931` fails at once, because
/// Python's clock cannot hold that many nanoseconds.
#[tokio::test]
async fn a_pause_too_long_for_the_clock_fails_at_once() {
    for args in [
        json!({"minutes": 153_722_867_280_912_931_i64}),
        json!({"seconds": 9_223_372_037_i64}),
    ] {
        let r = tokio::time::timeout(Duration::from_secs(5), local("pause", args.clone()))
            .await
            .expect("the pause fails rather than waits");
        assert_eq!(
            Value::Object(r.0),
            json!({"exception": "(traceback unavailable)", "failed": true, "msg": "Task failed: timestamp out of range for C PyTime_t"}),
            "{args}"
        );
    }
}

/// Measured on ansible-core 2.19.12, the two refusals of a pause.
#[tokio::test]
async fn pause_refuses_what_the_reference_refuses() {
    let r = local("pause", json!({"minutes": 1, "seconds": 1})).await;
    assert!(r.failed());
    assert_eq!(
        r.0["msg"],
        json!("parameters are mutually exclusive: minutes|seconds")
    );
    let r = local("pause", json!({"seconds": 1, "nosucharg": 1})).await;
    assert!(r.failed());
    assert_eq!(
        r.0["msg"],
        json!(
            "Unsupported parameters for (pause) module: nosucharg. Supported parameters include: echo, minutes, prompt, seconds."
        )
    );
}

/// Measured on ansible-core 2.19.12 with standard input not a terminal: a prompt warns, does
/// not wait, and reports in minutes.
#[tokio::test]
async fn a_prompt_without_a_terminal_warns_and_goes_on() {
    let item = local_item(json!({"prompt": "Continue?"}));
    let r = pause("pause", "pause", &item, false, &mut watch::channel(false).1)
        .await
        .expect("nothing stops this run");
    assert!(!r.failed(), "{:?}", r.0);
    assert_eq!(r.0["delta"], json!(0));
    assert_eq!(r.0["stdout"], json!("Paused for 0.0 minutes"));
    assert_eq!(r.0["user_input"], json!(""));
    assert_eq!(
        r.0["warnings"],
        json!(["Not waiting for response to prompt as stdin is not interactive"])
    );
}

/// On a terminal a prompt next to a duration is refused: this release cannot wait for both.
#[tokio::test]
async fn a_prompt_on_a_terminal_is_refused() {
    for args in [
        json!({"prompt": "Go?", "seconds": 1}),
        json!({"prompt": "Go?", "minutes": 1}),
    ] {
        let r = pause(
            "pause",
            "pause",
            &local_item(args.clone()),
            true,
            &mut watch::channel(false).1,
        )
        .await
        .expect("nothing stops this run");
        assert_eq!(
            Value::Object(r.0),
            json!({"failed": true, "msg": PROMPT_REFUSED}),
            "{args}"
        );
    }
}

/// An interrupted run does not sit out the rest of a pause, and the pause reports no result.
#[tokio::test]
async fn an_interrupted_pause_ends_at_once() {
    let (stop_tx, mut stop) = watch::channel(false);
    let item = local_item(json!({"minutes": 10}));
    let (r, ()) = tokio::join!(
        tokio::time::timeout(
            Duration::from_secs(5),
            pause("pause", "pause", &item, false, &mut stop)
        ),
        async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            stop_tx.send(true).expect("the receiver is alive");
        }
    );
    let r = r.expect("the interruption ends the pause");
    assert!(r.is_none(), "{r:?}");
}

/// Each complaint the argument check can make, in the reference's own words and in the order it
/// makes them.
#[test]
fn the_argument_check_speaks_the_reference_s_sentences() {
    let check = |provided: Value, host: Value| {
        let item = Item {
            element: None,
            label: None,
            args: vars(json!({
                "argument_spec": {
                    "zeta": {"type": "str", "required": true},
                    "alpha_req": {"type": "str", "required": true},
                    "count": {"type": "int"},
                    "flag": {"type": "bool"},
                    "pick": {"type": "str", "choices": ["alpha", "beta"]},
                },
                "provided_arguments": provided,
                "validate_args_context": {"argument_spec_name": "main", "name": "types", "type": "role"},
            })),
            args_untrusted: BTreeSet::new(),
            vars: hvars(host),
            environment: BTreeMap::new(),
            ignore_errors: None,
            check_mode: None,
            diff: None,
            skipped: None,
            set_fact_names: std::sync::OnceLock::new(),
            notify: Vec::new(),
        };
        validate_argument_spec(&item)
    };

    let all_wrong = check(
        json!({"count": "notanint", "pick": "gamma", "unknown_arg": "whatever"}),
        json!({}),
    );
    assert!(all_wrong.failed());
    assert_eq!(
        all_wrong.0["argument_errors"],
        json!([
            "missing required arguments: alpha_req, zeta",
            "argument 'count' is of type str and we were unable to convert to int: \"'notanint'\" cannot be converted to an int",
            "value of pick must be one of: alpha, beta, got: gamma",
            "unknown_arg. Supported parameters include: alpha_req, count, flag, pick, zeta.",
        ])
    );
    assert_eq!(
        all_wrong.0["msg"],
        json!(
            "Validation of arguments failed:\nmissing required arguments: alpha_req, zeta\nargument 'count' is of type str and we were unable to convert to int: \"'notanint'\" cannot be converted to an int\nvalue of pick must be one of: alpha, beta, got: gamma\nunknown_arg. Supported parameters include: alpha_req, count, flag, pick, zeta."
        )
    );
    assert_eq!(
        all_wrong.0["argument_spec_data"]["zeta"]["required"],
        json!(true)
    );

    // Measured: `"3"` is an `int` and `"yes"` is a `bool`, and nothing is written back - the
    // role's own tasks read `count=3 flag=yes`, the strings they were handed.
    let permissive = check(
        json!({"count": "3", "flag": "yes", "pick": "alpha", "zeta": "z", "alpha_req": "a"}),
        json!({}),
    );
    assert!(!permissive.failed(), "{:?}", permissive.0);
    assert_eq!(permissive.0["msg"], json!("The arg spec validation passed"));
    assert_eq!(
        permissive.0["validate_args_context"]["name"],
        json!("types")
    );

    // Nothing on the entry, everything on the host: the shape `-e zeta=z ...` makes.
    let from_host = check(
        json!({}),
        json!({"zeta": "z", "alpha_req": "a", "count": 2, "flag": false, "pick": "beta"}),
    );
    assert!(!from_host.failed(), "{:?}", from_host.0);
    // A host variable the spec never heard of is not an unknown argument.
    let noise = check(
        json!({}),
        json!({"zeta": "z", "alpha_req": "a", "ansible_forks": 5}),
    );
    assert!(!noise.failed(), "{:?}", noise.0);
}

/// `ansible_failed_task` carries the keys the reference carries, and the ones this engine
/// models carry the task's own values.
#[test]
fn the_failed_task_carries_the_reference_s_own_keys() {
    let mut task = task("ansible.builtin.command");
    task.name = "boom".to_string();
    task.register = Some("b".to_string());
    task.when = vec!["true".to_string()];
    task.tags = vec!["t1".to_string()];
    task.vars = vars(json!({"v": 1}));
    task.connect.port = Some("22".to_string());
    let value = failed_task_value(&task);
    let object = value.as_object().expect("an object");
    assert_eq!(object.len(), 40, "{value}");
    for key in [
        "action",
        "args",
        "become",
        "connection",
        "loop",
        "loop_control",
        "name",
        "register",
        "tags",
        "vars",
        "when",
    ] {
        assert!(object.contains_key(key), "{key} is missing from {value}");
    }
    assert!(!object.contains_key("local_action"), "{value}");
    assert!(!object.contains_key("with_items"), "{value}");
    assert_eq!(object["action"], json!("command"));
    assert_eq!(object["name"], json!("boom"));
    assert_eq!(object["register"], json!("b"));
    assert_eq!(object["when"], json!(["true"]));
    assert_eq!(object["tags"], json!(["t1"]));
    assert_eq!(object["vars"], json!({"v": 1}));
    assert_eq!(object["loop"], Value::Null);
    assert_eq!(
        object["loop_control"],
        json!({"loop_var": "item", "label": null})
    );
    assert_eq!(object["port"], json!(22));
    assert_eq!(
        object["connection"],
        Value::Null,
        "a keyword nobody wrote says nothing rather than the reference's default"
    );
}

#[test]
fn a_loop_aggregate_follows_ansible_shape() {
    let mut t = task("command");
    t.loop_items = Some(json!([1, 2]));
    let results = vec![
        (Some(json!(1)), result(json!({"changed": true, "rc": 0}))),
        (
            Some(json!(2)),
            result(json!({"changed": false, "rc": 1, "failed": true})),
        ),
    ];
    let agg = registered_value(&t, &results);
    assert_eq!(agg["changed"], json!(true));
    assert_eq!(agg["failed"], json!(true));
    assert_eq!(agg["skipped"], json!(false));
    assert_eq!(agg["msg"], json!("One or more items failed"));
    assert_eq!(agg["results"][1]["item"], json!(2));
    assert_eq!(agg["results"][1]["ansible_loop_var"], json!("item"));
}

/// Every result that ran comes out with `failed` set: kept when the result says it, otherwise
/// true for a non-zero `rc`.
#[test]
fn a_result_that_ran_says_whether_it_failed() {
    let templar = templar();
    let t = task("command");
    let item = Item {
        element: None,
        label: None,
        args: Map::new(),
        args_untrusted: BTreeSet::new(),
        vars: HostVars::default(),
        environment: BTreeMap::new(),
        ignore_errors: None,
        check_mode: None,
        diff: None,
        skipped: None,
        set_fact_names: std::sync::OnceLock::new(),
        notify: Vec::new(),
    };
    let failed = |r: Value| {
        let r = apply_conditions(&t, &item, result(r), &templar);
        r.0.get("failed").cloned()
    };
    assert_eq!(failed(json!({"ping": "pong"})), Some(json!(false)));
    assert_eq!(failed(json!({"rc": 0})), Some(json!(false)));
    assert_eq!(failed(json!({"rc": 1})), Some(json!(true)));
    assert_eq!(
        failed(json!({"failed": true, "msg": "missing"})),
        Some(json!(true))
    );
    assert_eq!(
        failed(json!({"rc": 1, "failed": false})),
        Some(json!(false))
    );
    // Measured on 2.19.12 through a module printing each shape: a failure registers `True`
    // whatever made it one, and a falsy `failed` is kept as written.
    assert_eq!(failed(json!({"rc": "2"})), Some(json!(true)));
    assert_eq!(failed(json!({"rc": null})), Some(json!(true)));
    assert_eq!(failed(json!({"failed": "yes"})), Some(json!(true)));
    assert_eq!(failed(json!({"failed": 1})), Some(json!(true)));
    assert_eq!(failed(json!({"failed": 0})), Some(json!(0)));
    assert_eq!(failed(json!({"failed": null})), Some(json!(null)));
    let mut t = task("command");
    t.failed_when = vec!["false".into()];
    let r = apply_conditions(&t, &item, result(json!({"rc": 0})), &templar);
    assert_eq!(r.0.get("failed"), Some(&json!(false)), "{:?}", r.0);
}

/// The recap counts changed, rescued and ignored as the reference did for these five tasks:
/// `ok=4 changed=2 rescued=1 ignored=2`.
#[test]
fn the_recap_counts_changed_the_way_the_reference_does() {
    let templar = templar();
    let item = Item {
        element: None,
        label: None,
        args: Map::new(),
        args_untrusted: BTreeSet::new(),
        vars: HostVars::default(),
        environment: BTreeMap::new(),
        ignore_errors: None,
        check_mode: None,
        diff: None,
        skipped: None,
        set_fact_names: std::sync::OnceLock::new(),
        notify: Vec::new(),
    };
    let plain = task("command");
    let mut quiet = task("command");
    quiet.changed_when = vec!["false".into()];
    let tasks = [
        (
            &plain,
            json!({"changed": true, "failed": true}),
            true,
            false,
        ),
        (
            &plain,
            json!({"changed": "yes", "failed": true}),
            true,
            false,
        ),
        (&quiet, json!({"changed": "yes"}), false, false),
        (
            &plain,
            json!({"changed": true, "failed": true}),
            false,
            true,
        ),
        (&plain, json!({"changed": false}), false, false),
    ];
    let mut stats = crate::stats::Stats::default();
    for (t, shape, ignore_errors, rescuable) in tasks {
        let r = apply_conditions(t, &item, result(shape), &templar);
        stats.record("h", classify(&r, ignore_errors, rescuable), r.changed());
    }
    let h = stats.host("h");
    assert_eq!(
        (h.ok, h.changed, h.rescued, h.ignored, h.failed),
        (4, 2, 1, 2, 0)
    );
}

#[test]
fn failed_when_false_rescues_a_non_zero_rc() {
    let templar = templar();
    let mut t = task("command");
    t.failed_when = vec!["result.rc == 99".into()];
    let item = Item {
        element: None,
        label: None,
        args: Map::new(),
        args_untrusted: BTreeSet::new(),
        vars: HostVars::default(),
        environment: BTreeMap::new(),
        ignore_errors: None,
        check_mode: None,
        diff: None,
        skipped: None,
        set_fact_names: std::sync::OnceLock::new(),
        notify: Vec::new(),
    };
    let r = apply_conditions(
        &t,
        &item,
        result(json!({"rc": 1, "changed": true})),
        &templar,
    );
    assert!(!r.failed(), "{:?}", r.0);
    assert_eq!(r.0["failed_when_result"], json!(false));
    let r = apply_conditions(
        &t,
        &item,
        result(json!({"rc": 99, "changed": true})),
        &templar,
    );
    assert!(r.failed());
}

/// `changed_when`, `failed_when` and `until` read the facts the task returned, laid out as
/// ansible-core 2.19.12 lays them before judging.
#[test]
fn failed_when_changed_when_and_until_read_the_facts_the_task_returned() {
    let templar = templar();
    let item = bare_item();
    let returned = || {
        result(
            json!({"changed": false, "ansible_facts": {"x": 1, "ansible_y": 2, "ansible_host": "evil"}}),
        )
    };
    let mut t = task("ns.coll.facts");
    t.failed_when = vec![
        "x != 1 or ansible_y != 2 or ansible_facts.ansible_facts.y != 2 or ansible_facts.y is defined or ansible_host is defined"
            .into(),
    ];
    t.changed_when = vec!["x == 1".into()];
    let r = finish(&t, &item, returned(), &templar);
    assert!(!r.failed(), "{:?}", r.0);
    assert_eq!(r.0["changed"], json!(true), "{:?}", r.0);
    t.until = vec!["x == 1".into()];
    let retry = retry_plan(&t, Some(&item), &templar)
        .unwrap()
        .expect("a retry plan");
    assert!(until_holds(&t, &item, &returned(), &retry, &templar).unwrap());

    let mut t = task("ns.coll.facts");
    t.register = Some("x".into());
    t.failed_when = vec!["x is not mapping".into()];
    assert!(!finish(&t, &item, returned(), &templar).failed());

    let seen = || result(json!({"changed": false, "ansible_facts": {"seen": 1}}));
    let mut t = task("set_fact");
    t.failed_when = vec!["seen is not defined".into()];
    assert!(!finish(&t, &item, seen(), &templar).failed());
    t.delegate_to = Some("h2".into());
    t.delegate_facts = Some(true);
    assert!(finish(&t, &item, seen(), &templar).failed());
}

/// A `changed_when` or `failed_when` that cannot be evaluated keeps the module's keys, sets
/// `failed` and files the error under `_when_result`.
#[test]
fn a_condition_that_cannot_be_evaluated_keeps_the_result() {
    let templar = templar();
    let item = bare_item();
    let ran = || result(json!({"changed": true, "msg": "", "rc": 0}));
    let mut t = task("command");
    t.failed_when = vec!["nosuch == 1".into()];
    let r = finish(&t, &item, ran(), &templar);
    assert!(r.failed(), "{:?}", r.0);
    assert_eq!(r.0["rc"], json!(0), "{:?}", r.0);
    assert_eq!(r.0["msg"], json!(""), "{:?}", r.0);
    assert_eq!(r.0["exception"], json!("(traceback unavailable)"));
    let why = r.0["failed_when_result"].as_str().unwrap_or_default();
    assert!(
        why.starts_with("Error while evaluating conditional: "),
        "{why}"
    );

    let mut t = task("command");
    t.changed_when = vec!["nosuch.key".into()];
    t.failed_when = vec!["false".into()];
    let r = finish(&t, &item, ran(), &templar);
    assert!(r.failed(), "{:?}", r.0);
    assert!(!r.0.contains_key("failed_when_result"), "{:?}", r.0);
    assert_eq!(r.0["changed"], json!(true), "{:?}", r.0);
    let why = r.0["changed_when_result"].as_str().unwrap_or_default();
    assert!(
        why.starts_with("Error while evaluating conditional: "),
        "{why}"
    );

    let mut t = task("ping");
    t.failed_when = vec!["1 +* 2".into()];
    let r = finish(
        &t,
        &item,
        result(json!({"changed": false, "ping": "pong"})),
        &templar,
    );
    assert_eq!(r.0["ping"], json!("pong"), "{:?}", r.0);
    assert_eq!(
        r.0["msg"],
        json!("Task failed: Action failed: Unknown error.")
    );
    let why = r.0["failed_when_result"].as_str().unwrap_or_default();
    assert!(why.starts_with("Syntax error in template: "), "{why}");
}

/// A `failed_when` failure whose result has no `msg` gets the reference's unknown-error `msg`
/// and an `exception`; an existing `msg` stays.
#[test]
fn a_failed_when_failure_with_no_msg_is_an_unknown_error() {
    let templar = templar();
    let item = bare_item();
    let mut t = task("set_fact");
    t.failed_when = vec!["true".into()];
    let r = finish(
        &t,
        &item,
        result(json!({"changed": false, "ansible_facts": {"a": 1}})),
        &templar,
    );
    assert_eq!(
        r.0[crate::render::ERROR_KEY],
        json!("Task failed: Action failed: Unknown error.")
    );
    assert_eq!(
        super::super::report::without_error(&r).0,
        json!({"ansible_facts": {"a": 1}, "changed": false, "exception": "(traceback unavailable)", "failed": true, "failed_when_result": true, "msg": "Task failed: Action failed: Unknown error."})
            .as_object()
            .unwrap()
            .clone()
    );
    let r = finish(
        &t,
        &item,
        result(json!({"changed": true, "msg": "", "rc": 0})),
        &templar,
    );
    assert_eq!(r.0["msg"], json!(""), "{:?}", r.0);
    // A module result that carried a truthy `exception` of its own reads `Module failed`; a
    // controller-side action's never does.
    let mut module = task("ping");
    module.failed_when = vec!["true".into()];
    let r = finish(
        &module,
        &item,
        result(json!({"changed": false, "exception": "(traceback unavailable)"})),
        &templar,
    );
    assert_eq!(
        r.0["msg"],
        json!("Task failed: Module failed: Unknown error."),
        "{:?}",
        r.0
    );

    let mut t = task("command");
    t.failed_when = vec!["false".into()];
    let r = finish(
        &t,
        &item,
        result(
            json!({"changed": true, "exception": "(traceback unavailable)", "failed": true, "msg": "non-zero return code", "rc": 1}),
        ),
        &templar,
    );
    assert!(!r.0.contains_key("exception"), "{:?}", r.0);
    assert_eq!(
        r.0["failed_when_suppressed_exception"],
        json!("(traceback unavailable)")
    );

    let mut t = task("ping");
    t.register = Some("a".into());
    t.failed_when = vec!["true".into()];
    t.until = vec!["a.msg is defined".into()];
    t.retries = Some("1".into());
    let retry = retry_plan(&t, Some(&item), &templar)
        .unwrap()
        .expect("a retry plan");
    let pong = || result(json!({"changed": false, "ping": "pong"}));
    assert!(matches!(
        judge_attempt(&t, &item, pong(), 1, &retry, &templar),
        Attempt::Again(..)
    ));
    let Attempt::Done(r) = judge_attempt(&t, &item, pong(), 2, &retry, &templar) else {
        panic!("the last attempt ends the loop");
    };
    assert_eq!(
        r.0["msg"],
        json!("Task failed: Action failed: Unknown error.")
    );
    assert_eq!(r.0["exception"], json!("(traceback unavailable)"));
}

/// A collection module given a free-form string that is not `key=value` fails each item it
/// runs, in the reference's words, before anything is sent.
#[test]
fn a_raw_argument_on_a_collection_module_fails_the_task_when_it_runs() {
    let templar = templar();
    let mut t = task("ns.coll.m");
    t.args.insert("v".into(), json!("1"));
    t.args.insert("_raw_params".into(), json!("stray"));
    t.failed_when = vec!["false".into()];
    let mut items = [bare_item(), bare_item()];
    items[1].skipped = Some(TaskResult::default());
    let mut results = vec![None, None];
    let sent = step_tasks(&t, &items, &Ok(None), &mut results);
    assert!(sent.is_empty(), "{sent:?}");
    let refused = results[0].clone().expect("the item that runs reports");
    assert!(results[1].is_none(), "a skipped item keeps its skip");
    let expected = json!({"changed": false, "exception": "(traceback unavailable)", "failed": true, "msg": "Task failed: Action 'ns.coll.m' does not support raw params."});
    assert_eq!(Value::Object(refused.0.clone()), expected);
    let judged = finish(&t, &items[0], refused, &templar);
    assert_eq!(Value::Object(judged.0), expected);

    for (module, action) in [
        ("ping", "ansible.builtin.ping"),
        ("ansible.legacy.ping", "ansible.builtin.ping"),
        ("debug", "ansible.builtin.debug"),
        ("copy", "ansible.builtin.copy"),
    ] {
        let mut t = task(module);
        t.args.insert("_raw_params".into(), json!("foo"));
        let refused = raw_params_refusal(&t).expect(module);
        assert_eq!(
            refused.0["msg"],
            json!(format!(
                "Task failed: Action '{action}' does not support raw params."
            ))
        );
    }
    for (module, raw) in [
        ("ansible.windows.win_shell", "Get-Service foo"),
        ("ns.coll.m", "{{ d }}"),
        ("ping", "{{ d }}"),
        ("command", "echo hi"),
        ("set_fact", "foo"),
        ("include_vars", "v.yml"),
    ] {
        let mut t = task(module);
        t.args.insert("_raw_params".into(), json!(raw));
        assert!(raw_params_refusal(&t).is_none(), "{module}: {raw}");
    }
}

/// A `run_once` step's registered variable and facts are written for every live host of the
/// batch; every other step writes for its own host alone.
#[test]
fn a_run_once_step_writes_its_result_for_the_whole_batch() {
    let live = ["h1".to_string(), "h2".to_string()];
    let mut t = task("command");
    assert_eq!(fact_targets(&t, "h1", &live), ["h1"]);
    t.run_once = Some(true);
    assert_eq!(fact_targets(&t, "h1", &live), live);
}

/// A payload the agent already holds is not sent again, and a payload it does not hold is sent
/// once for the whole link rather than once per batch.
#[tokio::test]
async fn a_payload_travels_once_per_link() {
    let mut logs = Vec::new();
    let mut agent = FakeAgent::answering(vec![state("ab", false), state("ab", true)]);
    ensure_blob(&mut agent, "h1", "ab", "UEsD", &mut logs)
        .await
        .expect("the agent stored it");
    assert_eq!(agent.puts(), 1);
    ensure_blob(&mut agent, "h1", "ab", "UEsD", &mut logs)
        .await
        .expect("it is already there");
    assert_eq!(agent.puts(), 1, "{:?}", agent.sent);

    let mut held = FakeAgent::answering(vec![state("cd", true)]);
    ensure_blob(&mut held, "h1", "cd", "UEsD", &mut logs)
        .await
        .expect("the agent already holds it");
    assert_eq!(held.puts(), 0, "{:?}", held.sent);
}

/// A file is staged, then put at its path; the agent's refusal comes back in its words.
#[tokio::test]
async fn a_file_is_staged_then_put_and_a_refusal_is_the_agents_own() {
    let hash = blake3::hash(b"abc").to_hex().to_string();
    let done = |changed, error: Option<&str>| FromAgent::PutFileDone {
        hash: hash.clone(),
        changed,
        error: error.map(Into::into),
    };
    let mut agent = FakeAgent::answering(vec![state(&hash, true), done(true, None)]);
    let changed = put_file(
        &mut agent,
        "h1",
        b"abc",
        "/etc/x",
        Some(0o644),
        &mut Vec::new(),
    )
    .await
    .expect("the agent put it");
    assert!(changed);
    assert_eq!(
        agent.sent[1..],
        [ToAgent::PutFile {
            hash: hash.clone(),
            path: "/etc/x".into(),
            mode: Some(0o644),
        }]
    );
    let mut agent = FakeAgent::answering(vec![
        state(&hash, true),
        done(
            false,
            Some("putting the file at x: the path is not absolute"),
        ),
    ]);
    let err = put_file(&mut agent, "h1", b"abc", "x", None, &mut Vec::new())
        .await
        .unwrap_err();
    assert_eq!(err, "putting the file at x: the path is not absolute");
    let mut agent = FakeAgent::answering(vec![state(&hash, false)]);
    let err = put_file(&mut agent, "h1", b"abc", "/x", None, &mut Vec::new())
        .await
        .unwrap_err();
    assert!(
        err.starts_with("the agent could not store the file for '/x'"),
        "{err}"
    );
}

/// An agent that never answers the put is given up on, so its late answer cannot desync the next task.
#[tokio::test]
async fn a_put_the_agent_never_answers_abandons_the_link() {
    let hash = blake3::hash(b"abc").to_hex().to_string();
    let mut agent = FakeAgent {
        hangs_when_empty: true,
        ..FakeAgent::answering(vec![state(&hash, true)])
    };
    let err = put_file_within(
        &mut agent,
        "h1",
        b"abc",
        "/x",
        None,
        &mut Vec::new(),
        Duration::from_millis(50),
    )
    .await
    .unwrap_err();
    assert!(
        err.starts_with("the agent did not put the file at '/x'"),
        "{err}"
    );
    assert!(agent.abandoned);
}

/// A union goes up deflated, a small file raw, and a blob the agent does not keep fails with one sentence.
#[tokio::test]
async fn a_blob_goes_up_in_the_form_its_size_calls_for() {
    {
        let zip = b"PK\x03\x04 a stored zip ".repeat(1000);
        let hash = blake3::hash(&zip).to_hex().to_string();
        let b64 = volant_protocol::encoding::b64_encode(&zip);
        let mut agent = FakeAgent::answering(vec![state(&hash, false), state(&hash, true)]);
        ensure_blob(&mut agent, "h1", &hash, &b64, &mut Vec::new())
            .await
            .expect("the agent stored it");
        assert_eq!(
            agent.sent,
            [
                ToAgent::HasBlob { hash: hash.clone() },
                ToAgent::PutBlob {
                    hash: hash.clone(),
                    len: zip.len() as u64,
                    encoding: volant_protocol::BlobEncoding::Deflate,
                    staged: false,
                },
            ]
        );
        assert_eq!(agent.frames.len(), 1);
        let (after, bytes) = &agent.frames[0];
        assert_eq!(*after, 2, "the frame follows its put_blob");
        assert!(bytes.len() < zip.len() / 10, "{} bytes", bytes.len());
        assert_eq!(
            volant_protocol::encoding::inflate(bytes, zip.len()).unwrap(),
            zip
        );
    }
    {
        let bytes = [7u8; 100];
        let blob = crate::action_plugins::files::blob_of("small", &bytes).expect("it fits");
        let mut agent = FakeAgent::answering(vec![state(&blob.hash, true)]);
        stage_files(
            &mut agent,
            "h1",
            &[("src".into(), blob.clone())],
            &mut Vec::new(),
        )
        .await
        .expect("the agent stored it");
        assert_eq!(
            agent.sent,
            [ToAgent::PutBlob {
                hash: blob.hash.clone(),
                len: 100,
                encoding: volant_protocol::BlobEncoding::Raw,
                staged: true,
            }]
        );
        assert_eq!(agent.frames, [(1, bytes.to_vec())]);
    }
    {
        let mut agent = FakeAgent::answering(vec![state("ab", false), state("ab", false)]);
        let err = ensure_blob(&mut agent, "h1", "ab", "UEsDBA==", &mut Vec::new())
            .await
            .expect_err("the agent refused it");
        assert_eq!(
            err,
            "the agent refused the module payload ab; it holds no payload to run this task from"
        );
        assert_eq!(agent.frames, [(2, b"PK\x03\x04".to_vec())]);
    }
}

/// A payload the agent refuses fails the batch, and the next batch of that host does not send
/// it again blindly.
#[tokio::test]
async fn a_refused_payload_fails_the_batch_and_is_not_sent_again() {
    let mut logs = Vec::new();
    let mut agent = FakeAgent::answering(vec![state("ab", false), state("ab", false)]);
    let err = ensure_blob(&mut agent, "h1", "ab", "UEsD", &mut logs)
        .await
        .expect_err("the agent refused it");
    assert!(err.contains("ab"), "{err}");
    let again = ensure_blob(&mut agent, "h1", "ab", "UEsD", &mut logs)
        .await
        .expect_err("the refusal is remembered");
    assert_eq!(again, err);
    assert_eq!(agent.puts(), 1, "{:?}", agent.sent);
}

/// An item that cannot be sent still reports, so the step gets a task line.
#[test]
fn an_item_that_cannot_be_sent_still_reports() {
    let items = [bare_item(), bare_item()];
    let refused: Result<Option<(&ModulePayload, &str)>, TaskResult> = Err(TaskResult::failed_with(
        "The module interpreter was not found.",
    ));
    let mut results = vec![None, None];
    let sent = step_tasks(&task("ping"), &items, &refused, &mut results);
    assert!(sent.is_empty(), "nothing can be sent: {sent:?}");
    for slot in &results {
        let result = slot.as_ref().expect("every item of the step reports");
        assert!(result.failed(), "{result:?}");
    }
}

/// A step that can be sent leaves the results alone and carries every item that is not skipped,
/// keyed by its own index.
#[test]
fn a_step_that_travels_carries_the_items_that_are_not_skipped() {
    let mut items = [bare_item(), bare_item()];
    items[0].skipped = Some(TaskResult::default());
    let mut results = vec![None, None];
    let sent = step_tasks(&task("command"), &items, &Ok(None), &mut results);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, 1, "the item's own index travels with it");
    assert!(results.iter().all(Option::is_none), "{results:?}");
}

/// A module that needs a payload and has none is refused by name rather than sent.
#[test]
fn a_module_with_no_payload_built_for_it_is_refused_by_name() {
    let failure = python_for("lineinfile", None, None)
        .expect_err("nothing was built for a module the union never saw");
    let msg = failure.0.get("msg").and_then(Value::as_str).unwrap_or("");
    assert!(msg.contains("lineinfile"), "{msg}");
    assert!(msg.contains("python payload"), "{msg}");
    assert!(
        python_for("command", None, None)
            .expect("a native module needs none")
            .is_none()
    );
}

/// The interpreter is the host's, read under the reference's name; discovery modes and blanks name nobody.
#[test]
fn the_interpreter_comes_from_the_host_and_is_read_under_the_reference_name() {
    {
        let mut item = bare_item();
        item.vars = hvars(json!({"ansible_python_interpreter": "/usr/bin/python3"}));
        let items = [item];
        assert_eq!(
            requested_interpreter(running_host_vars(None, &items)),
            Some("/usr/bin/python3".to_string())
        );
        let delegate = (
            "build-host".to_string(),
            hvars(json!({"ansible_python_interpreter": "/opt/py311/bin/python"})),
        );
        assert_eq!(
            requested_interpreter(running_host_vars(Some(&delegate), &items)),
            Some("/opt/py311/bin/python".to_string())
        );
    }
    {
        for mode in ["auto", "auto_legacy", "auto_silent", "auto_legacy_silent"] {
            assert_eq!(
                requested_interpreter(&vars(json!({ "ansible_python_interpreter": mode }))),
                None,
                "{mode} is a discovery mode, not a path"
            );
        }
        assert_eq!(
            requested_interpreter(&vars(json!({"ansible_python_interpreter": "   "}))),
            None
        );
        assert_eq!(
            requested_interpreter(&vars(
                json!({"ansible_python_interpreter": " /usr/bin/python3 "})
            )),
            Some("/usr/bin/python3".to_string())
        );
    }
    {
        assert_eq!(requested_interpreter(&vars(json!({}))), None);
        assert_eq!(
            requested_interpreter(&vars(json!({"ansible_python_interpreter": "/opt/py"}))),
            Some("/opt/py".to_string())
        );
    }
}

/// The closing line is worked out only for a batch with a task that wants a terminal.
#[tokio::test]
async fn the_closing_line_is_asked_for_only_by_a_task_that_wants_one() {
    let (_stop_tx, mut stop) = watch::channel(false);
    let mut stop_broken = false;
    let mut logs = Vec::new();
    for (module, asked) in [("command", 0), ("raw", 1)] {
        let mut agent = FakeAgent::answering(vec![FromAgent::BatchDone {
            batch: 1,
            outcome: BatchOutcome::Completed,
        }]);
        let tasks = vec![protocol_task(&task(module), &bare_item(), None)];
        let (_, ended) = send_batch(
            &mut agent,
            "h",
            1,
            tasks,
            &mut stop,
            &mut stop_broken,
            &mut logs,
        )
        .await;
        assert!(matches!(ended, Ok(BatchOutcome::Completed)));
        assert_eq!(agent.close_asked, asked, "{module}");
    }
}

/// Only the controller asks for a terminal, and a link that gives none drops the marker.
#[rstest::rstest]
#[case::raw_over_ssh("raw", None, Some("Shared connection to h closed."), Some(json!("Shared connection to h closed.")))]
#[case::raw_over_local("raw", None, None, None)]
#[case::raw_asking_for_its_own("ansible.builtin.raw", Some(json!("x")), None, None)]
#[case::command_asking("command", Some(json!(true)), Some("Connection to h closed."), None)]
fn a_terminal_is_given_by_the_controller_alone(
    #[case] module: &str,
    #[case] written: Option<Value>,
    #[case] close: Option<&str>,
    #[case] sent: Option<Value>,
) {
    let mut item = bare_item();
    if let Some(written) = written {
        item.args.insert(TTY_KEY.into(), written);
    }
    let mut tasks = vec![protocol_task(&task(module), &item, None)];
    give_terminals(&mut tasks, close.map(str::to_string));
    assert_eq!(tasks[0].args.get(TTY_KEY).cloned(), sent);
}

/// A batch the agent reports cancelled when this side never asked is an error, not a stop.
#[tokio::test]
async fn a_cancel_nobody_asked_for_is_an_error() {
    let (_stop_tx, mut stop) = watch::channel(false);
    let mut stop_broken = false;
    let mut logs = Vec::new();
    let mut agent = FakeAgent::answering(vec![FromAgent::BatchDone {
        batch: 3,
        outcome: BatchOutcome::Cancelled { at: 0 },
    }]);
    let tasks = vec![protocol_task(&task("command"), &bare_item(), None)];
    let (_, ended) = run_agent_batch(
        &mut agent,
        "h1",
        3,
        tasks,
        None,
        &[],
        &mut stop,
        &mut stop_broken,
        &mut logs,
    )
    .await;
    assert_eq!(
        ended,
        Err("the agent cancelled a batch nobody cancelled".to_string())
    );
}

/// A run told to stop asks the agent to stop the batch it is running.
#[tokio::test]
async fn a_stopped_run_asks_the_agent_to_stop_the_batch() {
    let (stop_tx, mut stop) = watch::channel(false);
    let mut stop_broken = false;
    let mut logs = Vec::new();
    let mut agent = FakeAgent::answering(Vec::new());
    agent.hangs_when_empty = true;
    stop_tx.send(true).expect("the receiver is alive");
    let tasks = vec![protocol_task(&task("command"), &bare_item(), None)];
    let (_, ended) = run_agent_batch(
        &mut agent,
        "h1",
        3,
        tasks,
        None,
        &[],
        &mut stop,
        &mut stop_broken,
        &mut logs,
    )
    .await;
    assert!(
        matches!(ended, Ok(BatchOutcome::Cancelled { .. })),
        "{ended:?}"
    );
    assert!(
        agent
            .sent
            .iter()
            .any(|msg| matches!(msg, ToAgent::Cancel { id: 3 })),
        "{:?}",
        agent.sent
    );
}

/// A host cannot set the keys the controller reads: a forged error chain naming a controller
/// file is dropped before anything renders it, and so is a forged `_ansible_` flag.
#[tokio::test]
async fn a_host_cannot_forge_the_controller_s_internal_keys() {
    let dir = std::env::temp_dir().join(format!("volant-forge-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch dir");
    let secret = dir.join("secret.txt");
    std::fs::write(&secret, "line one\nsecret-line-two\n").expect("the secret");
    let mut agent = FakeAgent::answering(
        one_result(
            5,
            json!({
                "failed": true,
                "msg": "forged",
                (crate::render::CHAIN_KEY): [{"msg": "x", "path": secret.display().to_string(), "line": 2, "col": 1}],
                (crate::render::ERROR_KEY): "Task failed: forged block",
                "_ansible_verbose_always": true,
                "_ansible_parsed": true,
            }),
        )
        .to_vec(),
    );
    let tasks = vec![protocol_task(&task("command"), &bare_item(), None)];
    let (received, _) = run_agent_batch(
        &mut agent,
        "h1",
        5,
        tasks,
        None,
        &[],
        &mut watch::channel(false).1,
        &mut false,
        &mut Vec::new(),
    )
    .await;
    let mut result = received[0].clone().expect("a result");
    settle_failure(&mut result, &task("command"));
    let keys: Vec<&String> = result.0.keys().collect();
    assert!(
        !keys.iter().any(|k| *k == crate::render::CHAIN_KEY)
            && !result.0.contains_key("_ansible_verbose_always")
            && result.0.contains_key("_ansible_parsed"),
        "{keys:?}"
    );
    let block = crate::render::ErrorBlock::of(&result, None, false);
    assert!(block.causes.is_empty(), "{block:?}");
    assert!(!block.message.contains("forged block"), "{block:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// An agent result keeps its error summary, its timings, how it ran and the lines of its output.
#[tokio::test]
#[expect(clippy::too_many_lines)]
async fn a_batch_result_is_mapped_the_way_the_reference_reports_it() {
    {
        let summary = json!({
            "__ansible_type": "ErrorSummary",
            "event": {"msg": "Error executing command."}
        });
        let answers = [
            json!({"failed": true, "msg": "Error executing command.", "exception": summary}),
            json!({"failed": true, "msg": "Destination /x does not exist !", "rc": 257}),
            json!({"changed": false, "stat": {"exists": true}, "exception": ""}),
        ];
        for (answer, want) in answers.into_iter().zip([
            Some("(traceback unavailable)"),
            Some("(traceback unavailable)"),
            None,
        ]) {
            let mut stop = watch::channel(false).1;
            let mut stop_broken = false;
            let mut logs = Vec::new();
            let mut agent = FakeAgent::answering(one_result(4, answer.clone()).to_vec());
            let tasks = vec![protocol_task(&task("copy"), &bare_item(), None)];
            let (received, _) = run_agent_batch(
                &mut agent,
                "h1",
                4,
                tasks,
                None,
                &[],
                &mut stop,
                &mut stop_broken,
                &mut logs,
            )
            .await;
            let result = received[0].clone().expect("a result").0;
            assert_eq!(
                result.get("exception").and_then(Value::as_str),
                want,
                "{answer} gave {result:?}"
            );
            if want.is_none() {
                assert!(!result.contains_key("exception"), "{result:?}");
            }
        }
    }
    {
        let ran = volant_protocol::Ran {
            path: volant_protocol::ExecPath::Native,
            reason: None,
            micros: u64::MAX,
            fork_micros: None,
            import_micros: None,
            module_micros: None,
        };
        let answer = |index| FromAgent::TaskResult {
            batch: 4,
            index,
            result: TaskResult(vars(json!({"changed": false}))),
            ran: Some(ran.clone()),
        };
        let mut agent = FakeAgent::answering(vec![
            answer(0),
            answer(1),
            FromAgent::BatchDone {
                batch: 4,
                outcome: BatchOutcome::Completed,
            },
        ]);
        let tasks = vec![
            protocol_task(&task("stat"), &bare_item(), None),
            protocol_task(&task("stat"), &bare_item(), None),
        ];
        let (received, _) = run_agent_batch(
            &mut agent,
            "h1",
            4,
            tasks,
            None,
            &[],
            &mut watch::channel(false).1,
            &mut false,
            &mut Vec::new(),
        )
        .await;
        assert!(received.iter().all(Option::is_some));
        assert_eq!(agent.ledger.ran.len(), 2);
        assert_eq!(agent.ledger.wire_micros, 0);
    }
    {
        let ran = volant_protocol::Ran {
            path: volant_protocol::ExecPath::Fallback,
            reason: Some("validate".into()),
            micros: 40,
            fork_micros: None,
            import_micros: None,
            module_micros: None,
        };
        let mut agent = FakeAgent::answering(vec![
            FromAgent::TaskResult {
                batch: 4,
                index: 1,
                result: TaskResult(vars(json!({"changed": false}))),
                ran: Some(ran.clone()),
            },
            FromAgent::TaskResult {
                batch: 4,
                index: 0,
                result: TaskResult(vars(json!({"changed": false}))),
                ran: None,
            },
            FromAgent::BatchDone {
                batch: 4,
                outcome: BatchOutcome::Completed,
            },
        ]);
        let tasks = vec![
            protocol_task(&task("command"), &bare_item(), None),
            protocol_task(&task("stat"), &bare_item(), None),
        ];
        let (received, _) = run_agent_batch(
            &mut agent,
            "h1",
            4,
            tasks,
            None,
            &[],
            &mut watch::channel(false).1,
            &mut false,
            &mut Vec::new(),
        )
        .await;
        assert!(received.iter().all(Option::is_some));
        assert_eq!(
            agent.ledger.ran,
            [
                (1, "stat".to_string(), Some(ran)),
                (0, "command".to_string(), None)
            ]
        );
    }
    {
        let mut stop = watch::channel(false).1;
        let mut stop_broken = false;
        let mut logs = Vec::new();
        let mut agent = FakeAgent::answering(
            one_result(
                4,
                json!({
                    "stdout": "a\nb\r\nc\rd\u{2028}e\x0bf\x1cg\u{85}h\u{2029}i\n",
                    "stderr": "",
                    "rc": 0
                }),
            )
            .to_vec(),
        );
        let tasks = vec![protocol_task(&task("pip"), &bare_item(), None)];
        let (received, _) = run_agent_batch(
            &mut agent,
            "h1",
            4,
            tasks,
            None,
            &[],
            &mut stop,
            &mut stop_broken,
            &mut logs,
        )
        .await;
        let result = received[0].clone().expect("a result").0;
        assert_eq!(
            result["stdout_lines"],
            json!(["a", "b", "c", "d", "e", "f", "g", "h", "i"])
        );
        assert_eq!(result["stderr_lines"], json!([]));

        let mut kept = TaskResult(vars(
            json!({"stdout": "one", "stdout_lines": ["kept"], "stderr": 3}),
        ));
        add_output_lines(&mut kept);
        assert_eq!(kept.0["stdout_lines"], json!(["kept"]));
        assert!(!kept.0.contains_key("stderr_lines"), "{:?}", kept.0);

        // `data.get('stdout') or ''`: a value Python reads as false gives an empty list.
        for falsy in [json!(null), json!(false), json!(0)] {
            let mut empty = TaskResult(vars(json!({"stdout": falsy, "stderr": falsy})));
            add_output_lines(&mut empty);
            assert_eq!(empty.0["stdout_lines"], json!([]), "{falsy}");
            assert_eq!(empty.0["stderr_lines"], json!([]), "{falsy}");
        }
    }
}

/// A batch reads only the frames of its own id.
#[tokio::test]
async fn a_batch_ignores_the_frames_of_another_batch() {
    let mut stop = watch::channel(false).1;
    let mut stop_broken = false;
    let mut logs = Vec::new();
    let mut answers = one_result(1, json!({"msg": "probe"})).to_vec();
    answers.extend(one_result(2, json!({"msg": "mine"})));
    let mut agent = FakeAgent::answering(answers);
    let tasks = vec![protocol_task(&task("ping"), &bare_item(), None)];
    let (received, ended) = run_agent_batch(
        &mut agent,
        "h1",
        2,
        tasks,
        None,
        &[],
        &mut stop,
        &mut stop_broken,
        &mut logs,
    )
    .await;
    assert!(matches!(ended, Ok(BatchOutcome::Completed)), "{ended:?}");
    let result = received[0].clone().expect("a result").0;
    assert_eq!(result["msg"], json!("mine"));

    // Batch 2 ends without a result of its own: the stale one is not taken for it.
    let mut answers = one_result(1, json!({"msg": "probe"})).to_vec();
    answers.push(FromAgent::BatchDone {
        batch: 2,
        outcome: BatchOutcome::Completed,
    });
    let mut agent = FakeAgent::answering(answers);
    let tasks = vec![protocol_task(&task("ping"), &bare_item(), None)];
    let (received, _) = run_agent_batch(
        &mut agent,
        "h1",
        2,
        tasks,
        None,
        &[],
        &mut stop,
        &mut stop_broken,
        &mut logs,
    )
    .await;
    assert!(received[0].is_none(), "{received:?}");
}

/// One item of a task backed by the plugin `kind`, run the way the driver runs it, against
/// the scripted `agent`: its arguments as `prepare` rendered them, the variables of the host
/// the module runs on, the interpreters that host's agent reported. The item's result, or the
/// outcome that ended the host's batch, and the warnings the plugin raised.
async fn plugin_item(
    kind: crate::action_plugins::Kind,
    args: Value,
    running: Value,
    agent: &mut FakeAgent,
    interpreters: &[String],
    stop: &mut watch::Receiver<bool>,
) -> (
    Result<TaskResult, Result<BatchOutcome, String>>,
    Vec<String>,
) {
    plugin_item_on(
        kind,
        args,
        running,
        agent,
        &mut NoRelink,
        false,
        None,
        interpreters,
        stop,
    )
    .await
}

/// [`plugin_item`], with a way to reconnect and a choice of whether the link is local.
#[expect(
    clippy::too_many_arguments,
    reason = "plugin_item's, plus the two it fixes"
)]
async fn plugin_item_on<R: Relink<FakeAgent>>(
    kind: crate::action_plugins::Kind,
    args: Value,
    running: Value,
    agent: &mut FakeAgent,
    relink: &mut R,
    local: bool,
    timeout: Option<u64>,
    interpreters: &[String],
    stop: &mut watch::Receiver<bool>,
) -> (
    Result<TaskResult, Result<BatchOutcome, String>>,
    Vec<String>,
) {
    let t = task(match kind {
        crate::action_plugins::Kind::Copy => "copy",
        crate::action_plugins::Kind::Dnf => "dnf",
        crate::action_plugins::Kind::Fetch => "fetch",
        crate::action_plugins::Kind::Package => "package",
        crate::action_plugins::Kind::Reboot => "reboot",
        crate::action_plugins::Kind::Service => "service",
        crate::action_plugins::Kind::Template => "template",
        crate::action_plugins::Kind::Unarchive => "unarchive",
        crate::action_plugins::Kind::Bridged(name) => name,
    });
    let t = PlayTask { timeout, ..t };
    let item = Item {
        args: vars(args),
        ..bare_item()
    };
    let running = HostVars {
        map: vars(running),
        ..HostVars::default()
    };
    let templar = Templar::new(PathBuf::from("."));
    let origin = crate::compile::Origin::default();
    let union = union_of("ab", crate::action_plugins::modules_for(kind));
    let mut warnings = Vec::new();
    let mut stop_broken = false;
    let mut logs = Vec::new();
    let mut batch_id = 0;
    let ran = {
        let mut plugin = crate::action_plugins::start(
            kind,
            crate::action_plugins::Context {
                args: &item.args,
                args_untrusted: &item.args_untrusted,
                running_vars: &running,
                delegated: false,
                escalated: false,
                local,
                item_vars: &item.vars,
                templar: &templar,
                origin: &origin,
                playbook_dir: Path::new("."),
                warnings: &mut warnings,

                environment: None,
                timeout: None,
                check_mode: false,
                diff: false,
            },
        );
        run_plugin_item(
            agent,
            relink,
            "h1",
            &mut batch_id,
            plugin.as_mut(),
            &t,
            &item,
            Some(&union),
            interpreters,
            None,
            stop,
            &mut stop_broken,
            &mut logs,
        )
        .await
    };
    (ran, warnings)
}

fn python3() -> Vec<String> {
    vec!["/usr/bin/python3".to_string()]
}

/// A plugin's result shaped as an unreachable host ends the item like a lost link; a failed
/// one, or one that is not unreachable, stays the item's result.
#[rstest::rstest]
#[case::unreachable(json!({"changed": false, "failed": false, "msg": "boom", "unreachable": true}), true)]
#[case::gone_and_failed(json!({"failed": true, "msg": "boom", "unreachable": true}), false)]
#[case::plain(json!({"changed": true}), false)]
fn an_unreachable_result_ends_the_item(#[case] result: Value, #[case] ends: bool) {
    let Value::Object(map) = result else {
        unreachable!("the rows are objects")
    };
    let ran = unreachable_ending(TaskResult(map));
    assert_eq!(
        matches!(ran, Err(Err(ref msg)) if ends && msg == "boom"),
        ends,
        "{ran:?}"
    );
    assert_eq!(ran.is_ok(), !ends);
}

/// A `package` task reaches the host as the module its manager names, asking `setup` for
/// `ansible_pkg_mgr` alone when facts are missing.
#[tokio::test]
async fn a_package_task_runs_the_module_its_manager_names() {
    use crate::action_plugins::Kind;
    let mut stop = watch::channel(false).1;
    // Measured on ansible-core 2.19.12 with facts gathered: `Running ansible.legacy.apt` and no
    // `AnsiballZ_setup.py`.
    let mut agent = FakeAgent::answering(
        [
            vec![state("ab", true)],
            one_result(1, json!({"cache_updated": false, "changed": false})).to_vec(),
        ]
        .concat(),
    );
    let (ran, _) = plugin_item(
        Kind::Package,
        json!({"name": "bash", "use": "auto"}),
        json!({"ansible_facts": {"pkg_mgr": "apt"}}),
        &mut agent,
        &python3(),
        &mut stop,
    )
    .await;
    let sent = batches_sent(&agent);
    assert_eq!(sent.len(), 1, "one frame and no setup: {:?}", agent.sent);
    assert_eq!(sent[0].len(), 1, "{:?}", sent[0]);
    assert_eq!(sent[0][0].module, "apt");
    assert_eq!(
        Value::Object(sent[0][0].args.clone()),
        json!({"name": "bash"}),
        "`use` is the plugin's and never reaches the module"
    );
    assert_eq!(
        sent[0][0].payload.as_ref().map(|p| p.module_fqn.as_str()),
        Some("ansible.modules.apt"),
        "the payload is the union's entry for the module the plugin chose"
    );
    assert!(!ran.expect("the item finished").failed());

    // Measured without facts: `AnsiballZ_setup.py`, then `Running ansible.legacy.apt`.
    let mut agent = FakeAgent::answering(
        [
            vec![state("ab", true)],
            one_result(
                1,
                json!({"ansible_facts": {"ansible_pkg_mgr": "apt"}, "changed": false}),
            )
            .to_vec(),
            one_result(2, json!({"cache_updated": false, "changed": false})).to_vec(),
        ]
        .concat(),
    );
    let (ran, _) = plugin_item(
        Kind::Package,
        json!({"name": "bash"}),
        json!({}),
        &mut agent,
        &python3(),
        &mut stop,
    )
    .await;
    let sent = batches_sent(&agent);
    let modules: Vec<&str> = sent.iter().map(|b| b[0].module.as_str()).collect();
    assert_eq!(modules, ["setup", "apt"], "{:?}", agent.sent);
    assert_eq!(
        Value::Object(sent[0][0].args.clone()),
        json!({"filter": ["ansible_pkg_mgr"], "gather_subset": ["!all"]})
    );
    let result = ran.expect("the item finished");

    // The result goes where every remote result goes, and the `setup` it took to choose the
    // module leaves nothing there.
    let mut store = one_host_store();
    record_facts(&mut store, &["h1".to_string()], &[(None, result.clone())]);
    let host = store.for_host("h1", &crate::vars::Scope::default());
    assert!(!host.contains_key("ansible_pkg_mgr"), "{host:?}");
    assert!(
        host.get("ansible_facts")
            .and_then(|f| f.get("pkg_mgr"))
            .is_none(),
        "{host:?}"
    );
    // Measured: the task registers the module's result as it is.
    assert_eq!(
        Value::Object(result.0),
        json!({"cache_updated": false, "changed": false})
    );
}

fn msg(result: &TaskResult) -> &str {
    result.0.get("msg").and_then(Value::as_str).unwrap_or("")
}

/// The order the package manager is picked in, and the reference's sentences when it cannot be.
#[tokio::test]
#[expect(clippy::too_many_lines)]
async fn the_package_manager_is_picked_in_the_reference_s_order_and_words() {
    use crate::action_plugins::Kind;
    let mut stop = watch::channel(false).1;
    for (args, running, name) in [
        (
            json!({"name": "bash", "use": "nosuchmgr"}),
            json!({}),
            "nosuchmgr",
        ),
        (
            json!({"name": "bash"}),
            json!({"ansible_facts": {"pkg_mgr": "pacman"}}),
            "pacman",
        ),
    ] {
        let mut agent = FakeAgent::answering(Vec::new());
        let (ran, _) = plugin_item(
            Kind::Package,
            args,
            running,
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        let result = ran.expect("the item finished");
        assert!(result.failed(), "{result:?}");
        assert_eq!(
            msg(&result),
            format!("Could not find a matching action for the \"{name}\" package manager.")
        );
        assert!(agent.sent.is_empty(), "nothing is sent: {:?}", agent.sent);
    }

    let mut agent = FakeAgent::answering(
        [
            vec![state("ab", true)],
            one_result(1, json!({"changed": false})).to_vec(),
        ]
        .concat(),
    );
    let (ran, _) = plugin_item(
        Kind::Package,
        json!({"name": "bash"}),
        json!({"ansible_package_use": "dnf", "ansible_facts": {"pkg_mgr": "apt"}}),
        &mut agent,
        &python3(),
        &mut stop,
    )
    .await;
    assert!(!ran.expect("the item finished").failed());
    let sent = batches_sent(&agent);
    assert_eq!(sent.len(), 1, "{:?}", agent.sent);
    assert_eq!(sent[0][0].module, "dnf", "the variable beats the facts");

    let setup_failed = json!({
        "failed": true,
        "msg": "boom",
        "ansible_facts": {"ansible_pkg_mgr": "apt"},
    });
    let mut agent = FakeAgent::answering(
        [
            vec![state("ab", true)],
            one_result(1, setup_failed).to_vec(),
        ]
        .concat(),
    );
    let (ran, _) = plugin_item(
        Kind::Package,
        json!({"name": "bash"}),
        json!({}),
        &mut agent,
        &python3(),
        &mut stop,
    )
    .await;
    let result = ran.expect("the item finished");
    assert!(result.failed(), "{result:?}");
    assert_eq!(
        msg(&result),
        "Failed to fetch ansible_pkg_mgr to determine the package action backend: boom"
    );
    assert!(
        !result.0.contains_key("ansible_facts"),
        "a filtered setup is never kept, failed or not: {result:?}"
    );
    assert_eq!(batches_sent(&agent).len(), 1, "{:?}", agent.sent);

    let mut agent = FakeAgent::answering(
        [
            vec![state("ab", true)],
            one_result(1, json!({"ansible_facts": {}, "changed": false})).to_vec(),
        ]
        .concat(),
    );
    let (ran, _) = plugin_item(
        Kind::Package,
        json!({"name": "bash"}),
        json!({}),
        &mut agent,
        &python3(),
        &mut stop,
    )
    .await;
    let result = ran.expect("the item finished");
    assert_eq!(
        msg(&result),
        "Could not detect a package manager. Try using the \"use\" option."
    );
    assert_eq!(batches_sent(&agent).len(), 1, "{:?}", agent.sent);
}

/// A dnf task asks setup then runs its backend, or fails in the reference's words without dnf.
#[tokio::test]
#[expect(clippy::too_many_lines)]
async fn a_dnf_task_runs_the_backend_it_names_or_fails_in_the_reference_s_words() {
    {
        use crate::action_plugins::Kind;
        let mut stop = watch::channel(false).1;
        for (answer, module) in [
            ("dnf", "dnf"),
            ("dnf4", "dnf"),
            ("yum", "dnf"),
            ("yum4", "dnf"),
            ("dnf5", "dnf5"),
        ] {
            let mut agent = FakeAgent::answering(
                [
                    vec![state("ab", true)],
                    one_result(
                        1,
                        json!({"ansible_facts": {"ansible_pkg_mgr": answer}, "changed": false}),
                    )
                    .to_vec(),
                    one_result(2, json!({"changed": false, "results": []})).to_vec(),
                ]
                .concat(),
            );
            let (ran, _) = plugin_item(
                Kind::Dnf,
                json!({"name": "bash"}),
                json!({}),
                &mut agent,
                &python3(),
                &mut stop,
            )
            .await;
            let sent = batches_sent(&agent);
            let modules: Vec<&str> = sent.iter().map(|b| b[0].module.as_str()).collect();
            assert_eq!(modules, ["setup", module], "{answer}: {:?}", agent.sent);
            assert_eq!(
                Value::Object(sent[0][0].args.clone()),
                json!({"filter": ["ansible_pkg_mgr"], "gather_subset": ["!all"]})
            );
            assert_eq!(
                Value::Object(sent[1][0].args.clone()),
                json!({"name": "bash"})
            );
            assert_eq!(
                sent[1][0].payload.as_ref().map(|p| p.module_fqn.as_str()),
                Some(format!("ansible.modules.{module}").as_str())
            );
            let result = ran.expect("the item finished");
            assert_eq!(
                Value::Object(result.0.clone()),
                json!({"ansible_facts": {"pkg_mgr": answer}, "changed": false, "results": []}),
                "{answer}"
            );
            let mut store = one_host_store();
            record_facts(&mut store, &["h1".to_string()], &[(None, result)]);
            let scope = crate::vars::Scope::default();
            assert_eq!(
                store.for_host("h1", &scope)["ansible_facts"]["pkg_mgr"],
                json!(answer)
            );
            assert!(store.untrusted_of("h1", &scope).contains("ansible_facts"));
        }

        // An empty `use_backend`, as `"{{ backend | default('') }}"` renders, asks the host
        // rather than the facts, and installs with what it answers.
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(
                    1,
                    json!({"ansible_facts": {"ansible_pkg_mgr": "dnf"}, "changed": false}),
                )
                .to_vec(),
                one_result(2, json!({"changed": true})).to_vec(),
            ]
            .concat(),
        );
        let (ran, _) = plugin_item(
            Kind::Dnf,
            json!({"name": "bash", "use_backend": ""}),
            json!({"ansible_facts": {"pkg_mgr": "apt"}}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        let modules: Vec<&str> = batches_sent(&agent)
            .iter()
            .map(|b| b[0].module.as_str())
            .collect();
        assert_eq!(modules, ["setup", "dnf"], "{:?}", agent.sent);
        assert_eq!(
            Value::Object(batches_sent(&agent)[1][0].args.clone()),
            json!({"name": "bash"})
        );
        assert_eq!(
            Value::Object(ran.expect("the item finished").0),
            json!({"ansible_facts": {"pkg_mgr": "dnf"}, "changed": true})
        );

        // Gathered facts that name a backend ask nothing, and the result carries nothing more
        // than the module's; `use_backend` wins over the facts and never reaches the module
        // (measured: `Running ansible.legacy.dnf5 as the backend for the dnf action plugin`).
        for (args, running, module) in [
            (
                json!({"name": "bash"}),
                json!({"ansible_facts": {"pkg_mgr": "dnf5"}}),
                "dnf5",
            ),
            (
                json!({"name": "bash", "use_backend": "dnf5"}),
                json!({"ansible_facts": {"pkg_mgr": "apt"}}),
                "dnf5",
            ),
            (json!({"name": "bash", "use": "dnf4"}), json!({}), "dnf"),
        ] {
            let mut agent = FakeAgent::answering(
                [
                    vec![state("ab", true)],
                    one_result(1, json!({"changed": false})).to_vec(),
                ]
                .concat(),
            );
            let (ran, _) =
                plugin_item(Kind::Dnf, args, running, &mut agent, &python3(), &mut stop).await;
            let sent = batches_sent(&agent);
            assert_eq!(sent.len(), 1, "{:?}", agent.sent);
            assert_eq!(sent[0][0].module, module);
            assert_eq!(
                Value::Object(sent[0][0].args.clone()),
                json!({"name": "bash"})
            );
            assert_eq!(
                Value::Object(ran.expect("the item finished").0),
                json!({"changed": false})
            );
        }
    }
    {
        use crate::action_plugins::Kind;
        let undetected = json!([
            "Could not detect which major revision of dnf is in use, which is required to determine module backend.",
            "You should manually specify use_backend to tell the module whether to use the dnf4 or dnf5 backend})",
        ]);
        let mut stop = watch::channel(false).1;
        // A `use_backend` naming no backend asks the host too, as the reference does: its test
        // of `VALID_BACKENDS` that runs the `setup` is not under the `auto`/`yum` one.
        for args in [
            json!({"name": "bash"}),
            json!({"name": "bash", "use_backend": "apt"}),
        ] {
            let mut agent = FakeAgent::answering(
                [
                    vec![state("ab", true)],
                    one_result(
                        1,
                        json!({"ansible_facts": {"ansible_pkg_mgr": "apt"}, "changed": false}),
                    )
                    .to_vec(),
                ]
                .concat(),
            );
            let (ran, _) = plugin_item(
                Kind::Dnf,
                args.clone(),
                json!({}),
                &mut agent,
                &python3(),
                &mut stop,
            )
            .await;
            let result = ran.expect("the item finished");
            assert!(result.failed(), "{result:?}");
            assert_eq!(result.0["msg"], undetected);
            let modules: Vec<&str> = batches_sent(&agent)
                .iter()
                .map(|b| b[0].module.as_str())
                .collect();
            assert_eq!(modules, ["setup"], "{args}: {:?}", agent.sent);
            // The reference puts the answer in the result before it fails, so it is
            // registered...
            assert_eq!(result.0["ansible_facts"], json!({"pkg_mgr": "apt"}));
            // ...and not kept as a fact, because the task failed.
            let mut store = one_host_store();
            record_facts(&mut store, &["h1".to_string()], &[(None, result)]);
            let host = store.for_host("h1", &crate::vars::Scope::default());
            assert!(
                host.get("ansible_facts")
                    .and_then(|f| f.get("pkg_mgr"))
                    .is_none(),
                "{host:?}"
            );
        }

        let mut agent = FakeAgent::answering(Vec::new());
        let (ran, _) = plugin_item(
            Kind::Dnf,
            json!({"name": "bash", "use": "dnf", "use_backend": "dnf5"}),
            json!({}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        let result = ran.expect("the item finished");
        assert!(result.failed(), "{result:?}");
        assert_eq!(
            result.0["msg"],
            json!("parameters are mutually exclusive: ('use', 'use_backend')")
        );
        assert!(agent.sent.is_empty(), "nothing is sent: {:?}", agent.sent);

        // A `setup` that failed fails the task with its cause under the reference's sentence,
        // read off `plugins/action/dnf.py`, and without the facts it carried.
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(
                    1,
                    json!({"failed": true, "msg": "boom", "ansible_facts": {"ansible_pkg_mgr": "dnf"}}),
                )
                .to_vec(),
            ]
            .concat(),
        );
        let (ran, _) = plugin_item(
            Kind::Dnf,
            json!({"name": "bash"}),
            json!({}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        let result = ran.expect("the item finished");
        assert!(result.failed(), "{result:?}");
        assert_eq!(
            result.0["msg"],
            json!("Failed to fetch ansible_pkg_mgr to determine the dnf action backend: boom")
        );
        assert!(!result.0.contains_key("ansible_facts"), "{result:?}");
        assert_eq!(batches_sent(&agent).len(), 1, "{:?}", agent.sent);
    }
}

/// A service task, a lost link, a failed module and the files a sub-task stages.
#[tokio::test]
#[expect(clippy::too_many_lines)]
async fn a_plugin_item_ends_the_way_its_module_and_link_do() {
    {
        use crate::action_plugins::Kind;
        let mut stop = watch::channel(false).1;
        // Measured without facts and with `sleep:` given: `AnsiballZ_setup.py`, then `Running
        // ansible.legacy.systemd` and the warning.
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(
                    1,
                    json!({"ansible_facts": {"ansible_service_mgr": "systemd"}, "changed": false}),
                )
                .to_vec(),
                one_result(2, json!({"changed": false})).to_vec(),
            ]
            .concat(),
        );
        let (ran, warnings) = plugin_item(
            Kind::Service,
            json!({"name": "cron", "state": "started", "sleep": 2, "use": "auto"}),
            json!({}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        assert!(!ran.expect("the item finished").failed());
        let sent = batches_sent(&agent);
        let modules: Vec<&str> = sent.iter().map(|b| b[0].module.as_str()).collect();
        assert_eq!(modules, ["setup", "systemd"], "{:?}", agent.sent);
        assert_eq!(
            Value::Object(sent[0][0].args.clone()),
            json!({"filter": ["ansible_service_mgr"], "gather_subset": ["!all"]})
        );
        assert_eq!(
            Value::Object(sent[1][0].args.clone()),
            json!({"name": "cron", "state": "started"})
        );
        assert_eq!(
            warnings,
            ["Ignoring \"sleep\" as it is not used in \"systemd\""]
        );

        // An unknown name runs `service`, as measured; a `use:` in capitals is lowered, as the
        // reference's plugin does. No `setup` either way.
        for (asked, module) in [("nosuchmgr", "service"), ("SystemD", "systemd")] {
            let mut agent = FakeAgent::answering(
                [
                    vec![state("ab", true)],
                    one_result(1, json!({"changed": false})).to_vec(),
                ]
                .concat(),
            );
            let (ran, _) = plugin_item(
                Kind::Service,
                json!({"name": "cron", "use": asked}),
                json!({}),
                &mut agent,
                &python3(),
                &mut stop,
            )
            .await;
            assert!(!ran.expect("the item finished").failed());
            let sent = batches_sent(&agent);
            assert_eq!(sent.len(), 1, "{:?}", agent.sent);
            assert_eq!(sent[0][0].module, module);
            assert_eq!(
                Value::Object(sent[0][0].args.clone()),
                json!({"name": "cron"})
            );
        }
    }
    {
        use crate::action_plugins::Kind;
        let setup = || {
            one_result(
                1,
                json!({"ansible_facts": {"ansible_pkg_mgr": "apt"}, "changed": false}),
            )
            .to_vec()
        };
        let mut stop = watch::channel(false).1;

        // The agent goes away once the `setup` is in.
        let mut agent = FakeAgent::answering([vec![state("ab", true)], setup()].concat());
        let (ran, _) = plugin_item(
            Kind::Package,
            json!({"name": "bash"}),
            json!({}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        assert!(matches!(ran, Err(Err(_))), "{ran:?}");
        assert_eq!(batches_sent(&agent).len(), 2, "{:?}", agent.sent);

        // Interrupted while the `setup` runs: the agent is asked to stop it, and nothing follows.
        let (stop_tx, mut stopping) = watch::channel(false);
        let mut agent = FakeAgent::answering(vec![state("ab", true)]);
        agent.hangs_when_empty = true;
        stop_tx.send(true).expect("the receiver is alive");
        let (ran, _) = plugin_item(
            Kind::Package,
            json!({"name": "bash"}),
            json!({}),
            &mut agent,
            &python3(),
            &mut stopping,
        )
        .await;
        assert!(
            matches!(ran, Err(Ok(BatchOutcome::Cancelled { .. }))),
            "{ran:?}"
        );
        let modules: Vec<&str> = batches_sent(&agent)
            .iter()
            .map(|b| b[0].module.as_str())
            .collect();
        assert_eq!(modules, ["setup"]);
        assert!(
            agent
                .sent
                .iter()
                .any(|m| matches!(m, ToAgent::Cancel { .. })),
            "{:?}",
            agent.sent
        );

        // No interpreter: the item fails, per task and rescuable, and nothing is sent.
        let mut agent = FakeAgent::answering(Vec::new());
        let (ran, _) = plugin_item(
            Kind::Package,
            json!({"name": "bash"}),
            json!({"ansible_facts": {"pkg_mgr": "apt"}}),
            &mut agent,
            &[],
            &mut stop,
        )
        .await;
        let result = ran.expect("a task failure, not the host's end");
        assert!(msg(&result).contains("no python interpreter"), "{result:?}");
        assert!(agent.sent.is_empty(), "{:?}", agent.sent);

        // The batch ended cleanly with no result in it.
        let mut agent = FakeAgent::answering(vec![
            state("ab", true),
            FromAgent::BatchDone {
                batch: 1,
                outcome: BatchOutcome::Completed,
            },
        ]);
        let (ran, _) = plugin_item(
            Kind::Package,
            json!({"name": "bash"}),
            json!({}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        let result = ran.expect("the item reports");
        assert!(result.failed(), "{result:?}");
        assert!(msg(&result).contains("'setup'"), "{result:?}");
    }
    {
        use crate::action_plugins::Kind;
        let mut stop = watch::channel(false).1;
        let mut agent = FakeAgent::answering(vec![
            state("ab", true),
            FromAgent::TaskResult {
                batch: 1,
                index: 0,
                result: TaskResult(vars(json!({"failed": true, "msg": "No package matching"}))),
                ran: None,
            },
            FromAgent::BatchDone {
                batch: 1,
                outcome: BatchOutcome::Failed { at: 0 },
            },
        ]);
        let (ran, _) = plugin_item(
            Kind::Package,
            json!({"name": "nosuchpackage"}),
            json!({"ansible_facts": {"pkg_mgr": "apt"}}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        let result = ran.expect("the item finished");
        assert!(result.failed(), "{result:?}");
        assert_eq!(msg(&result), "No package matching");
    }
    {
        let sub = crate::action_plugins::Sub {
            module: "copy",
            args: Map::new(),
            files: vec![(
                "src".to_string(),
                crate::action_plugins::FileBlob {
                    hash: "cd".into(),
                    b64: "eA==".into(),
                },
            )],
        };
        let union = union_of("ab", &["copy"]);
        let built = sub_task(
            &task("copy"),
            &bare_item(),
            &sub,
            Some(&union),
            &python3(),
            None,
        )
        .expect("a sub-task with files travels");
        assert_eq!(built.module, "copy");
        assert_eq!(
            built.files,
            [StagedFile {
                arg: "src".into(),
                blob: "cd".into(),
            }]
        );
        assert!(!built.force_python);
        // A plugin's sub-task follows the run's switch like a task the playbook wrote.
        let mut off = union.clone();
        off.natives.enabled = false;
        let built = sub_task(
            &task("copy"),
            &bare_item(),
            &sub,
            Some(&off),
            &python3(),
            None,
        )
        .expect("a sub-task with files travels");
        assert!(built.force_python, "{built:?}");
        let failure = sub_task(&task("copy"), &bare_item(), &sub, None, &python3(), None)
            .expect_err("a module the union lacks is never sent as native");
        assert!(msg(&failure).contains("python payload"), "{failure:?}");
        // A bridged plugin that calls `command` sends the agent's own module no name key.
        let sub = crate::action_plugins::Sub {
            module: "command",
            args: json!({"cmd": "true", "_ansible_module_name": "ansible.legacy.command"})
                .as_object()
                .cloned()
                .expect("an object"),
            files: Vec::new(),
        };
        let built = sub_task(&task("copy"), &bare_item(), &sub, None, &python3(), None)
            .expect("a native sub-task builds");
        assert_eq!(built.args.get("cmd"), Some(&json!("true")));
        assert!(
            !built.args.contains_key("_ansible_module_name"),
            "{built:?}"
        );
    }
}

/// The playbook interpreter wins, an explicit one is used as given, and a missing one says so.
#[test]
fn the_playbook_interpreter_wins_and_a_missing_one_says_so() {
    {
        let found = [
            "/usr/bin/python3.12".to_string(),
            "/usr/bin/python3".to_string(),
        ];
        assert_eq!(
            chosen_interpreter(None, &found).expect("the best of the two"),
            "/usr/bin/python3.12"
        );
        assert_eq!(
            chosen_interpreter(Some("/usr/bin/python3"), &found)
                .expect("what the playbook asked for"),
            "/usr/bin/python3"
        );
    }
    {
        assert_eq!(
            chosen_interpreter(
                Some("/opt/python3.12/bin/python"),
                &["/usr/bin/python3".to_string()]
            )
            .expect("the playbook named it, so it travels"),
            "/opt/python3.12/bin/python"
        );
    }
    {
        let failure = chosen_interpreter(None, &[]).expect_err("nothing to run a module under");
        let msg = failure.0.get("msg").and_then(Value::as_str).unwrap_or("");
        assert!(
            msg.contains("the agent on this host reported no python"),
            "{msg}"
        );
        assert!(failure.failed(), "{failure:?}");
    }
}

/// The two halves of a payload meet here or the step does not travel.
#[test]
fn a_step_travels_with_both_halves_of_its_payload_or_not_at_all() {
    let module = module_payload("ab");
    let chosen = Ok("/usr/bin/python3".to_string());
    let (payload, interpreter) = python_for("ping", Some(&module), Some(&chosen))
        .expect("both halves are in hand")
        .expect("a payload to send");
    assert_eq!(payload.blob, "ab");
    assert_eq!(interpreter, "/usr/bin/python3");

    assert!(
        python_for("command", None, None)
            .expect("a native step")
            .is_none(),
        "a step with no payload travels as it always did"
    );

    let refused: Result<String, TaskResult> = Err(TaskResult::failed_with(
        "The module interpreter was not found.",
    ));
    let failure = python_for("ping", Some(&module), Some(&refused))
        .expect_err("the interpreter could not be chosen");
    assert!(failure.failed(), "{failure:?}");

    let bug = python_for("ping", Some(&module), None).expect_err("a payload with no choice made");
    let msg = bug.0.get("msg").and_then(Value::as_str).unwrap_or("");
    assert!(msg.contains("ab"), "{msg}");
}

/// A payload that could not be stored fails every task of the batch and leaves the host
/// where it was.
#[tokio::test]
async fn a_payload_that_could_not_be_stored_fails_the_tasks_and_not_the_host() {
    let (_stop_tx, mut stop) = watch::channel(false);
    let mut stop_broken = false;
    let mut logs = Vec::new();
    let mut agent = FakeAgent::answering(vec![state("ab", false), state("ab", false)]);
    let tasks = vec![
        protocol_task(
            &task("ping"),
            &bare_item(),
            Some((&module_payload("ab"), "/usr/bin/python3")),
        ),
        protocol_task(&task("command"), &bare_item(), None),
    ];
    let (received, ended) = run_agent_batch(
        &mut agent,
        "h1",
        1,
        tasks,
        Some(&union_named("ab")),
        &[],
        &mut stop,
        &mut stop_broken,
        &mut logs,
    )
    .await;
    assert_eq!(received.len(), 2);
    for slot in &received {
        let result = slot.as_ref().expect("every task of the batch reports");
        assert!(result.failed(), "{result:?}");
        let msg = result.0.get("msg").and_then(Value::as_str).unwrap_or("");
        assert!(msg.contains("ab"), "{msg}");
    }
    assert!(
        matches!(ended, Ok(BatchOutcome::Failed { .. })),
        "the host stays reachable: {ended:?}"
    );
    assert!(
        !agent
            .sent
            .iter()
            .any(|msg| matches!(msg, ToAgent::RunBatch { .. })),
        "a batch went out for a payload the host has not got: {:?}",
        agent.sent
    );
}

/// A frame belonging to a batch, read while a blob question is outstanding, stops the exchange
/// rather than being skipped.
#[tokio::test]
async fn a_batch_frame_during_a_blob_question_is_the_stream_out_of_step() {
    let mut logs = Vec::new();
    let mut agent = FakeAgent::answering(vec![
        FromAgent::BatchDone {
            batch: 7,
            outcome: BatchOutcome::Completed,
        },
        state("ab", true),
    ]);
    let err = ensure_blob(&mut agent, "h1", "ab", "UEsD", &mut logs)
        .await
        .expect_err("the streams are out of step");
    assert!(err.contains('7'), "{err}");
}

/// A batch gets its blob first, never goes out unconfirmed, and asks nothing when it carries no payload.
#[tokio::test]
async fn a_batch_goes_out_only_with_the_blob_its_payload_needs() {
    {
        let mut logs = Vec::new();
        let mut agent = FakeAgent::answering(vec![state("ab", false), state("ab", true)]);
        let tasks = vec![
            protocol_task(&task("command"), &bare_item(), None),
            protocol_task(
                &task("ping"),
                &bare_item(),
                Some((&module_payload("ab"), "/usr/bin/python3")),
            ),
        ];
        blob_preflight(
            &mut agent,
            "h1",
            &tasks,
            Some(&union_named("ab")),
            &[],
            &mut logs,
        )
        .await
        .expect("the blob went first");
        assert_eq!(agent.puts(), 1, "{:?}", agent.sent);
    }
    {
        let mut logs = Vec::new();
        let mut agent = FakeAgent::answering(Vec::new());
        let tasks = vec![protocol_task(
            &task("ping"),
            &bare_item(),
            Some((&module_payload("ab"), "/usr/bin/python3")),
        )];
        let err = blob_preflight(&mut agent, "h1", &tasks, None, &[], &mut logs)
            .await
            .expect_err("no link confirmed this payload");
        assert!(err.contains("ab"), "{err}");
        assert!(agent.sent.is_empty(), "{:?}", agent.sent);
    }
    {
        let mut logs = Vec::new();
        let mut agent = FakeAgent::answering(Vec::new());
        let tasks = vec![protocol_task(&task("command"), &bare_item(), None)];
        blob_preflight(
            &mut agent,
            "h1",
            &tasks,
            Some(&union_named("ab")),
            &[],
            &mut logs,
        )
        .await
        .expect("nothing to ask for");
        assert!(agent.sent.is_empty(), "{:?}", agent.sent);
    }
}

/// A task whose payload names a blob this link never confirmed is a controller bug, and it
/// fails naming the hash rather than being sent for the host to fail on.
#[test]
fn a_payload_naming_an_unsent_blob_fails_before_the_batch() {
    let mut memory = BlobMemory::default();
    let tasks = vec![protocol_task(
        &task("ping"),
        &bare_item(),
        Some((&module_payload("ab"), "/usr/bin/python3")),
    )];
    let err = payloads_confirmed(&tasks, &memory).expect_err("nothing was confirmed");
    assert!(err.contains("ab"), "{err}");
    memory.remember("ab", Ok(()));
    payloads_confirmed(&tasks, &memory).expect("the link confirmed it");

    // A staged file counts only when it went up for this batch, and the check has no memory
    // to consult: the agent consumed the one it had for the task before.
    let mut staging = tasks;
    staging[0].files = vec![StagedFile {
        arg: "src".into(),
        blob: "cd".into(),
    }];
    let err = files_staged(&staging, &BTreeSet::new())
        .expect_err("the file was not put on the host for this batch");
    assert!(err.contains("cd") && err.contains("'src'"), "{err}");
    files_staged(&staging, &["cd".to_string()].into()).expect("the file went up for it");
}

/// A batch refused for its payload puts none of its files on the host.
#[tokio::test]
async fn a_batch_refused_for_its_payload_stages_nothing() {
    let blob = hello_blob();
    let mut staging = protocol_task(
        &task("copy"),
        &bare_item(),
        Some((&module_payload("zz"), "/usr/bin/python3")),
    );
    staging.files = vec![StagedFile {
        arg: "src".into(),
        blob: blob.hash.clone(),
    }];
    let mut agent = FakeAgent::answering(vec![state(&blob.hash, true)]);
    let mut stop = watch::channel(false).1;
    let (results, ended) = run_agent_batch(
        &mut agent,
        "h1",
        1,
        vec![staging],
        None,
        &[("src".into(), blob)],
        &mut stop,
        &mut false,
        &mut Vec::new(),
    )
    .await;
    assert!(
        matches!(ended, Ok(BatchOutcome::Failed { .. })),
        "{ended:?}"
    );
    let failed = results[0].as_ref().expect("the task's failure");
    assert!(msg(failed).contains("never confirmed"), "{failed:?}");
    assert!(agent.sent.is_empty(), "{:?}", agent.sent);
}

/// A link lost while staging, and a file the agent cannot store, fail the item and name the file.
#[tokio::test]
async fn a_file_that_cannot_be_staged_fails_its_item_and_names_the_file() {
    {
        let blob = hello_blob();
        let mut agent = FakeAgent::answering(Vec::new());
        let err = stage_files(&mut agent, "h1", &[("src".into(), blob)], &mut Vec::new())
            .await
            .expect_err("the agent stopped");
        assert!(
            err.starts_with("staging the file for 'src': ") && !err.contains("module payload"),
            "{err}"
        );
    }
    {
        use crate::action_plugins::Kind;
        let blob = hello_blob();
        let mut stop = watch::channel(false).1;
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(1, json!({"stat": {"exists": false}})).to_vec(),
                vec![
                    FromAgent::Log {
                        level: volant_protocol::LogLevel::Error,
                        message: "no space left".into(),
                    },
                    state(&blob.hash, false),
                ],
            ]
            .concat(),
        );
        let (ran, _) = plugin_item(
            Kind::Copy,
            copy_args("/tmp/v/full"),
            json!({}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        let result = ran.expect("the item finished");
        assert!(result.failed(), "{result:?}");
        assert!(
            msg(&result).starts_with("the agent could not store the file for 'src'"),
            "{result:?}"
        );
        assert!(
            !result.0.contains_key("diff") && !result.0.contains_key("checksum"),
            "{result:?}"
        );
        assert_eq!(
            batches_sent(&agent).len(),
            1,
            "only the stat ran: {:?}",
            agent.sent
        );
    }
    {
        use crate::action_plugins::Kind;
        let blob = hello_blob();
        let mut stop = watch::channel(false).1;
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(1, json!({"stat": {"exists": false}})).to_vec(),
                vec![
                    FromAgent::Log {
                        level: volant_protocol::LogLevel::Error,
                        message: format!(
                            "storing payload {}: No space left on device (os error 28)",
                            blob.hash
                        ),
                    },
                    state(&blob.hash, false),
                ],
            ]
            .concat(),
        );
        let (ran, _) = plugin_item(
            Kind::Copy,
            copy_args("/tmp/v/full"),
            json!({}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        let result = ran.expect("a task failure, not the host's end");
        assert!(result.failed(), "{result:?}");
        assert!(
            msg(&result).contains("'src'")
                && msg(&result).contains("No space left on device (os error 28)"),
            "{result:?}"
        );
        let modules: Vec<&str> = batches_sent(&agent)
            .iter()
            .map(|b| b[0].module.as_str())
            .collect();
        assert_eq!(modules, ["stat"], "no copy without its file");
    }
}

/// A batch with no task in it is not sent: there is nothing for the agent to answer, and the
/// batch ends the way an agent ends an empty one.
#[tokio::test]
async fn an_empty_batch_is_not_sent() {
    let mut agent = FakeAgent::answering(Vec::new());
    let mut stop = watch::channel(false).1;
    let (results, ended) = run_agent_batch(
        &mut agent,
        "h1",
        1,
        Vec::new(),
        None,
        &[],
        &mut stop,
        &mut false,
        &mut Vec::new(),
    )
    .await;
    assert!(results.is_empty());
    assert!(matches!(ended, Ok(BatchOutcome::Completed)), "{ended:?}");
    assert!(agent.sent.is_empty(), "{:?}", agent.sent);
}

/// The file blob of `hello\n`, as the plugin stages it.
fn hello_blob() -> crate::action_plugins::FileBlob {
    crate::action_plugins::files::blob_of("hello.txt", b"hello\n").expect("it fits")
}

/// A `hello\n` on the controller, copied to `dest`: the arguments `prepare` hands the plugin.
fn copy_args(dest: &str) -> Value {
    // The caller reads the file after this returns, so the directory is kept.
    let dir = scratch().keep();
    let src = dir.join("hello.txt");
    std::fs::write(&src, "hello\n").expect("the source");
    json!({"src": src.display().to_string(), "dest": dest})
}

/// Two items copying the same bytes each stage them on the host, without asking first.
#[tokio::test]
async fn the_same_content_is_staged_for_every_item() {
    use crate::action_plugins::Kind;
    let blob = hello_blob();
    let mut stop = watch::channel(false).1;
    let mut agent = FakeAgent::answering(
        [
            vec![state("ab", true)],
            one_result(1, json!({"stat": {"exists": false}})).to_vec(),
            vec![state(&blob.hash, true)],
            one_result(2, json!({"changed": true})).to_vec(),
            one_result(1, json!({"stat": {"exists": false}})).to_vec(),
            vec![state(&blob.hash, true)],
            one_result(2, json!({"changed": true})).to_vec(),
        ]
        .concat(),
    );
    for dest in ["/tmp/v/one", "/tmp/v/two"] {
        let (ran, _) = plugin_item(
            Kind::Copy,
            copy_args(dest),
            json!({}),
            &mut agent,
            &python3(),
            &mut stop,
        )
        .await;
        let result = ran.expect("the item finished");
        assert!(!result.failed(), "{dest}: {result:?}");
    }
    let staged = agent
        .sent
        .iter()
        .filter(|m| matches!(m, ToAgent::PutBlob { hash, staged: true, .. } if *hash == blob.hash))
        .count();
    assert_eq!(staged, 2, "{:?}", agent.sent);
    assert!(
        !agent
            .sent
            .iter()
            .any(|m| matches!(m, ToAgent::HasBlob { hash } if *hash == blob.hash)),
        "a file is never asked for: {:?}",
        agent.sent
    );
    let copies: Vec<&Task> = batches_sent(&agent)
        .into_iter()
        .flatten()
        .filter(|t| t.module == "copy")
        .collect();
    assert_eq!(copies.len(), 2, "{:?}", agent.sent);
    for copy in copies {
        assert_eq!(
            copy.files,
            [StagedFile {
                arg: "src".into(),
                blob: blob.hash.clone(),
            }]
        );
    }
}

/// One item of a `copy` task `t` run the way the driver runs it under `t`'s own `retries`,
/// `until` and `delay`: the item's result - `None` when the stop ended it between two
/// attempts - and the count each retry line would show.
async fn copy_attempts<C: AgentChannel>(
    t: &PlayTask,
    args: Value,
    agent: &mut C,
    stop: &mut watch::Receiver<bool>,
) -> (
    Result<Option<TaskResult>, Result<BatchOutcome, String>>,
    Vec<u32>,
) {
    let kind = crate::action_plugins::Kind::Copy;
    let item = Item {
        args: vars(args),
        ..bare_item()
    };
    let templar = Templar::new(PathBuf::from("."));
    let retry = retry_plan(t, Some(&item), &templar).expect("the retry plan renders");
    let origin = crate::compile::Origin::default();
    let running = HostVars::default();
    let union = union_of("ab", crate::action_plugins::modules_for(kind));
    let start = PluginStart {
        kind,
        running_vars: &running,
        delegated: false,
        escalated: false,
        local: false,
        templar: &templar,
        origin: &origin,
        playbook_dir: Path::new("."),
    };
    let mut lefts = Vec::new();
    let ran = run_plugin_attempts(
        agent,
        &mut NoRelink,
        "h1",
        &mut 0,
        &start,
        t,
        &item,
        retry.as_ref(),
        Some(&union),
        &python3(),
        None,
        stop,
        &mut false,
        &mut Vec::new(),
        &mut Vec::new(),
        &mut lefts,
    )
    .await;
    (ran, lefts.into_iter().map(|(left, _)| left).collect())
}

fn retried_copy(retries: u64) -> PlayTask {
    let mut t = task("copy");
    t.retries = Some(json!(retries));
    t.delay = Some(json!(0));
    t
}

/// A retried copy that succeeds runs once; one that fails replays its sequence and stages its file again.
#[tokio::test]
async fn a_retried_copy_runs_once_or_replays_its_whole_sequence() {
    {
        let mut stop = watch::channel(false).1;
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(1, json!({"changed": true})).to_vec(),
            ]
            .concat(),
        );
        let (ran, lefts) = copy_attempts(
            &retried_copy(3),
            json!({"src": "/etc/rancher/k3s/k3s.yaml", "dest": "/etc/rancher/k3s/k3s-copy.yaml", "mode": "0600", "remote_src": true}),
            &mut agent,
            &mut stop,
        )
        .await;
        let result = ran.expect("the item ran").expect("nothing stopped it");
        assert!(!result.failed(), "{result:?}");
        assert_eq!(result.0["attempts"], json!(1));
        assert_eq!(lefts, Vec::<u32>::new());
        assert_eq!(modules_sent(&agent), ["copy"]);
    }
    {
        let blob = hello_blob();
        let mut stop = watch::channel(false).1;
        let missing = json!({"failed": true, "msg": "Destination directory /tmp/volant-nosuch does not exist"});
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(1, json!({"stat": {"exists": false}})).to_vec(),
                vec![state(&blob.hash, true)],
                one_result(2, missing.clone()).to_vec(),
                one_result(3, json!({"stat": {"exists": false}})).to_vec(),
                vec![state(&blob.hash, true)],
                one_result(4, missing.clone()).to_vec(),
                one_result(5, json!({"stat": {"exists": false}})).to_vec(),
                vec![state(&blob.hash, true)],
                one_result(6, missing).to_vec(),
            ]
            .concat(),
        );
        let (ran, lefts) = copy_attempts(
            &retried_copy(2),
            copy_args("/tmp/volant-nosuch/hello.txt"),
            &mut agent,
            &mut stop,
        )
        .await;
        let result = ran.expect("the item ran").expect("nothing stopped it");
        assert!(result.failed(), "{result:?}");
        assert_eq!(result.0["attempts"], json!(2));
        assert_eq!(lefts, [2, 1], "`(2 retries left)` then `(1 retries left)`");
        assert_eq!(
            modules_sent(&agent),
            ["stat", "copy", "stat", "copy", "stat", "copy"]
        );
        let staged = agent
            .sent
            .iter()
            .filter(
                |m| matches!(m, ToAgent::PutBlob { hash, staged: true, .. } if *hash == blob.hash),
            )
            .count();
        assert_eq!(staged, 3, "{:?}", agent.sent);
    }
}

/// With `retries: 2` a failing task runs three times and the failure reports `attempts: 2`, as
/// `task_executor.py` does.
#[test]
fn retries_r_is_r_plus_one_runs_reporting_r_attempts() {
    let mut t = task("command");
    t.retries = Some(json!(2));
    t.delay = Some(json!(0));
    let item = bare_item();
    let templar = Templar::new(PathBuf::from("."));
    let retry = retry_plan(&t, Some(&item), &templar)
        .unwrap()
        .expect("a retry plan");
    let failing = || TaskResult(vars(json!({"failed": true, "rc": 1})));
    let mut lefts = Vec::new();
    let mut attempt = 0;
    let last = loop {
        attempt += 1;
        match judge_attempt(&t, &item, failing(), attempt, &retry, &templar) {
            Attempt::Done(r) => break r,
            Attempt::Again(left, failed) => {
                assert_eq!(failed.0["attempts"], json!(attempt));
                assert_eq!(
                    failed.0["retries"],
                    json!(3),
                    "R + 1, as the reference counts"
                );
                lefts.push(left);
            }
        }
    };
    assert_eq!(attempt, 3, "three runs");
    assert_eq!(lefts, [2, 1]);
    assert_eq!(last.0["attempts"], json!(2));
    assert!(last.failed());

    let passing = TaskResult(vars(json!({"changed": false, "rc": 0})));
    let Attempt::Done(r) = judge_attempt(&t, &item, passing, 1, &retry, &templar) else {
        panic!("a passing run ends the loop");
    };
    assert_eq!(r.0["attempts"], json!(1));
}

/// `break_when` reads the registered result, holds only when every condition does, and an
/// undefined name fails the item and ends the loop.
#[test]
fn break_when_reads_the_result_and_fails_on_an_unevaluable_condition() {
    let mut t = task("command");
    t.register = Some("r".into());
    let item = bare_item();
    let templar = Templar::new(PathBuf::from("."));
    let rc = || TaskResult(vars(json!({"stdout": "2", "changed": false})));
    for (conditions, breaks) in [
        (vec!["r.stdout == '2'"], true),
        (vec!["r.stdout == '2'", "r.stdout == '3'"], false),
        (vec!["r.stdout == '3'"], false),
    ] {
        t.break_when = conditions.iter().map(ToString::to_string).collect();
        let mut result = rc();
        assert_eq!(break_after(&t, &item, &mut result, &templar), breaks);
        assert!(!result.failed(), "{result:?}");
    }
    t.break_when = vec!["nosuch == 1".into()];
    let mut result = rc();
    assert!(break_after(&t, &item, &mut result, &templar));
    assert!(result.failed(), "{result:?}");
    assert!(result.0.contains_key("break_when_result"), "{result:?}");
}

/// `attempts` is on the result before `changed_when` and `failed_when` read it, and still on
/// it when a condition could not be evaluated. ansible-core 2.19.12 `task_executor.py` sets
#[test]
fn changed_when_reads_the_attempt_it_judges() {
    let mut t = task("command");
    t.register = Some("r".into());
    t.retries = Some(json!(3));
    t.delay = Some(json!(0));
    t.until = vec!["r.rc == 0".into()];
    t.changed_when = vec!["r.attempts > 1".into()];
    let item = bare_item();
    let templar = Templar::new(PathBuf::from("."));
    let retry = retry_plan(&t, Some(&item), &templar)
        .unwrap()
        .expect("a retry plan");
    let rc = |rc: i64| TaskResult(vars(json!({"rc": rc, "changed": true})));
    assert!(matches!(
        judge_attempt(&t, &item, rc(1), 1, &retry, &templar),
        Attempt::Again(3, _)
    ));
    let Attempt::Done(r) = judge_attempt(&t, &item, rc(0), 2, &retry, &templar) else {
        panic!("rc 0 ends the loop");
    };
    assert!(!r.failed(), "{r:?}");
    assert_eq!(r.0["changed"], json!(true), "{r:?}");
    assert_eq!(r.0["attempts"], json!(2));

    // An `until` the failure satisfies, so that the loop ends on the result the broken
    // condition handed back, and not on one an `until` reading a missing `rc` would raise.
    t.changed_when = vec!["r.nosuchkey".into()];
    t.until = vec!["r.failed".into()];
    let retry = retry_plan(&t, Some(&item), &templar)
        .unwrap()
        .expect("a retry plan");
    let Attempt::Done(r) = judge_attempt(&t, &item, rc(0), 1, &retry, &templar) else {
        panic!("the failure satisfies `until`");
    };
    assert!(r.failed(), "{r:?}");
    let why = r.0["changed_when_result"].as_str().unwrap_or_default();
    assert!(
        why.starts_with("Error while evaluating conditional: "),
        "{r:?}"
    );
    assert!(why.contains("undefined"), "{r:?}");
    assert_eq!(
        msg(&r),
        "Task failed: Action failed: Unknown error.",
        "{r:?}"
    );
    assert_eq!(r.0["attempts"], json!(1), "{r:?}");
}

/// A run that hit the `timeout` keyword ends the task as it stands: no second run, no
/// `attempts`, and `failed_when` never consulted. In ansible-core 2.19.12 the timeout is an
#[test]
fn a_timed_out_run_is_never_retried_nor_judged() {
    let mut t = task("command");
    t.retries = Some(json!(3));
    t.delay = Some(json!(0));
    t.failed_when = vec!["false".into()];
    let item = bare_item();
    let templar = Templar::new(PathBuf::from("."));
    let retry = retry_plan(&t, Some(&item), &templar)
        .unwrap()
        .expect("a retry plan");
    let Attempt::Done(r) = judge_attempt(&t, &item, TaskResult::timed_out(1), 1, &retry, &templar)
    else {
        panic!("a timeout ends the loop");
    };
    assert_eq!(r, TaskResult::timed_out(1));
    assert_eq!(
        finish(&t, &item, TaskResult::timed_out(1), &templar),
        TaskResult::timed_out(1)
    );
}

/// Enormous, negative and unsleepable delays and deadlines never panic.
#[tokio::test]
async fn a_delay_at_the_edge_of_the_clock_never_panics() {
    {
        let templar = Templar::new(PathBuf::from("."));
        let mut t = task("command");
        t.retries = Some(json!(1));
        t.delay = Some(json!(1e20));
        let retry = retry_plan(&t, Some(&bare_item()), &templar)
            .unwrap()
            .expect("a retry plan");
        assert_eq!(retry.delay, Duration::MAX);
        let failed = retry.sleep_failure().expect("the sleep fails");
        assert_eq!(msg(&failed), PYTIME_OUT_OF_RANGE);
        let (tx, mut stop) = watch::channel(false);
        tx.send(true).expect("the run stops");
        assert_eq!(wait_or_stop(retry.delay, &mut stop, &mut false).await, None);
    }
    {
        let blob = hello_blob();
        let mut stop = watch::channel(false).1;
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(1, json!({"stat": {"exists": false}})).to_vec(),
                vec![state(&blob.hash, true)],
                one_result(2, json!({"failed": true, "msg": "no"})).to_vec(),
            ]
            .concat(),
        );
        let mut t = retried_copy(1);
        t.delay = Some(json!(1e20));
        let (ran, lefts) = tokio::time::timeout(
            Duration::from_secs(10),
            copy_attempts(&t, copy_args("/tmp/v/sleepless"), &mut agent, &mut stop),
        )
        .await
        .expect("the item ended rather than waited");
        let result = ran.expect("the item ran").expect("nothing stopped it");
        assert_eq!(msg(&result), PYTIME_OUT_OF_RANGE);
        assert_eq!(result.0["changed"], json!(false));
        assert_eq!(lefts, [1]);
    }
    {
        let mut stop = watch::channel(false).1;
        let mut t = task("copy");
        t.timeout = Some(u64::MAX);
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(1, json!({"stat": {"exists": false}})).to_vec(),
            ]
            .concat(),
        );
        let _ = copy_attempts(&t, copy_args("/tmp/v/far"), &mut agent, &mut stop).await;
        assert_eq!(modules_sent(&agent)[0], "stat");
    }
    {
        let templar = Templar::new(PathBuf::from("."));
        let mut t = task("command");
        t.retries = Some(json!(1));
        for (delay, want) in [(-3, 1), (0, 0), (2, 2)] {
            t.delay = Some(json!(delay));
            let retry = retry_plan(&t, Some(&bare_item()), &templar)
                .unwrap()
                .expect("a retry plan");
            assert_eq!(retry.delay, Duration::from_secs(want), "delay: {delay}");
        }
    }
}

/// A plugin's retries follow the same rules, its transfer runs on the item's time, until reads its last result.
#[tokio::test]
#[expect(clippy::too_many_lines)]
async fn a_plugin_s_retries_run_on_the_item_s_time_and_until_reads_the_last_result() {
    {
        let blob = hello_blob();
        let mut stop = watch::channel(false).1;
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(1, json!({"stat": {"exists": false}})).to_vec(),
                vec![state(&blob.hash, true)],
                one_result(2, json!({"failed": true, "msg": "no"})).to_vec(),
                one_result(3, json!({"stat": {"exists": false}})).to_vec(),
                vec![state(&blob.hash, true)],
                one_result(4, json!({"changed": false})).to_vec(),
            ]
            .concat(),
        );
        let mut t = retried_copy(3);
        t.register = Some("r".into());
        t.changed_when = vec!["r.attempts > 1".into()];
        let (ran, _) = copy_attempts(&t, copy_args("/tmp/v/attempts"), &mut agent, &mut stop).await;
        let result = ran.expect("the item ran").expect("nothing stopped it");
        assert!(!result.failed(), "{result:?}");
        assert_eq!(result.0["changed"], json!(true), "{result:?}");
        assert_eq!(result.0["attempts"], json!(2));

        let timed_out = Value::Object(TaskResult::timed_out(1).0);
        let mut agent = FakeAgent::answering(
            [vec![state("ab", true)], one_result(1, timed_out).to_vec()].concat(),
        );
        let (ran, lefts) = copy_attempts(
            &retried_copy(3),
            copy_args("/tmp/v/slow"),
            &mut agent,
            &mut stop,
        )
        .await;
        let result = ran.expect("the item ran").expect("nothing stopped it");
        assert!(result.failed(), "{result:?}");
        assert_eq!(msg(&result), "Task failed: Timed out after 1 second(s).");
        assert_eq!(result.0["timedout"]["period"], json!(1));
        assert!(!result.0.contains_key("attempts"), "{result:?}");
        assert_eq!(lefts, Vec::<u32>::new());
        assert_eq!(modules_sent(&agent), ["stat"]);
    }
    {
        let blob = hello_blob();
        let mut stop = watch::channel(false).1;
        let answers = || {
            [
                vec![state("ab", true)],
                one_result(1, json!({"stat": {"exists": false}})).to_vec(),
                vec![state(&blob.hash, true)],
            ]
            .concat()
        };
        let mut t = task("copy");
        t.timeout = Some(1);
        let mut agent = FakeAgent::answering(
            [
                answers(),
                one_result(2, json!({"stat": {"exists": true}})).to_vec(),
            ]
            .concat(),
        );
        agent.transfer_takes = Duration::from_millis(1100);
        let (ran, _) = copy_attempts(&t, copy_args("/tmp/v/late"), &mut agent, &mut stop).await;
        let result = ran.expect("the item ran").expect("nothing stopped it");
        assert_eq!(msg(&result), "Task failed: Timed out after 1 second(s).");
        // The second `stat` takes the staged file, which the agent removes after it.
        assert_eq!(modules_sent(&agent), ["stat", "stat"]);
        let swept = &batches_sent(&agent)[1][0];
        assert_eq!(
            swept.files,
            [StagedFile {
                arg: "path".into(),
                blob: blob.hash.clone()
            }]
        );

        t.timeout = Some(3);
        let mut agent = FakeAgent::answering(
            [
                answers(),
                one_result(2, json!({"changed": true, "dest": "/tmp/v/late"})).to_vec(),
            ]
            .concat(),
        );
        agent.transfer_takes = Duration::from_millis(1100);
        let _ = copy_attempts(&t, copy_args("/tmp/v/late"), &mut agent, &mut stop).await;
        assert_eq!(modules_sent(&agent), ["stat", "copy"]);
        let given = agent.sent.iter().find_map(|m| match m {
            ToAgent::RunBatch { tasks, .. } if tasks[0].module == "copy" => tasks[0].timeout,
            _ => None,
        });
        assert!(given.is_some_and(|t| t < 3), "{given:?}");
    }
    {
        let blob = hello_blob();
        let mut stop = watch::channel(false).1;
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(1, json!({"changed": true, "stat": {"exists": false}})).to_vec(),
                vec![state(&blob.hash, true)],
                one_result(2, json!({"changed": false})).to_vec(),
                one_result(3, json!({"changed": true, "stat": {"exists": false}})).to_vec(),
                vec![state(&blob.hash, true)],
                one_result(4, json!({"changed": true})).to_vec(),
            ]
            .concat(),
        );
        let mut t = retried_copy(3);
        t.register = Some("r".into());
        t.until = vec!["r.changed".into()];
        let (ran, lefts) =
            copy_attempts(&t, copy_args("/tmp/v/until"), &mut agent, &mut stop).await;
        let result = ran.expect("the item ran").expect("nothing stopped it");
        assert!(!result.failed(), "{result:?}");
        assert_eq!(result.0["attempts"], json!(2));
        assert_eq!(lefts, [3]);
    }
}

/// A stop raised while one attempt ends is honoured before the next begins, even with no delay
/// to wait through: the item ends there with no result, and nothing more is sent.
#[tokio::test]
async fn a_stop_between_two_attempts_ends_the_item_at_once() {
    let (tx, mut stop) = watch::channel(false);
    let mut agent = StopsAfter {
        agent: FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(1, json!({"failed": true, "msg": "no"})).to_vec(),
            ]
            .concat(),
        ),
        stop: tx,
        after: 1,
    };
    let (ran, lefts) = copy_attempts(
        &retried_copy(2),
        json!({"src": "/a", "dest": "/b", "remote_src": true}),
        &mut agent,
        &mut stop,
    )
    .await;
    assert!(matches!(ran, Ok(None)), "{ran:?}");
    assert_eq!(lefts, [2]);
    assert_eq!(modules_sent(&agent.agent), ["copy"]);
}

/// A python task carries its payload to the agent, one payload for every interpreter.
#[test]
fn a_python_task_carries_one_payload_for_every_interpreter() {
    {
        let item = bare_item();
        let module = module_payload("ab");
        let built = protocol_task(&task("ping"), &item, Some((&module, "/usr/bin/python3")));
        assert_eq!(built.payload, Some(payload("ab", "/usr/bin/python3")));
        assert_eq!(protocol_task(&task("command"), &item, None).payload, None);
    }
    {
        let item = bare_item();
        let module = module_payload("ab");
        let here = protocol_task(&task("ping"), &item, Some((&module, "/usr/bin/python3")));
        let there = protocol_task(&task("ping"), &item, Some((&module, "/usr/bin/python3.12")));
        assert_eq!(
            here.payload.as_ref().map(|p| p.interpreter.as_str()),
            Some("/usr/bin/python3")
        );
        assert_eq!(
            there.payload.as_ref().map(|p| p.interpreter.as_str()),
            Some("/usr/bin/python3.12")
        );
        assert_eq!(
            here.payload.map(|p| p.module_fqn),
            there.payload.map(|p| p.module_fqn)
        );
    }
}

fn payload(blob: &str, interpreter: &str) -> volant_protocol::PythonPayload {
    volant_protocol::PythonPayload {
        blob: blob.to_string(),
        module_fqn: "ansible.modules.ping".into(),
        profile: "legacy".into(),
        rlimit_nofile: 0,
        extensions: Map::new(),
        interpreter: interpreter.into(),
    }
}

fn one_host_store() -> VarStore {
    let inventory = crate::inventory::Inventory::parse_ini("h1\n").expect("an inventory");
    VarStore::new(&inventory, None, Path::new("."), Map::new()).expect("a var store")
}

fn registered(store: &mut VarStore, value: Value) {
    let mut t = task("ping");
    t.register = Some("probe".into());
    let results = vec![(None, TaskResult(vars(value)))];
    record_registered(store, &t, &["h1".to_string()], &results, &[]);
}

/// A registered result carries no `invocation`, as the reference registers it, while `-vvv`
/// still shows it on the task's line.
#[test]
fn a_registered_result_drops_its_invocation_and_the_line_keeps_it() {
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let result = TaskResult(vars(json!({
        "changed": false,
        "invocation": {"module_args": {"path": "/etc/hostname"}},
        "stat": {"exists": true},
    })));
    let mut store = one_host_store();
    let mut t = task("stat");
    t.register = Some("s".into());
    record_registered(
        &mut store,
        &t,
        &["h1".to_string()],
        &[(None, result.clone())],
        &[],
    );
    let seen = store.for_host("h1", &crate::vars::Scope::default());
    assert_eq!(
        seen["s"],
        json!({"changed": false, "stat": {"exists": true}})
    );
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut out = crate::render::Renderer::with_writer(Box::new(Sink(buf.clone())), false, 79, 3);
    out.result(
        "h1",
        Outcome::Ok,
        &result,
        None,
        crate::render::Dump::No,
        false,
        None,
    );
    let line = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(
        line.contains("\"invocation\": {") && line.contains("\"module_args\": {"),
        "{line}"
    );

    // A loop's items are registered whole.
    t.loop_items = Some(json!(["a"]));
    record_registered(
        &mut store,
        &t,
        &["h1".to_string()],
        &[(Some(json!("a")), result)],
        &[],
    );
    let seen = store.for_host("h1", &crate::vars::Scope::default());
    assert!(
        seen["s"]["results"][0].get("invocation").is_some(),
        "{}",
        seen["s"]
    );
}

/// A module's warnings are shown and stay on the registered result, a loop's gathered on the
/// aggregate in item order; `pause` registers none. Measured on ansible-core 2.19.12 with a
#[test]
fn a_module_s_warnings_stay_on_its_registered_result() {
    let warned = || {
        TaskResult(vars(
            json!({"changed": false, "msg": "done", "warnings": ["probe warning"]}),
        ))
    };
    let mut store = one_host_store();
    let mut t = task("warn_probe");
    t.register = Some("w".into());
    let seen =
        |store: &mut VarStore| store.for_host("h1", &crate::vars::Scope::default())["w"].clone();
    let mut results = vec![(None, warned())];
    let warnings = take_warnings(&mut results);
    record_registered(&mut store, &t, &["h1".to_string()], &results, &warnings);
    assert_eq!(
        seen(&mut store),
        json!({"changed": false, "msg": "done", "warnings": ["probe warning"]})
    );
    assert!(
        results[0].1.0.get("warnings").is_none(),
        "shown once, not dumped"
    );

    t.loop_items = Some(json!([1, 2]));
    let mut results = vec![(Some(json!(1)), warned()), (Some(json!(2)), warned())];
    let warnings = take_warnings(&mut results);
    record_registered(&mut store, &t, &["h1".to_string()], &results, &warnings);
    let w = seen(&mut store);
    assert_eq!(
        w["warnings"],
        json!(["probe warning", "probe warning"]),
        "{w}"
    );
    assert!(w["results"][0].get("warnings").is_none(), "{w}");

    let mut t = task("pause");
    t.register = Some("w".into());
    record_registered(
        &mut store,
        &t,
        &["h1".to_string()],
        &[(None, TaskResult(vars(json!({"changed": false}))))],
        &["Not waiting for response to prompt as stdin is not interactive".to_string()],
    );
    let w = seen(&mut store);
    assert!(w.get("warnings").is_none(), "{w}");
}

/// Measured on ansible-core 2.19.12: `module.warn("probe warning")` arrives as
#[test]
fn a_warning_summary_is_its_brief_message() {
    let summary = json!({
        "event": {"msg": "probe warning", "__ansible_type": "Event"},
        "__ansible_type": "WarningSummary",
    });
    assert_eq!(warning_text(&summary), "probe warning");
    assert_eq!(warning_text(&json!("plain")), "plain");
    let chained = |follow: bool, cause: &str| {
        json!({"event": {"msg": "outer.", "chain": {
            "msg_reason": "because", "traceback_reason": "tb", "follow": follow,
            "event": {"msg": cause},
        }}})
    };
    assert_eq!(warning_text(&chained(true, "inner")), "outer: inner");
    assert_eq!(warning_text(&chained(false, "inner")), "outer.");
    let repeated = json!({"event": {"msg": "outer: inner", "chain": {
        "follow": true, "event": {"msg": "inner"},
    }}});
    assert_eq!(warning_text(&repeated), "outer: inner");
}

/// A python module result enters untrusted, and a template it returns is never rendered.
#[test]
fn a_python_module_result_is_untrusted_and_never_rendered() {
    {
        let mut store = one_host_store();
        registered(&mut store, json!({"changed": false}));
        assert!(
            store
                .untrusted_of("h1", &crate::vars::Scope::default())
                .contains("probe")
        );
    }
    #[cfg(unix)]
    {
        let dir_guard = scratch();
        let dir = dir_guard.path().to_path_buf();
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let templar = Templar::new(PathBuf::from("."));
        let render = |store: &mut VarStore| {
            let vars = HostVars {
                map: store.for_host("h1", &crate::vars::Scope::default()),
                untrusted: store.untrusted_of("h1", &crate::vars::Scope::default()),
                untrusted_hosts: store.untrusted_hosts(),
                ..HostVars::default()
            };
            templar.render("{{ probe.msg }}", crate::template::Vars::from(&vars))
        };

        let marker = dir.join("untrusted-marker");
        let _ = std::fs::remove_file(&marker);
        let mut store = one_host_store();
        let text = format!("{{{{ lookup('pipe', 'touch {}') }}}}", marker.display());
        registered(&mut store, json!({ "msg": text }));
        // Asserted, not discarded: a `record_registered` that wrote nothing would leave
        // `probe.msg` undefined, the render would fail, and the marker would be absent for a
        // reason that has nothing to do with the taint. The value has to arrive, and arrive
        // verbatim.
        assert_eq!(
            render(&mut store).expect("the value renders as the text it is"),
            text
        );
        assert!(!marker.exists(), "a module's own output was rendered");

        let author = dir.join("author-marker");
        let _ = std::fs::remove_file(&author);
        let mut store = one_host_store();
        store.set_fact(
            "h1",
            "probe",
            json!({"msg": format!("{{{{ lookup('pipe', 'touch {}') }}}}", author.display())}),
        );
        render(&mut store).expect("an author-side template renders");
        assert!(author.exists(), "the lookup itself does nothing here");
    }
}

/// Gathered facts are untrusted, and they land under `ansible_facts` as well as flat.
#[test]
fn gathered_facts_are_untrusted_and_land_under_both_names() {
    let templar = Templar::new(PathBuf::from("."));
    let mut store = one_host_store();
    let render = |store: &mut VarStore, text: &str| {
        let vars = HostVars {
            map: store.for_host("h1", &crate::vars::Scope::default()),
            untrusted: store.untrusted_of("h1", &crate::vars::Scope::default()),
            untrusted_hosts: store.untrusted_hosts(),
            ..HostVars::default()
        };
        templar
            .render(text, crate::template::Vars::from(&vars))
            .expect("a gathered fact reads")
    };
    // The shape ansible-core 2.19.12's `setup` returns, measured with `gather_subset: [min]`:
    // every fact already carries the `ansible_` prefix except `gather_subset` and
    // `module_setup`, and `ansible_local` is always there.
    let result = TaskResult(vars(json!({
        "ansible_facts": {
            "ansible_hostname": "probe-hostname",
            "ansible_distribution": "Ubuntu",
            "ansible_local": {},
            "gather_subset": ["min"],
            "module_setup": true,
        },
        "changed": false,
    })));
    record_facts(&mut store, &["h1".to_string()], &[(None, result)]);
    // Read through a real render rather than off the store, because reading is what a
    // playbook does with a fact: a name written somewhere `for_host` does not merge is a
    // fact nothing can use.
    assert_eq!(
        render(&mut store, "{{ ansible_hostname }}"),
        "probe-hostname"
    );
    assert_eq!(
        render(&mut store, "{{ ansible_facts['distribution'] }}"),
        "Ubuntu"
    );
    // The reference, same play: `ansible_facts['ansible_distribution']` is undefined, and
    // `ansible_local` is the one prefix `namespace_facts()` keeps.
    assert_eq!(
        render(
            &mut store,
            "{{ ansible_facts['ansible_distribution'] is defined }} \
                 {{ ansible_facts.ansible_local is defined }} {{ ansible_facts.local is defined }} \
                 {{ ansible_facts.module_setup }} {{ ansible_ansible_hostname is defined }}"
        ),
        "False True False True False"
    );
    // Flat, a key lands as the module spelled it: the reference's `clean_facts()` adds no
    // prefix, so `module_setup` is there and `ansible_module_setup` is undefined.
    assert_eq!(
        render(
            &mut store,
            "{{ module_setup }} {{ gather_subset | first }} {{ ansible_module_setup is defined }}"
        ),
        "True min False"
    );
    for name in ["ansible_hostname", "ansible_facts"] {
        assert!(
            store
                .untrusted_of("h1", &crate::vars::Scope::default())
                .contains(name),
            "{name} carries a managed host's own words"
        );
    }
    store.set_fact("h1", "ansible_hostname", json!("SHADOWED"));
    assert_eq!(
        render(
            &mut store,
            "{{ ansible_hostname }} / {{ ansible_facts.hostname }}"
        ),
        "SHADOWED / probe-hostname"
    );
}

/// A failed task keeps no facts, a fact cannot move the next connection, a template in a fact never renders.
#[test]
#[expect(clippy::too_many_lines)]
fn a_gathered_fact_is_data_and_moves_nothing_it_should_not() {
    {
        let scope = crate::vars::Scope::default();
        let fact = |value: Value| TaskResult(vars(value));
        let mut store = one_host_store();
        record_facts(
            &mut store,
            &["h1".to_string()],
            &[(
                None,
                fact(json!({"failed": true, "msg": "boom", "ansible_facts": {"kept": 1}})),
            )],
        );
        record_facts(
            &mut store,
            &["h1".to_string()],
            &[
                (
                    Some(json!(1)),
                    fact(json!({"changed": false, "ansible_facts": {"looped": 1}})),
                ),
                (Some(json!(2)), fact(json!({"failed": true, "rc": 1}))),
            ],
        );
        let host = store.for_host("h1", &scope);
        assert!(!host.contains_key("kept"), "{host:?}");
        assert!(!host.contains_key("looped"), "{host:?}");
        // A non-zero `rc` that `failed_when: false` forgave: `failed` is what decides.
        record_facts(
            &mut store,
            &["h1".to_string()],
            &[(
                None,
                fact(json!({"rc": 2, "failed": false, "ansible_facts": {"forgiven": 1}})),
            )],
        );
        assert_eq!(store.for_host("h1", &scope)["forgiven"], json!(1));
    }
    {
        let inventory = crate::inventory::Inventory::parse_ini(
            "h1 ansible_host=192.0.2.10 ansible_python_interpreter=/usr/bin/python3\n",
        )
        .expect("an inventory");
        let mut store = VarStore::new(&inventory, None, Path::new("."), Map::new()).unwrap();
        let result = TaskResult(vars(json!({
            "ansible_facts": {
                "ansible_host": "203.0.113.9",
                "ansible_connection": "local",
                "ansible_user": "intruder",
                "ansible_python_interpreter": "/tmp/evil/python",
                "ansible_foo_interpreter": "x",
                "ansible_become_password": "x",
                "ansible_become_anything": "x",
                "ansible_ssh_extra_args": "-oProxyCommand=x",
                "ansible_remote_tmp": "/home/deploy/x",
                "ansible_ssh_host_key_rsa_public": "KEY",
                "ansible_ssh_foo_bridge": "br",
                "ansible_local": {"a": 1},
                "ansible_local_thing": "x",
                "ansible_winrm_x": "x",
                "ansible_paramiko_ssh_x": "x",
                "ansible_psrp_x": "x",
                "ansible_network_os": "x",
                "ansible_rsync_path": "x",
                "ansible_playbook_python": "x",
                "add_host": "x",
                "add_group": "x",
                "_ansible_hidden": "x",
                "nested": {"_ansible_inner": 1, "kept": 2},
                "plain": 1,
            },
            "changed": false,
        })));
        let warned = record_facts(&mut store, &["h1".to_string()], &[(None, result)]);
        let v = store.for_host("h1", &crate::vars::Scope::default());
        let defaults = ConnectionDefaults {
            remote_user: None,
            connection: None,
            private_key: None,
            host_key_checking: true,
            remote_tmp: "~/.ansible/tmp".to_string(),
            connect_timeout: Duration::from_secs(10),
            r#become: false,
            become_user: "root".to_string(),
            become_method: "sudo".to_string(),
            become_exe: None,
            become_flags: None,
            become_password: None,
            ssh_common_args: Vec::new(),
            ssh_extra_args: Vec::new(),
        };
        let Transport::Ssh(target) = Transport::for_vars("h1", &v, &defaults).unwrap() else {
            panic!("a fact turned the host's connection local");
        };
        assert_eq!(target.address, "192.0.2.10");
        assert_eq!(target.user, None);
        assert!(target.extra_args.is_empty(), "{:?}", target.extra_args);
        // Where the escalated agent is looked for: a host that picks it plants its own.
        assert_eq!(target.remote_tmp, defaults.remote_tmp);
        assert_eq!(
            requested_interpreter(&v).as_deref(),
            Some("/usr/bin/python3")
        );
        assert!(!v.contains_key("ansible_become_password"));
        assert_eq!(v["ansible_ssh_host_key_rsa_public"], json!("KEY"));
        assert_eq!(v["ansible_ssh_foo_bridge"], json!("br"));
        assert_eq!(v["ansible_local"], json!({"a": 1}));
        assert_eq!(v["plain"], json!(1));
        assert!(!v.contains_key("_ansible_hidden"));
        assert_eq!(v["nested"], json!({"kept": 2}));
        assert_eq!(
            warned,
            [
                "add_group",
                "add_host",
                "ansible_become_anything",
                "ansible_become_password",
                "ansible_connection",
                "ansible_foo_interpreter",
                "ansible_host",
                "ansible_local_thing",
                "ansible_network_os",
                "ansible_paramiko_ssh_x",
                "ansible_playbook_python",
                "ansible_psrp_x",
                "ansible_python_interpreter",
                "ansible_remote_tmp",
                "ansible_rsync_path",
                "ansible_ssh_extra_args",
                "ansible_user",
                "ansible_winrm_x",
            ]
        );
        // The reference leaves the namespace alone, and nothing reads a connection from it.
        assert_eq!(v["ansible_facts"]["host"], json!("203.0.113.9"));
    }
    #[cfg(unix)]
    {
        let dir_guard = scratch();
        let dir = dir_guard.path().to_path_buf();
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let marker = dir.join("gathered-marker");
        let _ = std::fs::remove_file(&marker);
        let text = format!("{{{{ lookup('pipe', 'touch {}') }}}}", marker.display());
        let mut store = one_host_store();
        let result = TaskResult(vars(json!({ "ansible_facts": { "hostname": text } })));
        record_facts(&mut store, &["h1".to_string()], &[(None, result)]);
        let templar = Templar::new(PathBuf::from("."));
        let vars = HostVars {
            map: store.for_host("h1", &crate::vars::Scope::default()),
            untrusted: store.untrusted_of("h1", &crate::vars::Scope::default()),
            untrusted_hosts: store.untrusted_hosts(),
            ..HostVars::default()
        };
        assert_eq!(
            templar
                .render(
                    "{{ ansible_facts.hostname }}",
                    crate::template::Vars::from(&vars)
                )
                .expect("the value renders as the text it is"),
            text
        );
        assert!(!marker.exists(), "a gathered fact was rendered");
    }
}

/// `AgentLink`'s `stop_batch` forwards to `cancel`, called against a real agent the way the
/// driver calls it.
#[cfg(unix)]
#[tokio::test]
async fn a_real_link_s_stop_batch_forward_reaches_the_agent() {
    let marker = format!("40.{}", std::process::id());
    // The lib test binary lives in target/<profile>/deps/; the workspace's other binaries,
    // volant-agent included, are built one level up, in target/<profile>/ itself.
    let exe = std::env::current_exe().expect("this test's own binary path");
    let agent_dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("deps/ has a parent")
        .to_path_buf();
    let saved = std::env::var("VOLANT_AGENT_DIR").ok();
    // SAFETY: nothing else in this process reads or writes this variable concurrently -
    // `AgentSource::discover` runs synchronously between the two lines that set and restore
    // it, and nextest gives every test its own process.
    unsafe {
        std::env::set_var("VOLANT_AGENT_DIR", &agent_dir);
    }
    // Caught so the restore below runs even when `discover` panics, then re-raised.
    let source = std::panic::catch_unwind(AgentSource::discover);
    match saved {
        Some(v) => unsafe { std::env::set_var("VOLANT_AGENT_DIR", v) },
        None => unsafe { std::env::remove_var("VOLANT_AGENT_DIR") },
    }
    let source = source.unwrap_or_else(|panic| std::panic::resume_unwind(panic));

    let mut vars = BTreeMap::new();
    vars.insert("ansible_connection".to_string(), json!("local"));
    let host = crate::inventory::Host {
        name: "localhost".to_string(),
        vars,
    };
    let defaults = ConnectionDefaults {
        remote_user: None,
        connection: None,
        private_key: None,
        host_key_checking: true,
        remote_tmp: "~/.ansible/tmp".to_string(),
        connect_timeout: Duration::from_secs(10),
        r#become: false,
        become_user: "root".to_string(),
        become_method: "sudo".to_string(),
        become_exe: None,
        become_flags: None,
        become_password: None,
        ssh_common_args: Vec::new(),
        ssh_extra_args: Vec::new(),
    };
    let transport = Transport::for_host(&host, &defaults).expect("a local transport");
    let mut link = transport
        .connect(&source, None)
        .await
        .expect("a real agent");
    link.handshake().await.expect("a real handshake");
    link.send(&ToAgent::RunBatch {
        id: 77,
        tasks: vec![Task {
            module: "shell".into(),
            args: json!({"_raw_params": format!("sleep {marker} & wait")})
                .as_object()
                .unwrap()
                .clone(),
            ignore_errors: false,
            timeout: None,
            environment: BTreeMap::default(),
            payload: None,
            files: Vec::new(),
            force_python: false,
            job: None,
        }],
    })
    .await
    .expect("sending the batch");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The trait method the driver actually calls, not `cancel` directly.
    let confirmed = AgentChannel::stop_batch(&mut link, 77, Duration::from_secs(5)).await;
    // `kill_group` is a fire-and-forget SIGKILL with nothing waiting on the backgrounded
    // grandchild, so a dying `sleep` can still be in /proc for one instant after the
    // confirmation frame - the same slack `cancellation_kills_the_whole_process_group` gives
    // it before its own `pgrep`.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let survivors = std::process::Command::new("pgrep")
        .args(["-f", &marker])
        .output()
        .expect("pgrep");
    // Before the assertion, not after: a failure here must not also leak the agent and its
    // 40-second sleep.
    link.shutdown().await;
    assert!(confirmed, "the agent must confirm the cancel");
    assert!(
        survivors.stdout.is_empty(),
        "the task kept running on a real agent after the real stop_batch forward: {}",
        String::from_utf8_lossy(&survivors.stdout)
    );
}

/// The host a `reboot` starts on: `setup`, the boot id, `find`, and whatever `shutdown`
/// answers, which is nothing at all when `shutdown` is `None` - the link goes down under it.
fn before_the_reboot(shutdown: Option<Vec<FromAgent>>) -> FakeAgent {
    FakeAgent::answering(
        [
            vec![state("ab", true)],
            one_result(
                1,
                json!({"ansible_facts": {"ansible_distribution": "Ubuntu",
                    "ansible_distribution_version": "24.04", "ansible_os_family": "Debian"}}),
            )
            .to_vec(),
            raw_answer(2, 0, "old-boot-id\n").to_vec(),
            one_result(
                3,
                json!({"files": [{"path": "/sbin/shutdown"}], "matched": 1}),
            )
            .to_vec(),
            shutdown.unwrap_or_default(),
        ]
        .concat(),
    )
}

async fn reboot_item(
    args: Value,
    agent: &mut FakeAgent,
    relink: &mut Scripted,
    local: bool,
    stop: &mut watch::Receiver<bool>,
) -> Result<TaskResult, Result<BatchOutcome, String>> {
    reboot_item_under(None, args, agent, relink, local, stop).await
}

/// [`reboot_item`] for a task with the `timeout` keyword.
async fn reboot_item_under(
    timeout: Option<u64>,
    args: Value,
    agent: &mut FakeAgent,
    relink: &mut Scripted,
    local: bool,
    stop: &mut watch::Receiver<bool>,
) -> Result<TaskResult, Result<BatchOutcome, String>> {
    // A broken wait is a wait that never ends: bounded here, so it fails rather than hangs.
    let (ran, _) = tokio::time::timeout(
        Duration::from_secs(30),
        plugin_item_on(
            crate::action_plugins::Kind::Reboot,
            args,
            json!({}),
            agent,
            relink,
            local,
            timeout,
            &python3(),
            stop,
        ),
    )
    .await
    .expect("the item ended within thirty seconds");
    ran
}

/// A reboot waits through dead links for a new boot id, times out by name, or fails at once when refused.
#[tokio::test]
async fn a_reboot_waits_for_a_new_boot_id_or_fails_by_name() {
    {
        let mut stop = watch::channel(false).1;
        let mut agent = before_the_reboot(None);
        let mut relink = Scripted::new(vec![
            Try::Down("ssh: connect to host target port 22: No route to host"),
            Try::Down("Timeout (12s) waiting for privilege escalation prompt"),
            Try::Up(FakeAgent::answering(
                raw_answer(5, 0, "old-boot-id\n").to_vec(),
            )),
            Try::Up(FakeAgent::answering(
                [
                    raw_answer(6, 0, "new-boot-id\n").to_vec(),
                    raw_answer(7, 0, "root\n").to_vec(),
                ]
                .concat(),
            )),
        ]);
        let result = reboot_item(json!({}), &mut agent, &mut relink, false, &mut stop)
            .await
            .expect("the item ran to its end");
        assert_eq!(
            Value::Object(result.0),
            json!({"changed": true, "elapsed": 0, "rebooted": true})
        );
        assert_eq!(
            relink.attempts, 4,
            "two down, one on the old boot, one back"
        );
        let links: Vec<&FakeAgent> = relink.retired.iter().chain([&agent]).collect();
        assert_eq!(
            sent_lines(&links),
            [
                vec![
                    "setup",
                    "cat /proc/sys/kernel/random/boot_id",
                    "find",
                    "/sbin/shutdown -r 0 \"Reboot initiated by Ansible\"",
                ],
                vec!["cat /proc/sys/kernel/random/boot_id"],
                vec!["cat /proc/sys/kernel/random/boot_id", "whoami"],
            ]
        );
        let first = batches_sent(links[0]);
        assert_eq!(
            Value::Object(first[0][0].args.clone()),
            json!({"gather_subset": ["min"]})
        );
        assert_eq!(
            Value::Object(first[2][0].args.clone()),
            json!({"paths": ["/sbin", "/bin", "/usr/sbin", "/usr/bin", "/usr/local/sbin"],
                "patterns": ["shutdown"], "file_type": "any"})
        );
        assert!(
            first[1][0].payload.is_none() && first[3][0].payload.is_none(),
            "the commands are the agent's own `raw`"
        );
    }
    {
        let mut stop = watch::channel(false).1;
        let mut agent = before_the_reboot(None);
        let mut relink = Scripted::new(Vec::new());
        relink.after = Some(|| FakeAgent::answering(raw_answer(9, 0, "old-boot-id\n").to_vec()));
        let result = reboot_item(
            json!({"reboot_timeout": 1}),
            &mut agent,
            &mut relink,
            false,
            &mut stop,
        )
        .await
        .expect("the item ran to its end");
        assert!(result.failed(), "{result:?}");
        assert_eq!(
            msg(&result),
            "Timed out waiting for last boot time check (timeout=1)"
        );
        assert_eq!(result.0["rebooted"], json!(true));
        assert!(relink.attempts > 1, "{}", relink.attempts);
    }
    {
        let mut stop = watch::channel(false).1;
        let refused = one_result(
            4,
            json!({"rc": 1, "stdout": "", "stderr": "Failed to set wall message, ignoring: Interactive authentication required.\n", "changed": true}),
        )
        .to_vec();
        let mut agent = before_the_reboot(Some(refused));
        let mut relink = Scripted::new(Vec::new());
        let result = reboot_item(json!({}), &mut agent, &mut relink, false, &mut stop)
            .await
            .expect("the item ran to its end");
        assert!(result.failed(), "{result:?}");
        assert_eq!(
            msg(&result),
            "Reboot command failed. Error was: ', Failed to set wall message, ignoring: Interactive authentication required.'"
        );
        assert_eq!(result.0["rebooted"], json!(false));
        assert_eq!(relink.attempts, 0);
    }
}

/// A shutdown nowhere to be found names the paths, and a local connection is never rebooted.
#[tokio::test]
async fn a_reboot_that_cannot_start_says_why() {
    {
        let mut stop = watch::channel(false).1;
        let mut agent = FakeAgent::answering(
            [
                vec![state("ab", true)],
                one_result(
                    1,
                    json!({"ansible_facts": {"ansible_distribution": "Ubuntu",
                        "ansible_distribution_version": "24.04", "ansible_os_family": "Debian"}}),
                )
                .to_vec(),
                raw_answer(2, 0, "old-boot-id\n").to_vec(),
                one_result(3, json!({"files": [], "matched": 0})).to_vec(),
            ]
            .concat(),
        );
        let mut relink = Scripted::new(Vec::new());
        let result = reboot_item(
            json!({"search_paths": ["/opt/bin", "/usr/local/bin"]}),
            &mut agent,
            &mut relink,
            false,
            &mut stop,
        )
        .await
        .expect("the item ran to its end");
        assert_eq!(
            msg(&result),
            "Unable to find command \"shutdown\" in search paths: ['/opt/bin', '/usr/local/bin']"
        );
        assert_eq!(batches_sent(&agent).len(), 3, "no shutdown sent");
    }
    {
        let mut stop = watch::channel(false).1;
        let mut agent = FakeAgent::answering(Vec::new());
        let mut relink = Scripted::new(Vec::new());
        let result = reboot_item(json!({}), &mut agent, &mut relink, true, &mut stop)
            .await
            .expect("the item ran to its end");
        assert_eq!(
            Value::Object(result.0),
            json!({"changed": false, "elapsed": 0, "failed": true,
                "msg": "Running reboot with local connection would reboot the control node.",
                "rebooted": false})
        );
        assert!(agent.sent.is_empty(), "{:?}", agent.sent);
    }
}

/// An interruption ends the pause between tries, a try that hangs, and the wait behind an abandoned probe.
#[tokio::test]
async fn an_interruption_ends_a_wait_at_once() {
    {
        let (tx, mut stop) = watch::channel(false);
        let mut agent = before_the_reboot(None);
        let mut relink = Scripted::new(vec![Try::Down("No route to host")]);
        relink.stop = Some(tx);
        relink.pause = Duration::from_secs(60);
        let ended = reboot_item(json!({}), &mut agent, &mut relink, false, &mut stop).await;
        assert!(
            matches!(ended, Err(Ok(BatchOutcome::Cancelled { .. }))),
            "{ended:?}"
        );
        assert_eq!(relink.attempts, 1);
    }
    {
        let (tx, mut stop) = watch::channel(false);
        let mut agent = before_the_reboot(None);
        let mut relink = Scripted::new(vec![Try::Hangs]);
        relink.stop = Some(tx);
        let ended = reboot_item(json!({}), &mut agent, &mut relink, false, &mut stop).await;
        assert!(
            matches!(ended, Err(Ok(BatchOutcome::Cancelled { .. }))),
            "{ended:?}"
        );
        assert_eq!(relink.attempts, 1);
    }
    {
        let (raise, mut stop) = watch::channel(false);
        let mut agent = before_the_reboot(None);
        let probing = FakeAgent::busy(vec![None], false);
        let mut relink = Scripted::new(vec![Try::Up(probing), Try::Hangs]);
        relink.stop = Some(raise);
        relink.stop_on = 2;
        let ran = reboot_item_under(
            Some(2),
            json!({}),
            &mut agent,
            &mut relink,
            false,
            &mut stop,
        )
        .await;
        assert!(
            matches!(ran, Err(Ok(BatchOutcome::Cancelled { .. }))),
            "{ran:?}"
        );
        assert_eq!(relink.attempts, 2);
    }
}

/// The connect timeout reaches the connection and becomes ssh's ConnectTimeout.
#[tokio::test]
async fn a_connect_timeout_reaches_the_connection_as_ssh_s_connecttimeout() {
    {
        let mut stop = watch::channel(false).1;
        let mut agent = before_the_reboot(None);
        let mut relink = Scripted::new(vec![Try::Up(FakeAgent::answering(
            [
                raw_answer(5, 0, "new-boot-id\n").to_vec(),
                raw_answer(6, 0, "root\n").to_vec(),
            ]
            .concat(),
        ))]);
        let result = reboot_item(
            json!({"connect_timeout": 3}),
            &mut agent,
            &mut relink,
            false,
            &mut stop,
        )
        .await
        .expect("the item ran to its end");
        assert!(!result.failed(), "{result:?}");
        assert_eq!(relink.connect_timeouts, [Some(Duration::from_secs(3))]);
    }
    {
        let host = crate::inventory::Host {
            name: "h1".to_string(),
            vars: BTreeMap::from([("ansible_host".to_string(), json!("probe-hostname"))]),
        };
        let defaults = ConnectionDefaults {
            remote_user: None,
            connection: None,
            private_key: None,
            host_key_checking: true,
            remote_tmp: "~/.ansible/tmp".to_string(),
            connect_timeout: Duration::from_secs(10),
            r#become: false,
            become_user: "root".to_string(),
            become_method: "sudo".to_string(),
            become_exe: None,
            become_flags: None,
            become_password: None,
            ssh_common_args: Vec::new(),
            ssh_extra_args: Vec::new(),
        };
        let kept = Transport::for_host(&host, &defaults).expect("an ssh transport");
        let argv = |t: &Transport| match t {
            Transport::Ssh(target) => target.ssh_argv("true").join(" "),
            Transport::Local => panic!("an ssh transport"),
        };
        let fresh = with_connect_timeout(&kept, Some(Duration::from_millis(2500)));
        assert!(
            argv(&fresh).contains("ConnectTimeout=3"),
            "{}",
            argv(&fresh)
        );
        assert!(argv(&kept).contains("ConnectTimeout=10"), "{}", argv(&kept));
        assert_eq!(with_connect_timeout(&kept, None), kept);
    }
}

/// An ssh transport to `h1` sharing its connection under `dir`, with no ssh configuration
/// read but the empty one, so the test does not depend on the account it runs under.
fn shared_to_h1(dir: &Path) -> Transport {
    let vars = json!({"ansible_host": "probe-hostname", "ansible_ssh_common_args": "-F /dev/null"});
    let defaults = ConnectionDefaults {
        remote_user: None,
        connection: None,
        private_key: None,
        host_key_checking: true,
        remote_tmp: "~/.ansible/tmp".to_string(),
        connect_timeout: Duration::from_secs(10),
        r#become: false,
        become_user: "root".to_string(),
        become_method: "sudo".to_string(),
        become_exe: None,
        become_flags: None,
        become_password: None,
        ssh_common_args: Vec::new(),
        ssh_extra_args: Vec::new(),
    };
    Transport::for_vars("h1", vars.as_object().expect("an object"), &defaults)
        .expect("an ssh transport")
        .shared(Some(dir), "h1", None)
}

async fn socket_of(transport: &Transport) -> Option<Box<Path>> {
    match transport {
        Transport::Ssh(target) => target.resolved().await.control_path,
        Transport::Local => None,
    }
}

/// `Relinker::relink` stops the host's master and reconnects under the `reboot` task's own
/// `connect_timeout`, naming one socket both times.
#[tokio::test]
async fn a_reboot_with_its_own_connect_timeout_stops_and_reopens_one_socket() {
    let kept = shared_to_h1(Path::new("/run/user/1000/volant-cm"));
    let socket = socket_of(&kept)
        .await
        .expect("shared; ssh -G must run here");
    let fresh = with_connect_timeout(&kept, Some(Duration::from_secs(5)));
    assert_ne!(fresh, kept, "the reconnection does carry its own timeout");
    assert_eq!(socket_of(&fresh).await, Some(socket));
}

/// A listener on the host's socket stands in for its master and counts the `ssh -O exit`
/// that reach it, closing each at once so the stop returns.
#[cfg(unix)]
fn fake_master(socket: &Path) -> Arc<std::sync::atomic::AtomicUsize> {
    let listener = std::os::unix::net::UnixListener::bind(socket).expect("a socket");
    let stops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&stops);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(stream);
        }
    });
    stops
}

/// The delegate `h1` rebooted twice while its delegating host kept two links to it, which share
/// one socket.
#[cfg(unix)]
#[tokio::test]
async fn a_failed_check_stops_the_master_and_retires_the_hosts_other_links() {
    use std::sync::atomic::Ordering::SeqCst;
    let dir = Path::new("/tmp").join(format!("volant-cm-kept-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let transport = shared_to_h1(&dir);
    let socket = socket_of(&transport)
        .await
        .expect("shared; ssh -G must run here");
    let stops = fake_master(&socket);
    let key = |become_user: Option<&str>| LinkKey {
        host: "h1".to_string(),
        become_user: become_user.map(str::to_string),
        become_program: None,
        transport: transport.clone(),
    };
    let (escalated, plain) = (key(Some("root")), key(None));
    let (on_m1, _) = FakeKept::new(FakeAnswer::Gone);
    let (on_m2, plain_asked) = FakeKept::new(FakeAnswer::Wedged);
    let mut links = HashMap::from([(escalated.clone(), on_m1), (plain.clone(), on_m2)]);
    let mut checked = HashMap::from([(escalated.clone(), 1), (plain.clone(), 1)]);
    let timeout = Duration::from_millis(200);

    let (stale, retired) = check_kept(&mut links, &mut checked, &escalated, 2, timeout).await;
    assert!(stale.is_some(), "the escalated link rode M1, gone");
    assert_eq!(
        stops.load(SeqCst),
        1,
        "M2 is stopped although M1 failed at once"
    );
    assert_eq!(retired.len(), 1, "the plain link is retired with it");
    assert!(!links.contains_key(&plain) && !checked.contains_key(&plain));

    // Both reconnect through the one new master: the escalated link now, the plain one on its
    // next use, with nothing left to check and nothing stopped.
    let (fresh, _) = FakeKept::new(FakeAnswer::Alive);
    links.insert(escalated.clone(), fresh);
    let (stale, retired) = check_kept(&mut links, &mut checked, &plain, 2, timeout).await;
    assert!(stale.is_none() && retired.is_empty());
    assert!(
        !plain_asked.load(SeqCst),
        "the retired plain link was never checked"
    );
    assert_eq!(stops.load(SeqCst), 1, "the new master was left alone");
    assert!(
        links.contains_key(&escalated),
        "the reopened escalated link is kept"
    );
}

/// The same two links, one step earlier: after the first reboot the plain link fails its check,
/// retires the escalated one and reopens the master as M2.
#[cfg(unix)]
#[tokio::test]
async fn a_key_with_no_kept_link_does_not_ride_a_master_older_than_the_last_reboot() {
    use std::sync::atomic::Ordering::SeqCst;
    let dir = Path::new("/tmp").join(format!("volant-cm-unproven-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let transport = shared_to_h1(&dir);
    let socket = socket_of(&transport)
        .await
        .expect("shared; ssh -G must run here");
    let stops = fake_master(&socket);
    let key = |become_user: Option<&str>| LinkKey {
        host: "h1".to_string(),
        become_user: become_user.map(str::to_string),
        become_program: None,
        transport: transport.clone(),
    };
    let (escalated, plain) = (key(Some("root")), key(None));
    let (plain_on_m1, _) = FakeKept::new(FakeAnswer::Gone);
    let (escalated_on_m1, _) = FakeKept::new(FakeAnswer::Gone);
    let mut links = HashMap::from([
        (plain.clone(), plain_on_m1),
        (escalated.clone(), escalated_on_m1),
    ]);
    let mut checked = HashMap::from([(plain.clone(), 0), (escalated.clone(), 0)]);
    let timeout = Duration::from_millis(200);

    // First reboot: the plain link fails, M1 is stopped, the escalated link retired.
    let (stale, retired) = check_kept(&mut links, &mut checked, &plain, 1, timeout).await;
    assert!(stale.is_some() && retired.len() == 1);
    assert_eq!(stops.load(SeqCst), 1);
    let (plain_on_m2, plain_asked) = FakeKept::new(FakeAnswer::Wedged);
    links.insert(plain.clone(), plain_on_m2);

    // Second reboot, and the escalated key, with no kept link, is used first.
    let (stale, retired) = check_kept(&mut links, &mut checked, &escalated, 2, timeout).await;
    assert!(stale.is_none(), "nothing was kept to fail");
    assert_eq!(
        stops.load(SeqCst),
        2,
        "M2, proven only at the first reboot, is stopped"
    );
    assert_eq!(retired.len(), 1, "the plain link on M2 is retired with it");
    assert!(!links.contains_key(&plain) && !checked.contains_key(&plain));

    // The escalated key connects and so proves the socket at this count; the plain key then
    // rides that master, unchecked and without another stop.
    let (fresh, _) = FakeKept::new(FakeAnswer::Alive);
    links.insert(escalated.clone(), fresh);
    let (stale, retired) = check_kept(&mut links, &mut checked, &plain, 2, timeout).await;
    assert!(stale.is_none() && retired.is_empty());
    assert!(!plain_asked.load(SeqCst));
    assert_eq!(
        stops.load(SeqCst),
        2,
        "the master the escalated key opened is left alone"
    );
}

/// The `timeout` keyword bounds a plugin's whole item, the host's return included: a `reboot`
/// under `timeout: 2` ends at two seconds.
#[tokio::test]
async fn the_timeout_keyword_bounds_a_plugin_s_whole_item() {
    let mut stop = watch::channel(false).1;
    let mut agent = before_the_reboot(None);
    let mut relink = Scripted::new(Vec::new());
    relink.after = Some(|| FakeAgent::answering(raw_answer(9, 0, "old-boot-id\n").to_vec()));
    let result = reboot_item_under(
        Some(2),
        json!({}),
        &mut agent,
        &mut relink,
        false,
        &mut stop,
    )
    .await
    .expect("the item ran to its end");
    assert_eq!(result, TaskResult::timed_out(2));
    // Each sub-task carries what is left of the item's time, in whole seconds: all of it at
    // the start, one second of it once the host has been away for more than one.
    let timeouts = |link: &FakeAgent| -> Vec<Option<u64>> {
        batches_sent(link).iter().map(|b| b[0].timeout).collect()
    };
    assert_eq!(timeouts(&relink.retired[0]), [Some(2); 4]);
    assert_eq!(timeouts(relink.retired.last().expect("a probe")), [Some(1)]);
}

/// A probe the item's deadline abandoned is cancelled on the host.
#[tokio::test]
async fn a_probe_abandoned_at_the_deadline_is_cancelled() {
    fn fresh() -> FakeAgent {
        FakeAgent::busy(vec![Some(raw_answer(0, 0, "next\n").to_vec())], true)
    }
    for confirms in [true, false] {
        let mut stop = watch::channel(false).1;
        let mut agent = before_the_reboot(None);
        let probing = FakeAgent::busy(
            vec![None, Some(raw_answer(0, 0, "next\n").to_vec())],
            confirms,
        );
        let mut relink = Scripted::new(vec![Try::Up(probing)]);
        relink.after = Some(fresh);
        let result = reboot_item_under(
            Some(2),
            json!({}),
            &mut agent,
            &mut relink,
            false,
            &mut stop,
        )
        .await
        .expect("the item ran to its end");
        assert_eq!(result, TaskResult::timed_out(2), "{confirms}");
        // A confirmed cancel keeps the link; an unconfirmed one replaces it.
        assert_eq!(relink.attempts, if confirms { 1 } else { 2 }, "{confirms}");
        let next = vec![protocol_task(&task("command"), &bare_item(), None)];
        let (received, _) = tokio::time::timeout(
            Duration::from_secs(10),
            run_agent_batch(
                &mut agent,
                "h1",
                1000,
                next,
                None,
                &[],
                &mut stop,
                &mut false,
                &mut Vec::new(),
            ),
        )
        .await
        .expect("the next batch was answered rather than dropped");
        let answered = received[0].clone().expect("a result").0;
        assert_eq!(answered["stdout"], json!("next\n"), "{confirms}");
    }
}

/// A `shutdown` that answered and then took the link down reports its answer: a refusal written
/// just before the connection closed is still a refusal.
#[tokio::test]
async fn a_shutdown_answer_before_the_link_went_is_kept() {
    let mut stop = watch::channel(false).1;
    let answered = vec![FromAgent::TaskResult {
        batch: 4,
        index: 0,
        result: TaskResult(vars(
            json!({"rc": 1, "stdout": "", "stderr": "shutdown: Permission denied\n"}),
        )),
        ran: None,
    }];
    let mut agent = before_the_reboot(Some(answered));
    let mut relink = Scripted::new(Vec::new());
    let result = reboot_item(json!({}), &mut agent, &mut relink, false, &mut stop)
        .await
        .expect("the item ran to its end");
    assert_eq!(
        msg(&result),
        "Reboot command failed. Error was: ', shutdown: Permission denied'"
    );
    assert_eq!(relink.attempts, 0);
}

/// The reference's schedule between two tries: 1, 2, 4, 8, then 12 seconds for good.
#[test]
fn the_pause_doubles_from_one_second_up_to_twelve() {
    let pauses: Vec<u64> = (1..=7)
        .map(|n| Relink::<FakeAgent>::pause(&NoRelink, n).as_secs())
        .collect();
    assert_eq!(pauses, [1, 2, 4, 8, 12, 12, 12]);
}

/// Real agents on the local connection, found the way the other real-link test finds them.
#[cfg(unix)]
fn local_agents() -> (AgentSource, ConnectionDefaults) {
    let exe = std::env::current_exe().expect("this test's own binary path");
    let agent_dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("deps/ has a parent")
        .to_path_buf();
    // SAFETY: nextest gives every test its own process, and nothing else here reads it.
    unsafe {
        std::env::set_var("VOLANT_AGENT_DIR", &agent_dir);
    }
    let defaults = ConnectionDefaults {
        remote_user: None,
        connection: None,
        private_key: None,
        host_key_checking: true,
        remote_tmp: "~/.ansible/tmp".to_string(),
        connect_timeout: Duration::from_secs(10),
        r#become: false,
        become_user: "root".to_string(),
        become_method: "sudo".to_string(),
        become_exe: None,
        become_flags: None,
        become_password: None,
        ssh_common_args: Vec::new(),
        ssh_extra_args: Vec::new(),
    };
    (AgentSource::discover(), defaults)
}

#[cfg(unix)]
fn local_key(host: &str, user: Option<&str>) -> LinkKey {
    LinkKey {
        host: host.to_string(),
        become_user: user.map(str::to_string),
        become_program: None,
        transport: Transport::Local,
    }
}

/// Reconnecting drops every link kept for the host - its own and the escalated one - and no
/// other host's, and puts a fresh link in place of the one the plugin holds.
#[cfg(unix)]
#[tokio::test]
async fn reconnecting_drops_the_host_s_links_and_no_other() {
    let (source, defaults) = local_agents();
    let key = local_key;
    let reboots = Reboots::default();
    let mut links = HashMap::new();
    let mut checked = HashMap::new();
    for k in [key("h1", Some("root")), key("h2", None)] {
        let link = connect(&Transport::Local, &source, &defaults, None)
            .await
            .expect("a real agent");
        checked.insert(k.clone(), 0);
        links.insert(k, link);
    }
    let mine = key("h1", None);
    checked.insert(mine.clone(), 0);
    let mut held = connect(&Transport::Local, &source, &defaults, None)
        .await
        .expect("a real agent");
    let mut relink = Relinker {
        links: &mut links,
        checked: &mut checked,
        reboots: &reboots,
        key: &mine,
        escalation: None,
        agents: &source,
        defaults: &defaults,
    };
    relink.relink(&mut held, None).await.expect("a fresh link");
    let left: Vec<&LinkKey> = links.keys().collect();
    assert_eq!(left, [&key("h2", None)]);
    assert_eq!(checked, HashMap::from([(key("h2", None), 0)]));
    assert_eq!((reboots.of("h1"), reboots.of("h2")), (1, 0));
    held.handshake().await.expect("the fresh link answers");
    held.shutdown().await;
    for (_, link) in links.drain() {
        link.shutdown().await;
    }
}

/// A link another host's driver keeps to a host this one rebooted is checked again before it is
/// reused, and replaced when the reboot killed it.
#[cfg(unix)]
#[tokio::test]
async fn a_link_another_driver_keeps_is_checked_again_after_a_reboot() {
    let (source, defaults) = local_agents();
    let (stop_tx, stop) = watch::channel(false);
    let options = RunOptions {
        defaults: defaults.clone(),
        forks: 1,
        force_handlers: false,
        batching: false,
        control_dir: None,
        stop,
        abort: Arc::new(super::super::Abort::new(stop_tx)),
        reboots: Arc::default(),
        profile: Arc::default(),
        step: None,
    };
    let to_h1 = local_key("h1", None);
    // `h2`'s driver: its link to `h1`, proved alive once this play.
    let mut h2_links = HashMap::new();
    let mut h2_checked = HashMap::new();
    reuse_or_connect(
        &mut h2_links,
        &mut h2_checked,
        &to_h1,
        None,
        &source,
        &options,
    )
    .await
    .expect("h2 reaches h1");
    // `h1`'s driver reboots `h1`, which kills every agent on it, `h2`'s included.
    h2_links
        .get_mut(&to_h1)
        .expect("h2 kept its link")
        .kill()
        .await;
    let mut h1_links = HashMap::new();
    let mut h1_checked = HashMap::new();
    let mut held = connect(&Transport::Local, &source, &defaults, None)
        .await
        .expect("a real agent");
    Relinker {
        links: &mut h1_links,
        checked: &mut h1_checked,
        reboots: &options.reboots,
        key: &to_h1,
        escalation: None,
        agents: &source,
        defaults: &defaults,
    }
    .relink(&mut held, None)
    .await
    .expect("h1 is back");
    // `h2` delegates to `h1` again.
    let link = reuse_or_connect(
        &mut h2_links,
        &mut h2_checked,
        &to_h1,
        None,
        &source,
        &options,
    )
    .await
    .expect("h2 reaches h1 again");
    let answered = link.handshake().await;
    held.shutdown().await;
    for (_, link) in h2_links.drain() {
        link.shutdown().await;
    }
    answered.expect("the link h2 got is alive, not the one the reboot killed");
}

/// The three results land where the reference puts them, a failed task lands nowhere, and a
/// module that is not the action never moves the inventory whatever key its result carries.
#[test]
fn inventory_and_stats_results_are_applied_by_the_action_alone() {
    let tmp = scratch();
    let inv = crate::inventory::Inventory::parse_ini(
        "[g]
h1
",
    )
    .unwrap();
    let mut store = VarStore::new(&inv, None, tmp.path(), Map::new()).unwrap();
    let ok = |value: Value| vec![(None, TaskResult(value.as_object().unwrap().clone()))];
    let added = json!({"add_host": {"host_name": "n1", "groups": ["g1"], "host_vars": {"x": 1}}});
    record_inventory_results(&mut store, &task("command"), "h1", &ok(added.clone()));
    assert!(store.take_inventory_changes().is_none());
    let mut failed = added.clone();
    failed["failed"] = json!(true);
    record_inventory_results(&mut store, &task("add_host"), "h1", &ok(failed));
    assert!(store.take_inventory_changes().is_none());
    record_inventory_results(
        &mut store,
        &task("ansible.builtin.add_host"),
        "h1",
        &ok(added),
    );
    record_inventory_results(
        &mut store,
        &task("group_by"),
        "h1",
        &ok(json!({"add_group": "role_web", "parent_groups": ["all_roles"]})),
    );
    record_inventory_results(
        &mut store,
        &task("set_stats"),
        "h1",
        &ok(json!({"ansible_stats": {"data": {"n": 1}, "per_host": true, "aggregate": true}})),
    );
    // A loop whose item 2 failed adds nothing, not even items 1 and 3 (measured); a
    // delegated task applies to the host it was written for, which is the one passed in.
    let added = |n: &str| json!({"add_host": {"host_name": n, "groups": ["lg"], "host_vars": {}}});
    let mut second = added("L2");
    second["failed"] = json!(true);
    let looped: Vec<_> = [added("L1"), second, added("L3")]
        .into_iter()
        .map(|v| (None, TaskResult(v.as_object().unwrap().clone())))
        .collect();
    record_inventory_results(&mut store, &task("add_host"), "h1", &looped);
    record_inventory_results(
        &mut store,
        &task("group_by"),
        "h1",
        &ok(json!({"add_group": "delegated"})),
    );
    let grown = store.take_inventory_changes().expect("grown");
    let groups = grown.groups();
    assert!(!groups.contains_key("lg"));
    assert_eq!(groups["delegated"], ["h1"]);
    assert_eq!(groups["g1"], ["n1"]);
    assert_eq!(groups["all_roles"], ["h1"]);
    assert_eq!(store.custom_stats().hosts()["h1"]["n"], json!(1));
}
