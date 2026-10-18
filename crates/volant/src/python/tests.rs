// SPDX-License-Identifier: GPL-3.0-or-later
use super::builder::{exchange, find_python, refusal_for, resolve_exchange, start_from};
use super::reach::modules_to_build;
use super::union_build::{gathering_only, no_builder, union_from};
use super::*;
use crate::testing::scratch;
use rstest::rstest;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io::Cursor;
use std::process::{Command, Stdio};
use volant_protocol::frame::{read_frame, write_frame};

/// Two modules with one short name stay two modules: a collection's module is built under its
/// full name and looked up by it, and a builtin keeps its short key.
#[test]
fn two_modules_of_one_short_name_stay_two_in_the_union() {
    let answer = br#"{"zip_b64": "UEsD", "modules": {
            "sysctl": {"module_fqn": "ansible.modules.sysctl", "profile": "legacy",
                "rlimit_nofile": 0, "extensions": {}},
            "ansible.posix.sysctl": {"module_fqn":
                "ansible_collections.ansible.posix.plugins.modules.sysctl", "profile": "legacy",
                "rlimit_nofile": 0, "extensions": {}},
            "community.general.sysctl": {"module_fqn":
                "ansible_collections.community.general.plugins.modules.sysctl",
                "profile": "legacy", "rlimit_nofile": 0, "extensions": {}}}}"#;
    let asked = ["sysctl", "ansible.posix.sysctl", "community.general.sysctl"].map(str::to_string);
    let union = exchange(Vec::new(), framed(answer), &asked).unwrap().0;
    assert_eq!(union.modules.len(), 3, "{:?}", union.modules.keys());
    assert_eq!(
        union.modules[payload_key("ansible.posix.sysctl")].module_fqn,
        "ansible_collections.ansible.posix.plugins.modules.sysctl"
    );
    assert_eq!(
        union.modules[payload_key("community.general.sysctl")].module_fqn,
        "ansible_collections.community.general.plugins.modules.sysctl"
    );
    // `ansible.builtin.ping` is never the `ping` a `library/` shadows.
    assert_eq!(payload_key("ansible.builtin.ping"), "ansible.builtin.ping");
    assert_eq!(payload_key("ansible.legacy.ping"), "ping");
    let built = modules_to_build([
        "ansible.posix.sysctl",
        "community.general.sysctl",
        "ansible.builtin.ping",
        "ping",
    ]);
    assert_eq!(
        built.iter().map(String::as_str).collect::<Vec<_>>(),
        [
            "ansible.builtin.ping",
            "ansible.posix.sysctl",
            "community.general.sysctl",
            "ping"
        ]
    );
}

/// The helper's resolution, read one name at a time.
#[test]
fn the_helper_resolution_is_read_name_by_name() {
    let answer = br#"{"resolved": {
            "a.b.mod": {"module": "a.b.mod", "collection": ["a.b", "1.2.3"]},
            "a.b.act": {"action_plugin": "a.b.act"},
            "a.b.none": {"missing": null},
            "c.d.mod": {"missing": "c.d"},
            "a.b.gone": {"unusable": "The 'a.b.gone' module has been removed."}}}"#;
    let asked = ["a.b.mod", "a.b.act", "a.b.none", "c.d.mod", "a.b.gone"].map(str::to_string);
    let mut sent = Vec::new();
    let resolved = resolve_exchange(&mut sent, framed(answer), &asked).unwrap();
    assert!(
        String::from_utf8_lossy(&sent).contains(r#"{"resolve":["a.b.mod","#),
        "{sent:?}"
    );
    assert_eq!(
        resolved.into_iter().collect::<Vec<_>>(),
        [
            (
                "a.b.act".to_string(),
                Resolved::ActionPlugin {
                    fqcn: "a.b.act".into(),
                    with_module: false
                }
            ),
            (
                "a.b.gone".to_string(),
                Resolved::Unusable {
                    reason: "The 'a.b.gone' module has been removed.".into()
                }
            ),
            (
                "a.b.mod".to_string(),
                Resolved::Module {
                    fqcn: "a.b.mod".into(),
                    collection: Some(("a.b".into(), "1.2.3".into())),
                }
            ),
            (
                "a.b.none".to_string(),
                Resolved::Missing { collection: None }
            ),
            (
                "c.d.mod".to_string(),
                Resolved::Missing {
                    collection: Some("c.d".into())
                }
            ),
        ]
    );
    for (answer, says) in [
        (&br#"{"resolved": {}}"#[..], "did not resolve 'x.y.z'"),
        (br#"{"resolved": {"x.y.z": {"other": 1}}}"#, "cannot read"),
        (
            br#"{"resolved": {"x.y.z": {"module": "x.y.z", "collection": "x.y"}}}"#,
            "not as a name and a version",
        ),
        (
            br#"{"error": "RuntimeError: boom"}"#,
            "could not resolve the modules: RuntimeError: boom",
        ),
    ] {
        let err = resolve_exchange(Vec::new(), framed(answer), &["x.y.z".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains(says), "{err}");
    }
}

/// A controller without ansible-core refuses a collection module in a play, but not one named
/// only in a role file for another platform.
#[test]
fn a_role_file_alone_never_demands_ansible_core() {
    let none = || Err(anyhow::anyhow!("{}", refusal_for("python3")));
    let modules = std::collections::BTreeSet::from(["community.general.zypper".to_string()]);
    assert_eq!(
        union_from(none, &modules, &[], None, &mut |_| {}).unwrap(),
        None
    );
    let named = [("Z".to_string(), "community.general.zypper".to_string())];
    let err = union_from(none, &modules, &named, None, &mut |_| {})
        .unwrap_err()
        .to_string();
    assert!(err.contains("VOLANT_PYTHON"), "{err}");
    let modules = std::collections::BTreeSet::from([
        "community.general.zypper".to_string(),
        "ping".to_string(),
    ]);
    let err = union_from(none, &modules, &[], None, &mut |_| {})
        .unwrap_err()
        .to_string();
    assert!(err.contains("VOLANT_PYTHON"), "{err}");

    // A dotted typo a task gives is the reference's misspelling first, then why nothing could
    // tell a typo from a missing collection. Red if it reads as the interpreter refusal
    // alone, which sends the operator to install ansible-core to fix a typo.
    let modules = std::collections::BTreeSet::from(["ansible.builtins.debug".to_string()]);
    let named = [("D".to_string(), "ansible.builtins.debug".to_string())];
    let err = union_from(none, &modules, &named, None, &mut |_| {}).unwrap_err();
    assert_eq!(crate::stats::error_code(&err), 4, "{err:#}");
    let err = format!("{err:#}");
    assert!(
        err.starts_with(
            "task 'D': couldn't resolve module/action 'ansible.builtins.debug'. This often indicates a misspelling"
        ),
        "{err}"
    );
    assert!(err.contains("VOLANT_PYTHON"), "{err}");
}

/// A collection written for the test, under its own `ANSIBLE_COLLECTIONS_PATH`, holding every
/// shape the controller has to tell apart. Measured on ansible-core 2.19.12 with this very
/// layout: `routed` has `action_plugin` set by `runtime.yml`, `winmod` resolves to its `.ps1`
/// without `mod_type` and to a documentation-only `.py` with it, `onlyps` resolves to nothing
/// with it, `gone` raises `AnsiblePluginRemovedError` (as `community.general.atomic_host` does
/// in community.general 13.4.0), and `moved` resolves to nothing with a `redirect_list` ending
/// in `absentns.absent.moved`.
fn fixture_collection(root: &std::path::Path) {
    let coll = root.join("ansible_collections/volanttest/coll");
    for dir in ["plugins/modules", "plugins/action", "meta"] {
        std::fs::create_dir_all(coll.join(dir)).unwrap();
    }
    let write = |path: &str, text: &str| std::fs::write(coll.join(path), text).unwrap();
    write(
        "MANIFEST.json",
        r#"{"collection_info": {"namespace": "volanttest", "name": "coll", "version": "1.0.0"}}"#,
    );
    let module = "from ansible.module_utils.basic import AnsibleModule\n\ndef main():\n    AnsibleModule(argument_spec={}).exit_json(changed=False)\n\nif __name__ == '__main__':\n    main()\n";
    write("plugins/modules/good.py", module);
    write("plugins/modules/routed.py", module);
    write("plugins/modules/winmod.py", "DOCUMENTATION = 'x'\n");
    write("plugins/modules/winmod.ps1", "#!powershell\n");
    write("plugins/modules/onlyps.ps1", "#!powershell\n");
    write(
        "plugins/action/act.py",
        "from ansible.plugins.action import ActionBase\n\nclass ActionModule(ActionBase):\n    pass\n",
    );
    write(
        "meta/runtime.yml",
        "requires_ansible: '>=2.15'\nplugin_routing:\n  modules:\n    routed:\n      action_plugin: volanttest.coll.act\n    gone:\n      tombstone:\n        removal_version: 1.0.0\n        warning_text: use good instead\n    moved:\n      redirect: absentns.absent.moved\n",
    );
}

/// Dropping the builder lets ansible-core's exit cleanup run, so the `ansible-local-*`
/// directory it creates at import is gone once the helper is.
#[test]
fn dropping_the_builder_leaves_no_ansible_local_directory() {
    let python = match find_python(
        std::env::var("VOLANT_PYTHON").ok().as_deref(),
        std::env::var("VIRTUAL_ENV").ok().as_deref(),
        false,
    ) {
        Ok(python) => python,
        Err(why) => return skip_or_fail(&format!("{why:#}")),
    };
    let root_guard = scratch();
    let root = root_guard.path().to_path_buf();
    std::fs::create_dir_all(&root).unwrap();
    let local = root.display().to_string();
    let mut builder =
        PythonBuilder::under_with(&python, &[("ANSIBLE_LOCAL_TEMP", &local)]).unwrap();
    builder.union(&["ping".to_string()]).unwrap();
    let during = std::fs::read_dir(&root).unwrap().count();
    drop(builder);
    let after = std::fs::read_dir(&root).unwrap().count();
    assert_eq!(during, 1, "the helper made its ansible-local directory");
    assert_eq!(after, 0, "the directory outlived the helper");
}

/// Only ansible-core's own module is reported as core.
#[test]
fn only_ansible_core_s_own_module_is_core() {
    let python = match find_python(
        std::env::var("VOLANT_PYTHON").ok().as_deref(),
        std::env::var("VIRTUAL_ENV").ok().as_deref(),
        false,
    ) {
        Ok(python) => python,
        Err(why) => return skip_or_fail(&format!("{why:#}")),
    };
    let root_guard = scratch();
    let root = root_guard.path().to_path_buf();
    fixture_collection(&root);
    let library = root.join("library");
    std::fs::create_dir_all(&library).unwrap();
    std::fs::write(
        library.join("stat.py"),
        "from ansible.module_utils.basic import AnsibleModule\n\ndef main():\n    AnsibleModule(argument_spec={}).exit_json(changed=False)\n\nif __name__ == '__main__':\n    main()\n",
    )
    .unwrap();
    let collections = root.display().to_string();
    let library = library.display().to_string();
    let builder = |env: &[(&str, &str)]| PythonBuilder::under_with(&python, env).unwrap();
    let union = builder(&[("ANSIBLE_COLLECTIONS_PATH", &collections)])
        .union(&["stat".to_string(), "volanttest.coll.good".to_string()])
        .unwrap()
        .0;
    assert!(union.modules["stat"].core);
    assert!(!union.modules["volanttest.coll.good"].core);
    let union = builder(&[("ANSIBLE_LIBRARY", &library)])
        .union(&["stat".to_string(), "ping".to_string()])
        .unwrap()
        .0;
    // Measured on ansible-core 2.19.12: the wrapper names it `ansible.legacy.stat`, which
    // an agent looking natives up by `ansible.modules.<name>` does not match either. `core`
    // is the controller's own word on it, whatever the wrapper's naming does next.
    assert_eq!(union.modules["stat"].module_fqn, "ansible.legacy.stat");
    assert!(!union.modules["stat"].core, "the library's stat.py");
    assert!(union.modules["ping"].core);
    let mut union = union;
    union.natives.enabled = true;
    assert!(
        !union
            .payload("ping")
            .expect("the union holds it")
            .force_python
    );
    assert!(
        union
            .payload("stat")
            .expect("the union holds it")
            .force_python
    );
}

/// Every shape a collection's name can take on the controller, each answered on its own, and
/// only the buildable ones reaching the union.
#[test]
#[expect(clippy::too_many_lines)]
fn a_collection_the_controller_cannot_run_is_answered_name_by_name() {
    let python = match find_python(
        std::env::var("VOLANT_PYTHON").ok().as_deref(),
        std::env::var("VIRTUAL_ENV").ok().as_deref(),
        false,
    ) {
        Ok(python) => python,
        Err(why) => return skip_or_fail(&format!("{why:#}")),
    };
    let root_guard = scratch();
    let root = root_guard.path().to_path_buf();
    fixture_collection(&root);
    let path = root.display().to_string();
    let start = || PythonBuilder::under_with(&python, &[("ANSIBLE_COLLECTIONS_PATH", &path)]);
    let names = ["good", "routed", "winmod", "onlyps", "gone", "moved"]
        .map(|m| format!("volanttest.coll.{m}"));
    let resolved = start().unwrap().resolve(&names).unwrap();
    assert_eq!(
        resolved["volanttest.coll.good"],
        Resolved::Module {
            fqcn: "volanttest.coll.good".into(),
            collection: Some(("volanttest.coll".into(), "1.0.0".into())),
        }
    );
    assert_eq!(
        resolved["volanttest.coll.routed"],
        // The plugin `runtime.yml` routes the module to runs, and the module builds.
        Resolved::ActionPlugin {
            fqcn: "volanttest.coll.act".into(),
            with_module: true
        }
    );
    assert!(
        matches!(&resolved["volanttest.coll.winmod"], Resolved::Unusable { reason } if reason.contains("'old'")),
        "{:?}",
        resolved["volanttest.coll.winmod"]
    );
    assert_eq!(
        resolved["volanttest.coll.onlyps"],
        Resolved::Missing { collection: None }
    );
    assert!(
        matches!(&resolved["volanttest.coll.gone"], Resolved::Unusable { reason } if reason.contains("has been removed. use good instead")),
        "{:?}",
        resolved["volanttest.coll.gone"]
    );
    assert_eq!(
        resolved["volanttest.coll.moved"],
        Resolved::Missing {
            collection: Some("absentns.absent".into())
        }
    );

    // Found only by the role scan, the unusable names stay out and the run builds the rest.
    let modules = std::collections::BTreeSet::from(names.clone());
    let built = union_from(start, &modules, &[], None, &mut |_| {})
        .unwrap()
        .expect("good is built");
    assert_eq!(
        built.modules.keys().collect::<Vec<_>>(),
        ["volanttest.coll.good", "volanttest.coll.routed"]
    );
    // What was set aside is kept in the pre-flight's words, even with nothing left to build,
    // and an include reaching one says it: the install hint for a missing collection, not
    // the sentence for a name nothing read. Red if only removed modules are kept, or if the
    // reasons go with a union that holds no module.
    let set_aside = std::collections::BTreeSet::from(
        ["volanttest.coll.moved", "volanttest.coll.gone"].map(str::to_string),
    );
    let kept = union_from(start, &set_aside, &[], None, &mut |_| {})
        .unwrap()
        .expect("the reasons are kept");
    assert!(kept.modules.is_empty(), "{:?}", kept.modules);
    let included = crate::playbook::parse(
        "- hosts: all\n  gather_facts: false\n  tasks:\n    - name: Moved\n      volanttest.coll.moved: a=1\n",
        "included.yml",
    )
    .unwrap();
    let compiled = crate::compile::compile(
        &included.plays[0],
        &crate::roles::RoleSearch::default(),
        &crate::compile::TagSelection::new(Vec::new(), Vec::new()),
    )
    .unwrap();
    let text = format!(
        "{:#}",
        crate::preflight::check_built(&compiled, Some(&kept)).unwrap_err()
    );
    assert!(
        text.starts_with("task 'Moved': couldn't resolve module/action 'volanttest.coll.moved'")
            && text
                .ends_with("install it with volant runtime install --collections absentns.absent"),
        "{text}"
    );
    // Named by a task, each is refused by its own sentence.
    for (module, says) in [
        (
            "volanttest.coll.winmod",
            "cannot run: module 'volanttest.coll.winmod' is built as 'old'",
        ),
        (
            "volanttest.coll.gone",
            "cannot run: AnsiblePluginRemovedError: The 'volanttest.coll.gone' module has been removed",
        ),
        (
            "volanttest.coll.moved",
            "volant runtime install --collections absentns.absent",
        ),
    ] {
        let named = [("T".to_string(), module.to_string())];
        let err = union_from(start, &modules, &named, None, &mut |_| {}).unwrap_err();
        assert!(format!("{err:#}").contains(says), "{module}: {err:#}");
    }
}

/// The version of `ansible.posix` the collection tests are written against, the one
/// installed on the controller that measured them.
const ANSIBLE_POSIX: &str = "2.2.2";

/// The same for `community.general`.
const COMMUNITY_GENERAL: &str = "13.4.0";

/// The helper under the controller's ansible-core, or `None` after saying why the test is
/// skipped. Under `VOLANT_PYTHON` there is no skip.
fn helper_or_skip() -> Option<PythonBuilder> {
    match PythonBuilder::start() {
        Ok(helper) => Some(helper),
        Err(why) => {
            skip_or_fail(&format!("{why:#}"));
            None
        }
    }
}

fn skip_or_fail(why: &str) {
    if std::env::var_os("VOLANT_PYTHON").is_some() {
        panic!("VOLANT_PYTHON cannot resolve the pinned collections: {why}");
    }
    eprintln!(
        "skipped: {why}. Set VOLANT_PYTHON to an ansible-core with ansible.posix {ANSIBLE_POSIX}."
    );
}

/// The controller's own ansible-core answers what its collections hold, and a collection's
/// module lands in the union under its full name.
#[test]
#[expect(clippy::too_many_lines)]
fn the_controller_resolves_what_its_collections_hold() {
    let Some(mut helper) = helper_or_skip() else {
        return;
    };
    let asked = [
        "ansible.posix.sysctl",
        "ansible.posix.synchronize",
        "ansible.posix.nosuch",
        "nosuch.coll.mod",
        "nodots.x",
        "community.general.ufw",
        "community.general.atomic_host",
    ]
    .map(str::to_string);
    let resolved = helper.resolve(&asked).unwrap();
    let sysctl = Resolved::Module {
        fqcn: "ansible.posix.sysctl".into(),
        collection: Some(("ansible.posix".into(), ANSIBLE_POSIX.into())),
    };
    if resolved["ansible.posix.sysctl"] != sysctl {
        skip_or_fail(&format!(
            "the controller's ansible.posix.sysctl is {:?}, not ansible.posix {ANSIBLE_POSIX}",
            resolved["ansible.posix.sysctl"]
        ));
        return;
    }
    assert_eq!(
        resolved["ansible.posix.synchronize"],
        Resolved::ActionPlugin {
            fqcn: "ansible.posix.synchronize".into(),
            with_module: true
        }
    );
    assert_eq!(
        resolved["ansible.posix.nosuch"],
        Resolved::Missing { collection: None }
    );
    assert_eq!(
        resolved["nosuch.coll.mod"],
        Resolved::Missing {
            collection: Some("nosuch.coll".into())
        }
    );
    assert_eq!(resolved["nodots.x"], Resolved::Missing { collection: None });
    // community.general 13.4.0's `runtime.yml` tombstones `atomic_host` (removed in 13.0.0):
    // the loader raises for it, and that is this name's answer, not the request's.
    let general = Resolved::Module {
        fqcn: "community.general.ufw".into(),
        collection: Some(("community.general".into(), COMMUNITY_GENERAL.into())),
    };
    if resolved["community.general.ufw"] == general {
        assert!(
            matches!(&resolved["community.general.atomic_host"], Resolved::Unusable { reason }
                if reason.contains("'community.general.atomic_host' module has been removed")),
            "{:?}",
            resolved["community.general.atomic_host"]
        );
        // Named only in a file an include reads, the removed module stays out of the union,
        // and the include that reaches it fails with the reason the controller gave.
        let modules = std::collections::BTreeSet::from(
            ["community.general.atomic_host", "ping"].map(str::to_string),
        );
        let built = union_from(PythonBuilder::start, &modules, &[], None, &mut |_| {})
            .unwrap()
            .expect("ping is built");
        let included = crate::playbook::parse(
            "- hosts: all\n  gather_facts: false\n  tasks:\n    - name: Atomic\n      community.general.atomic_host: revision=latest\n",
            "included.yml",
        )
        .unwrap();
        let compiled = crate::compile::compile(
            &included.plays[0],
            &crate::roles::RoleSearch::default(),
            &crate::compile::TagSelection::new(Vec::new(), Vec::new()),
        )
        .unwrap();
        let text = format!(
            "{:#}",
            crate::preflight::check_built(&compiled, Some(&built)).unwrap_err()
        );
        assert!(
            text.starts_with("task 'Atomic': module 'community.general.atomic_host' cannot run: ")
                && text.contains("'community.general.atomic_host' module has been removed"),
            "{text}"
        );
    } else {
        skip_or_fail(&format!(
            "the controller's community.general.ufw is {:?}, not community.general {COMMUNITY_GENERAL}",
            resolved["community.general.ufw"]
        ));
    }
    let union = helper
        .union(&["ansible.posix.sysctl".to_string(), "ping".to_string()])
        .unwrap()
        .0;
    assert_eq!(
        union.modules["ansible.posix.sysctl"].module_fqn,
        "ansible_collections.ansible.posix.plugins.modules.sysctl"
    );
    assert_eq!(union.modules["ping"].module_fqn, "ansible.modules.ping");

    let modules = std::collections::BTreeSet::from(
        ["ansible.posix.synchronize", "nosuch.coll.mod", "ping"].map(str::to_string),
    );
    let built = union_from(PythonBuilder::start, &modules, &[], None, &mut |_| {})
        .unwrap()
        .expect("ping is built");
    assert_eq!(
        built.modules.keys().collect::<Vec<_>>(),
        ["ansible.posix.synchronize", "ping"]
    );
    let named = [("Sync".to_string(), "ansible.posix.synchronize".to_string())];
    // Served by an action plugin, the name runs in the helper; its module is in the union.
    union_from(PythonBuilder::start, &modules, &named, None, &mut |_| {}).unwrap();
    assert!(crate::action_plugins::kind("ansible.posix.synchronize").is_some());
}

/// The controller's interpreter is chosen in one order and that order is visible: an explicit
/// `VOLANT_PYTHON`, then the active virtualenv, then `python3` on `PATH`.
#[test]
fn the_controller_interpreter_follows_one_visible_order() {
    assert_eq!(candidates(Some("/opt/py"), Some("/venv"))[0], "/opt/py");
    assert_eq!(candidates(None, Some("/venv"))[0], "/venv/bin/python");
    assert_eq!(candidates(None, None), vec!["python3".to_string()]);
}

/// An interpreter without ansible-core is refused before connecting; a gather-only run is told it can stop gathering.
#[test]
fn an_interpreter_without_ansible_core_is_refused_in_words_that_help() {
    {
        let err = refusal_for("/usr/bin/python3");
        assert!(err.contains("/usr/bin/python3"), "{err}");
        assert!(err.contains("ansible-core"), "{err}");
        assert!(err.contains("VOLANT_PYTHON"), "{err}");
    }
    {
        let base = anyhow::anyhow!("{}", refusal_for("python3"));
        let only = format!("{:#}", no_builder(&base, &["setup".to_string()]));
        assert!(only.contains("gather_facts: false"), "{only}");
        assert!(only.contains("VOLANT_PYTHON"), "{only}");
        let also = format!(
            "{:#}",
            no_builder(&base, &["lineinfile".to_string(), "setup".to_string()])
        );
        assert!(!also.contains("gather_facts"), "{also}");
        assert!(also.contains("ansible-core"), "{also}");
    }
}

/// An explicit interpreter is never fallen back from; the probe walks the order and refuses when nothing is left.
#[test]
fn an_explicit_interpreter_stands_alone_and_a_bare_one_is_probed() {
    {
        assert_eq!(
            candidates(Some("/nowhere/python"), Some("/venv")),
            vec!["/nowhere/python".to_string()]
        );
        let err = start_from(Some("/nowhere/python"), None, true)
            .map(|_| ())
            .expect_err("an interpreter that does not exist cannot build a payload")
            .to_string();
        assert!(err.contains("/nowhere/python"), "{err}");
        assert!(err.contains("VOLANT_PYTHON"), "{err}");
    }
    {
        let err = start_from(Some("/nowhere/python"), Some("/venv"), true)
            .map(|_| ())
            .expect_err("no candidate here has ansible-core")
            .to_string();
        assert!(
            err.contains("no Python interpreter with ansible-core"),
            "{err}"
        );
        assert!(
            !err.contains("/venv"),
            "an explicit interpreter stands alone: {err}"
        );
    }
}

/// An answer the helper cannot stand behind fails the run on the controller, naming what broke.
#[rstest]
#[case::dies(&["ping"], None, &["exited before answering"])]
#[case::cannot_build(
    &["nosuch"],
    Some(r#"{"error": "RuntimeError: ansible-core has no module 'nosuch'"}"#),
    &["nosuch"],
)]
#[case::leaves_a_module_out(
    &["ping", "stat", "file", "copy", "setup"],
    Some(r#"{"zip_b64": "UEsD", "modules": {}}"#),
    &["ping"],
)]
#[case::empty_blob(&[], Some(r#"{"zip_b64": "", "modules": {}}"#), &["empty"])]
#[case::not_base64(&[], Some(r#"{"zip_b64": "UEsD!BA==", "modules": {}}"#), &["base64"])]
#[case::missing_extensions(
    &["ping"],
    Some(r#"{"zip_b64": "UEsD", "modules": {"ping": {"module_fqn": "ansible.modules.ping", "profile": "legacy", "rlimit_nofile": 0}}}"#),
    &["extensions", "ping"],
)]
#[case::missing_module_fqn(
    &["ping"],
    Some(r#"{"zip_b64": "UEsD", "modules": {"ping": {"profile": "legacy"}}}"#),
    &["module_fqn", "ping"],
)]
fn a_helper_answer_that_is_not_whole_is_refused(
    #[case] asked: &[&str],
    #[case] answer: Option<&str>,
    #[case] named: &[&str],
) {
    let asked: Vec<String> = asked.iter().map(ToString::to_string).collect();
    let reply = answer.map_or_else(|| Cursor::new(Vec::new()), |a| framed(a.as_bytes()));
    let err = exchange(Vec::new(), reply, &asked).unwrap_err().to_string();
    for word in named {
        assert!(err.contains(word), "{word}: {err}");
    }
}

/// The request names the modules asked for, and the facts come back keyed by module.
#[test]
fn a_union_carries_the_blob_and_one_set_of_facts_per_module() {
    let answer = br#"{"zip_b64": "UEsD", "modules": {"ping": {"module_fqn":
            "ansible.modules.ping", "profile": "legacy", "rlimit_nofile": 0,
            "extensions": {}}}}"#;
    let mut sent = Vec::new();
    let union = exchange(&mut sent, framed(answer), &["ping".to_string()])
        .unwrap()
        .0;
    assert!(
        String::from_utf8_lossy(&sent).contains(r#"{"modules":["ping"]}"#),
        "{sent:?}"
    );
    assert_eq!(union.zip_b64, "UEsD");
    assert_eq!(
        union.modules["ping"],
        ModuleFacts {
            module_fqn: "ansible.modules.ping".to_string(),
            profile: "legacy".to_string(),
            rlimit_nofile: 0,
            extensions: Map::new(),
            core: false,
        }
    );
}

/// The facts are keyed by the short module name for a bare or `ansible.legacy.` spelling, so
/// one module asked for under both is one entry; `ansible.builtin.` keeps its own entry.
#[test]
fn the_facts_are_keyed_by_the_short_module_name() {
    let answer = br#"{"zip_b64": "UEsD", "modules": {"ansible.legacy.ping": {"module_fqn":
            "ansible.legacy.ping", "profile": "legacy", "rlimit_nofile": 0,
            "extensions": {}}, "ansible.builtin.ping": {"module_fqn": "ansible.modules.ping",
            "profile": "legacy", "rlimit_nofile": 0, "extensions": {}}}}"#;
    let asked = ["ansible.legacy.ping", "ansible.builtin.ping"].map(str::to_string);
    let union = exchange(Vec::new(), framed(answer), &asked).unwrap().0;
    assert_eq!(
        union.modules.keys().collect::<Vec<_>>(),
        ["ansible.builtin.ping", "ping"]
    );
}

/// Which modules go through the payload path: what ansible-core ships, minus what this release
/// runs itself, minus what the reference runs through an action plugin.
#[test]
fn the_payload_path_takes_what_nothing_else_runs() {
    for through in ["lineinfile", "stat", "ping", "apt", "ansible.builtin.file"] {
        assert!(is_python_module(through), "{through}");
    }
    for native in [
        "command",
        "shell",
        "raw",
        "debug",
        "set_fact",
        "include_vars",
    ] {
        assert!(!is_python_module(native), "{native} runs here already");
    }
    for plugin in ["package", "service", "template", "copy", "assert"] {
        assert!(
            !is_python_module(plugin),
            "{plugin} needs its action plugin"
        );
    }
    assert!(
        is_python_module("community.general.lineinfile"),
        "a collection's module goes to the controller's ansible-core to resolve"
    );
    assert!(
        !is_python_module("ansible.builtin.nosuch"),
        "no collection can supply a name ansible-core owns"
    );
}

/// A run naming a plugin builds every module the plugin may pick, and never the plugin's own
/// name.
#[test]
fn a_plugin_brings_its_backends_into_the_union_and_not_itself() {
    let built = modules_to_build(["ansible.builtin.package", "service", "ping", "command"]);
    let built: Vec<&str> = built.iter().map(String::as_str).collect();
    assert_eq!(
        built,
        [
            "apt",
            "dnf",
            "dnf5",
            "ping",
            "service",
            "setup",
            "systemd",
            "systemd_service",
            "sysvinit"
        ]
    );
    assert!(!built.contains(&"package"), "{built:?}");
}

/// The union a run builds, for the first play of `text` written at `dir`.
fn union_of(dir: &std::path::Path, text: &str) -> anyhow::Result<Vec<String>> {
    let pb = crate::playbook::parse(text, dir.join("play.yml").to_str().unwrap())?;
    let search = crate::roles::RoleSearch {
        paths: vec![dir.join("roles")],
        collections: Vec::new(),
    };
    let selection = crate::compile::TagSelection::new(Vec::new(), Vec::new());
    let play = crate::compile::compile(&pb.plays[0], &search, &selection)?;
    Ok(modules_for_run(&Reach::walk(&[(dir, &play)])?)
        .into_iter()
        .collect())
}

/// A module written only in a file a dynamic include reads goes into the union, and so does one
/// in a role file nothing includes by a name the compiler can read.
#[test]
fn the_union_holds_what_a_dynamic_include_may_read() {
    let dir_guard = scratch();
    let dir = dir_guard.path().to_path_buf();
    let tasks = dir.join("roles/r/tasks");
    std::fs::create_dir_all(tasks.join("sub")).unwrap();
    std::fs::create_dir_all(dir.join("roles/r/handlers")).unwrap();
    let write = |path: &str, text: &str| std::fs::write(dir.join(path), text).unwrap();
    write(
        "roles/r/tasks/main.yml",
        "- include_tasks: \"setup-{{ ansible_os_family }}.yml\"\n",
    );
    write(
        "roles/r/tasks/setup-Debian.yml",
        "- block:\n    - apt: name=x\n",
    );
    write(
        "roles/r/tasks/setup-Suse.yml",
        // Written for other platforms, and read here only for the union: a free-form Windows
        // command and a `key=value` string with a spaced template in it are parsed, not
        // refused, whatever this release would make of them if a host reached them.
        "- community.general.zypper: name=x\n- ansible.windows.win_shell: Get-Service foo\n- ns.coll.mod: port={{ x }}\n",
    );
    write(
        "roles/r/tasks/sub/deep.yaml",
        "- lineinfile: path=/x line=y\n",
    );
    write(
        "roles/r/handlers/main.yml",
        "- name: h\n  systemd: name=x\n",
    );
    write("inc.yml", "- stat: path=/\n- include_tasks: inner.yml\n");
    write("inner.yml", "- ping:\n- include_role: {name: dyn}\n");
    // A role reached only through that `include_role`, and its dependency.
    for role in ["dyn/tasks", "dyn/meta", "dep/tasks"] {
        std::fs::create_dir_all(dir.join("roles").join(role)).unwrap();
    }
    write("roles/dyn/tasks/main.yml", "- debug: msg=x\n");
    write("roles/dyn/meta/main.yml", "dependencies: [dep]\n");
    write("roles/dep/tasks/main.yml", "- apt_repository: repo=x\n");
    let union = union_of(
        &dir,
        "- hosts: all\n  gather_facts: false\n  roles: [r]\n  tasks:\n    - include_tasks: inc.yml\n",
    )
    .unwrap();
    assert_eq!(
        union,
        [
            "ansible.windows.win_shell",
            "apt",
            "apt_repository",
            "community.general.zypper",
            "lineinfile",
            "ns.coll.mod",
            "ping",
            "stat",
            "systemd"
        ]
    );

    // A role file that is not YAML is a broken role, refused by its own name.
    write("roles/r/tasks/broken.yml", "- apt: [\n");
    let err = union_of(&dir, "- hosts: all\n  roles: [r]\n").unwrap_err();
    assert!(format!("{err:#}").contains("broken.yml"), "{err:#}");
}

/// A run that names no Python module builds nothing, so a controller without ansible-core runs
/// native playbooks exactly as before.
#[test]
fn a_run_with_no_python_module_builds_nothing() {
    let none = union_for(&std::collections::BTreeSet::new(), &[], &[], &mut |_| {})
        .expect("nothing to build");
    assert!(none.is_none());
}

/// The blob is named by the hash of the zip's own bytes, which is the name the agent computes
/// for the same blob when it decides whether it already holds it.
#[test]
fn the_blob_is_named_by_the_hash_of_the_zip_not_of_its_base64() {
    let answer = br#"{"zip_b64": "UEsDBA==", "modules": {"ping": {"module_fqn":
            "ansible.modules.ping", "profile": "legacy", "rlimit_nofile": 0,
            "extensions": {}}}}"#;
    let union = exchange(Vec::new(), framed(answer), &["ping".to_string()])
        .unwrap()
        .0;
    assert_eq!(union.hash, blake3::hash(b"PK\x03\x04").to_hex().to_string());
    assert_ne!(union.hash, blake3::hash(b"UEsDBA==").to_hex().to_string());
    // The shape the agent refuses a name by before it will look for a file under it.
    assert_eq!(union.hash.len(), 64, "{}", union.hash);
    assert!(
        union
            .hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "{}",
        union.hash
    );
}

/// The helper's own self-check runs under any `python3`: the union merge refuses a byte
/// conflict and the wrapper reads its fields.
#[test]
fn the_helper_refuses_a_byte_conflict_in_the_union() {
    let out = Command::new("python3")
        .args(["-c", HELPER, "--self-check"])
        .output()
        .expect("python3 is on PATH wherever these tests run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A `print()` anywhere in the helper lands on stderr, and stdout carries the frame alone.
#[test]
fn a_print_in_the_helper_never_reaches_the_frames() {
    use std::io::Write as _;
    const DRIVER: &str = "import sys\nns = {'__name__': 'helper'}\nexec(sys.argv[1], ns)\nns['union'] = lambda modules: (print('noise from a plugin loader'), {'zip_b64': ''})[1]\nns['main']()\n";
    let mut child = Command::new("python3")
        .args(["-c", DRIVER, HELPER])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("python3 is on PATH wherever these tests run");
    let mut request = Vec::new();
    write_frame(&mut request, br#"{"modules": ["ping"]}"#).unwrap();
    child.stdin.take().unwrap().write_all(&request).unwrap();
    let out = child.wait_with_output().unwrap();
    let mut stdout = Cursor::new(out.stdout);
    let frame = read_frame(&mut stdout)
        .expect("stdout starts with a frame")
        .expect("one frame");
    assert_eq!(frame, br#"{"zip_b64": ""}"#);
    assert_eq!(
        stdout.position() as usize,
        stdout.get_ref().len(),
        "nothing but the frame on stdout"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("noise from a plugin loader"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A request and its answer over the helper's own pipes, which is the only place the two sides
/// of the frame header meet.
#[test]
fn the_helper_answers_one_frame_over_its_own_pipes() {
    let mut builder = PythonBuilder::under("python3").expect("python3 is on PATH");
    match builder.union(&["ping".to_string()]) {
        Ok((union, _)) => assert!(!union.zip_b64.is_empty(), "an empty blob was accepted"),
        Err(refused) => assert!(
            refused.to_string().contains("could not build the modules"),
            "{refused}"
        ),
    }
}

/// A helper that answers every request with its first argument, whatever it asks.
const CANNED: &str = "import struct, sys\nwhile True:\n    head = sys.stdin.buffer.read(4)\n    if len(head) < 4:\n        break\n    sys.stdin.buffer.read(struct.unpack('>I', head)[0])\n    answer = sys.argv[1].encode()\n    sys.stdout.buffer.write(struct.pack('>I', len(answer)) + answer)\n    sys.stdout.buffer.flush()\n";

/// A place in `root`, for the `python3` on `PATH`, which is what the canned helper runs under.
fn fresh_place(root: &std::path::Path) -> crate::union_cache::Place {
    let interpreter =
        crate::union_cache::interpreter_without_running(Some("python3"), None).unwrap();
    crate::union_cache::Place {
        dir: root.join("unions"),
        key: crate::union_cache::key(
            &interpreter,
            &std::collections::BTreeSet::from(["ping".into()]),
            &[],
            &[],
        )
        .expect("no plugin file to vouch for"),
        interpreter: interpreter.path,
        real: interpreter.real,
    }
}

/// Where the `python3` on `PATH` leads, which is what a helper started under it reports.
fn python3_real() -> std::path::PathBuf {
    crate::union_cache::interpreter_without_running(Some("python3"), None)
        .unwrap()
        .real
}

/// The answer of a helper that built `ping` into `zip` from the file `source`.
fn canned(zip: &[u8], source: Option<&std::path::Path>) -> String {
    canned_under(zip, source, &python3_real())
}

/// [`canned`] from a helper reporting `interpreter` as the one it ran under.
fn canned_under(
    zip: &[u8],
    source: Option<&std::path::Path>,
    interpreter: &std::path::Path,
) -> String {
    let sources = source.map_or(Value::Null, |path| {
        let now = crate::union_cache::Source::now(path).unwrap();
        serde_json::json!([{
            "path": path.to_str().unwrap(),
            "len": now.len,
            "mtime_ns": i64::try_from(now.mtime_ns).unwrap(),
        }])
    });
    serde_json::json!({
        "zip_b64": volant_protocol::encoding::b64_encode(zip),
        "modules": {"ping": {"module_fqn": "ansible.modules.ping", "profile": "legacy",
            "rlimit_nofile": 0, "extensions": {}}},
        "sources": sources,
        "interpreter": interpreter.to_str().unwrap(),
    })
    .to_string()
}

/// A valid entry starts no helper and a stale one is rebuilt once; a union without its sources is not kept.
#[test]
fn a_valid_cache_starts_nothing_and_a_union_without_sources_is_not_kept() {
    {
        let root_guard = scratch();
        let root = root_guard.path().to_path_buf();
        let source = root.join("ping.py");
        std::fs::write(&source, "ping").unwrap();
        let place = fresh_place(&root);
        let modules = std::collections::BTreeSet::from(["ping".to_string()]);
        let starts = std::cell::Cell::new(0);
        let run = |answer: &str| {
            let start = || {
                starts.set(starts.get() + 1);
                PythonBuilder::spawn("python3", &["-c", CANNED, answer], &[])
            };
            union_from(start, &modules, &[], Some(&place), &mut |w| panic!("{w}")).unwrap()
        };

        let first = run(&canned(b"PK first", Some(&source))).unwrap();
        assert_eq!(starts.get(), 1);
        let second = run(&canned(b"PK other", Some(&source))).unwrap();
        assert_eq!(starts.get(), 1, "a valid entry started the helper");
        assert_eq!(second, first);

        std::fs::write(&source, "pong").unwrap();
        let file = std::fs::File::options().write(true).open(&source).unwrap();
        let mtime = file.metadata().unwrap().modified().unwrap();
        file.set_modified(mtime + std::time::Duration::from_secs(1))
            .unwrap();
        let third = run(&canned(b"PK third", Some(&source))).unwrap();
        assert_eq!(starts.get(), 2);
        assert_eq!(third.hash, blake3::hash(b"PK third").to_hex().to_string());
        let kept = crate::union_cache::load(&place.dir, &place.key).expect("written again");
        assert_eq!(kept.union, third);
    }
    {
        let root_guard = scratch();
        let root = root_guard.path().to_path_buf();
        let place = fresh_place(&root);
        let modules = std::collections::BTreeSet::from(["ping".to_string()]);
        let answer = canned(b"PK untraced", None);
        let start = || PythonBuilder::spawn("python3", &["-c", CANNED, &answer], &[]);
        union_from(start, &modules, &[], Some(&place), &mut |w| panic!("{w}")).unwrap();
        assert_eq!(crate::union_cache::load(&place.dir, &place.key), None);
        assert!(
            !std::fs::read_dir(&place.dir).is_ok_and(|mut files| files.next().is_some()),
            "an entry was written"
        );
    }
}

/// An unwritable cache and a file others could write are warned about and the run goes on.
#[test]
fn a_cache_that_cannot_be_trusted_warns_and_the_run_goes_on() {
    {
        let root_guard = scratch();
        let root = root_guard.path().to_path_buf();
        let source = root.join("ping.py");
        std::fs::write(&source, "ping").unwrap();
        std::fs::write(root.join("unions"), "a file where the directory goes").unwrap();
        let place = fresh_place(&root);
        let modules = std::collections::BTreeSet::from(["ping".to_string()]);
        let answer = canned(b"PK", Some(&source));
        let start = || PythonBuilder::spawn("python3", &["-c", CANNED, &answer], &[]);
        let mut warnings = Vec::new();
        let union = union_from(start, &modules, &[], Some(&place), &mut |w| {
            warnings.push(w);
        });
        assert!(union.unwrap().is_some());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].starts_with(&format!(
                "the module cache at {} is not usable: ",
                place.dir.display()
            )) && warnings[0].ends_with("; payloads are rebuilt on every run"),
            "{}",
            warnings[0]
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let root_guard = scratch();
        let root = root_guard.path().to_path_buf();
        let source = root.join("ping.py");
        std::fs::write(&source, "ping").unwrap();
        let place = fresh_place(&root);
        let modules = std::collections::BTreeSet::from(["ping".to_string()]);
        let answer = canned(b"PK kept", Some(&source));
        let start = || PythonBuilder::spawn("python3", &["-c", CANNED, &answer], &[]);
        union_from(start, &modules, &[], Some(&place), &mut |w| panic!("{w}")).unwrap();
        for file in std::fs::read_dir(&place.dir).unwrap() {
            let path = file.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "json") {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o620)).unwrap();
            }
        }
        let starts = std::cell::Cell::new(0);
        let start = || {
            starts.set(starts.get() + 1);
            PythonBuilder::spawn("python3", &["-c", CANNED, &answer], &[])
        };
        let mut warnings = Vec::new();
        union_from(start, &modules, &[], Some(&place), &mut |w| {
            warnings.push(w);
        })
        .unwrap();
        assert_eq!(starts.get(), 1, "a file others could write was read");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("writable by nobody else"),
            "{}",
            warnings[0]
        );
    }
}

/// A kept answer says a name is an action plugin, as a fresh one would.
#[test]
fn a_cache_hit_still_knows_a_collection_action_plugin() {
    let root_guard = scratch();
    let root = root_guard.path().to_path_buf();
    let source = root.join("ping.py");
    std::fs::write(&source, "ping").unwrap();
    let place = fresh_place(&root);
    let entry = crate::union_cache::Entry {
        union: Union {
            hash: blake3::hash(b"PK").to_hex().to_string(),
            zip_b64: volant_protocol::encoding::b64_encode(b"PK"),
            modules: BTreeMap::new(),
            refused: BTreeMap::new(),
            natives: Natives::default(),
        },
        sources: vec![crate::union_cache::Source::now(&source).unwrap()],
        resolved: BTreeMap::from([(
            "ns.c.sync".to_string(),
            Resolved::ActionPlugin {
                fqcn: "ns.c.sync".into(),
                with_module: false,
            },
        )]),
    };
    crate::union_cache::store(&place.dir, &place.key, &entry).unwrap();
    let modules = std::collections::BTreeSet::from(["ping".to_string()]);
    let unstarted = || -> anyhow::Result<PythonBuilder> { panic!("the helper was started") };
    let named = [("Sync".to_string(), "ns.c.sync".to_string())];
    union_from(unstarted, &modules, &named, Some(&place), &mut |_| {}).unwrap();
    assert!(crate::action_plugins::kind("ns.c.sync").is_some());
}

/// An entry is filed under the interpreter that built it and the executable that answered.
#[test]
fn an_entry_is_filed_only_under_the_interpreter_and_executable_that_built_it() {
    {
        let root_guard = scratch();
        let root = root_guard.path().to_path_buf();
        let source = root.join("ping.py");
        std::fs::write(&source, "ping").unwrap();
        let place = crate::union_cache::Place {
            interpreter: root.join("venv/bin/python"),
            ..fresh_place(&root)
        };
        let modules = std::collections::BTreeSet::from(["ping".to_string()]);
        let answer = canned(b"PK", Some(&source));
        let start = || PythonBuilder::spawn("python3", &["-c", CANNED, &answer], &[]);
        union_from(start, &modules, &[], Some(&place), &mut |w| panic!("{w}")).unwrap();
        assert_eq!(crate::union_cache::load(&place.dir, &place.key), None);
    }
    {
        let root_guard = scratch();
        let root = root_guard.path().to_path_buf();
        let source = root.join("ping.py");
        std::fs::write(&source, "ping").unwrap();
        let place = fresh_place(&root);
        let modules = std::collections::BTreeSet::from(["ping".to_string()]);
        let answer = canned_under(b"PK", Some(&source), &root.join("python3.12"));
        let start = || PythonBuilder::spawn("python3", &["-c", CANNED, &answer], &[]);
        union_from(start, &modules, &[], Some(&place), &mut |w| panic!("{w}")).unwrap();
        assert_eq!(crate::union_cache::load(&place.dir, &place.key), None);
    }
}

/// The helper names the files a union came from: each module and `module_utils` it copied,
/// ansible-core's own version, and the manifests of each collection it read.
#[test]
fn the_helper_names_the_files_a_union_came_from() {
    let Some(mut helper) = helper_or_skip() else {
        return;
    };
    let names = ["ansible.posix.sysctl".to_string(), "ping".to_string()];
    let (_, traced) = helper.union(&names).unwrap();
    let traced = traced.expect("every entry is named");
    let python = std::env::var("VOLANT_PYTHON").unwrap();
    assert_eq!(traced.interpreter, std::fs::canonicalize(python).unwrap());
    let sources = traced.sources;
    let paths: Vec<String> = sources
        .iter()
        .map(|s| s.path.display().to_string())
        .collect();
    for end in [
        "/ansible/release.py",
        "/ansible/executor/module_common.py",
        "/ansible/modules/ping.py",
        "/ansible/module_utils/basic.py",
        "/ansible_collections/ansible/posix/plugins/modules/sysctl.py",
        "/ansible_collections/ansible/posix/MANIFEST.json",
        "/ansible_collections/ansible/posix/FILES.json",
    ] {
        assert!(paths.iter().any(|p| p.ends_with(end)), "{end}: {paths:#?}");
    }
    // Where a collection could still appear: the interpreter's own site-packages, which holds
    // no `ansible_collections` on the machine this was measured on, is itself recorded.
    assert!(
        paths.iter().any(|p| p.ends_with("/site-packages")),
        "{paths:#?}"
    );
    // Where a module could be dropped to shadow a builtin: `~/.ansible/plugins/modules`, or
    // the nearest directory above it that exists.
    let home = std::env::var("HOME").unwrap();
    let shadow = std::path::Path::new(&home).join(".ansible/plugins/modules");
    assert!(
        sources
            .iter()
            .any(|s| shadow.starts_with(&s.path) && s.path.starts_with(&home)),
        "{shadow:?}: {paths:#?}"
    );
    for source in &sources {
        assert_eq!(
            crate::union_cache::Source::now(&source.path).unwrap(),
            *source
        );
    }
}

fn framed(payload: &[u8]) -> Cursor<Vec<u8>> {
    let mut buf = Vec::new();
    write_frame(&mut buf, payload).unwrap();
    Cursor::new(buf)
}

/// A run that only gathers facts never installs Volant's runtime; any other module may.
#[test]
fn only_a_facts_only_run_never_installs() {
    let set = |names: &[&str]| names.iter().map(ToString::to_string).collect();
    assert!(gathering_only(&set(&["setup"])));
    assert!(!gathering_only(&set(&["setup", "lineinfile"])));
    assert!(!gathering_only(&set(&["lineinfile"])));
}
