// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use crate::testing::{args, done, scratch};
use std::sync::atomic::Ordering;

use std::path::{Path, PathBuf};

use rstest::rstest;
use serde_json::json;

use crate::modules::{Context, Run};

/// The module_utils come from the blob, not from the interpreter. This is the proof: the dev
/// host's `/usr/bin/python3` has no ansible-core at all, and the module still runs.
#[test]
fn the_module_utils_come_from_the_blob_and_not_from_the_interpreter() {
    assert_ne!(
        Command::new(HOST_PYTHON)
            .args(["-c", "import ansible"])
            .status()
            .expect("the dev host has a python3")
            .code(),
        Some(0),
        "this test is only meaningful on an interpreter without ansible-core"
    );
    let blob = stub_blob(
        r#"
    print(json.dumps({"probe": "ok", "got": args, "fqn": module_fqn, "profile": profile}))
"#,
    );
    let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
    let run = server.run(
        &payload("ansible.modules.probe"),
        &args(json!({"x": 1})),
        &Context::default(),
        &|| false,
    );
    let Run::Done(result) = run else {
        panic!("not cancelled")
    };
    assert_eq!(result.0["probe"], "ok", "{:?}", result.0);
    assert_eq!(result.0["got"]["x"], 1);
    assert_eq!(result.0["fqn"], "ansible.modules.probe");
    assert_eq!(result.0["profile"], "legacy");
    assert!(!result.failed());
}

/// One module does not reach the next, and the task environment reaches the module and stops there.
#[test]
fn a_task_environment_stops_at_its_own_module() {
    {
        let blob = stub_blob(
            r#"
    import os
    marker = os.environ.get("VOLANT_PROBE_MARKER", "clean")
    os.environ["VOLANT_PROBE_MARKER"] = "dirty"
    os.chdir("/")
    print(json.dumps({"marker": marker, "cwd": os.getcwd()}))
"#,
        );
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let first = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        let second = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert_eq!(first.0["marker"], "clean");
        assert_eq!(
            second.0["marker"], "clean",
            "the second module saw what the first one did to its own process"
        );
        assert!(server.alive(), "the server outlives a task");
    }
    {
        let blob = stub_blob(
            r#"
    import os
    print(json.dumps({"seen": os.environ.get("VOLANT_TASK_VAR", "unset")}))
"#,
        );
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let mut context = Context::default();
        context
            .environment
            .insert("VOLANT_TASK_VAR".into(), "here".into());
        let with = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &context,
            &|| false,
        ));
        let without = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert_eq!(with.0["seen"], "here");
        assert_eq!(without.0["seen"], "unset", "it leaked into the next task");
    }
}

/// A module's imports are done in the server once its first child runs, so its later children
/// find them done, and the first child still imports them itself.
#[test]
fn a_module_s_imports_are_done_in_the_server_once_its_first_child_runs() {
    let blob = runpy_blob(&[]);
    let logs = scratch();
    let log = logs.path().join("imports.log");
    let log_literal = format!("{:?}", log.to_str().unwrap());
    std::fs::write(
        blob.path().join("ansible/module_utils/counted.py"),
        format!(
            "import os\nwith open({log_literal}, \"a\") as log:\n    log.write(\"%d\\n\" % os.getpid())\n"
        ),
    )
    .unwrap();
    std::fs::write(
        blob.path().join("ansible/modules/probe.py"),
        "import json, os\ntry:\n    import volant_no_such_module\nexcept ImportError:\n    pass\nfrom ansible.module_utils import counted\nprint(json.dumps({\"pid\": os.getpid()}))\n",
    )
    .unwrap();
    let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
    let pids: Vec<u64> = (0..3)
        .map(|_| {
            let result = done(server.run(
                &payload("ansible.modules.probe"),
                &args(json!({})),
                &Context::default(),
                &|| false,
            ));
            assert!(!result.failed(), "{:?}", result.0);
            result.0["pid"].as_u64().unwrap()
        })
        .collect();
    let mut imported: Vec<u64> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|pid| pid.parse().unwrap())
        .collect();
    imported.sort_unstable();
    let mut expected = vec![pids[0], u64::from(server.child.id())];
    expected.sort_unstable();
    assert_eq!(
        imported, expected,
        "the dependency should be imported by the first child and by the server, and by no later child (children: {pids:?})"
    );
}

/// A module's later children take its spec from the server instead of asking the path again.
#[test]
fn a_module_s_later_children_take_its_spec_from_the_server() {
    let blob = runpy_blob(&[(
        "ansible/modules/probe.py",
        "import json, os, sys\nprint(json.dumps({\"pid\": os.getpid(), \"finders\": [type(f).__name__ for f in sys.meta_path]}))\n",
    )]);
    let site = scratch();
    // Outside the path: a file written into a directory on `sys.path` is a package moving
    // under the server, which replaces it.
    let logs = scratch();
    let log = logs.path().join("lookups");
    let log_literal = format!("{:?}", log.to_str().unwrap());
    std::fs::write(
        site.path().join("sitecustomize.py"),
        format!(
            "import os, sys\n\n\nclass Spy:\n    def find_spec(self, name, path=None, target=None):\n        if name == \"ansible.modules.probe\":\n            with open({log_literal}, \"a\") as log:\n                log.write(\"%d\\n\" % os.getpid())\n        return None\n\n\nsys.meta_path.insert(0, Spy())\n"
        ),
    )
    .unwrap();
    let environment = BTreeMap::from([(
        "PYTHONPATH".to_string(),
        site.path().to_str().unwrap().to_string(),
    )]);
    let mut server = Server::start_in(HOST_PYTHON, blob.path(), &environment, &|| false).unwrap();
    let results: Vec<TaskResult> = (0..3)
        .map(|_| call(&mut server, "ansible.modules.probe"))
        .collect();
    for result in &results {
        assert!(!result.failed(), "{:?}", result.0);
    }
    let mut looked: Vec<u64> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|pid| pid.parse().unwrap())
        .collect();
    looked.sort_unstable();
    let mut expected = vec![
        results[0].0["pid"].as_u64().unwrap(),
        u64::from(server.child.id()),
    ];
    expected.sort_unstable();
    assert_eq!(
        looked, expected,
        "only the first child and the server should ask the path for the module"
    );
    assert_eq!(results[2].0["finders"][0], "Spy", "{:?}", results[2].0);
}

/// Each environment gets a server started with it, so imports ahead run under the task's environment.
#[test]
#[cfg(target_os = "linux")]
fn each_environment_gets_a_server_started_with_it() {
    let root = scratch();
    let tmp = root.path().to_str().unwrap().to_string();
    let payload = cached_payload(
        root.path(),
        "\n    import os\n    with open(\"/proc/%d/environ\" % os.getppid(), \"rb\") as fh:\n        seen = [v for v in fh.read().decode().split(\"\\0\") if v.startswith(\"VOLANT_ENV_PROBE=\")]\n    print(json.dumps({\"parent\": os.getppid(), \"seen\": seen}))\n",
    );
    batch_started();
    let task = |value: Option<&str>| {
        let mut context = Context {
            remote_tmp: tmp.clone(),
            ..Context::default()
        };
        if let Some(value) = value {
            context
                .environment
                .insert("VOLANT_ENV_PROBE".into(), value.into());
        }
        let result = done(run(&payload, &args(json!({})), &context, &|| false).0);
        assert!(!result.failed(), "{:?}", result.0);
        result
    };
    let none = task(None);
    let a = task(Some("a"));
    let a_again = task(Some("a"));
    let b = task(Some("b"));
    let none_again = task(None);
    assert_eq!(none.0["seen"], json!([]));
    assert_eq!(a.0["seen"], json!(["VOLANT_ENV_PROBE=a"]));
    assert_eq!(b.0["seen"], json!(["VOLANT_ENV_PROBE=b"]));
    assert_eq!(none_again.0["seen"], json!([]));
    assert_eq!(
        a_again.0["parent"], a.0["parent"],
        "a second server for `a`"
    );
    assert_eq!(none_again.0["parent"], none.0["parent"]);
    assert!(
        !Path::new(&format!("/proc/{}", a.0["parent"])).exists(),
        "the server for `a` outlived the one for `b`"
    );
}

/// A package outside the payload, served by another finder or setting itself up as it loads, is
/// imported by each child and stays cold.
#[test]
fn a_package_outside_the_payload_is_imported_by_each_child_whoever_serves_it() {
    {
        let blob = runpy_blob(&[
            (
                "ansible/module_utils/uses_third.py",
                "import volant_third_party\nVALUE = volant_third_party.VALUE\n",
            ),
            (
                "ansible/module_utils/maybe_third.py",
                "try:\n    import volant_third_party\n    VALUE = volant_third_party.VALUE\nexcept ImportError:\n    VALUE = None\n",
            ),
            (
                "ansible/modules/probe.py",
                "import json, os\nfrom ansible.module_utils.uses_third import VALUE\nprint(json.dumps({\"value\": VALUE, \"parent\": os.getppid()}))\n",
            ),
            (
                "ansible/modules/probe_from.py",
                "import json, os\nfrom ansible.module_utils import maybe_third\nprint(json.dumps({\"value\": maybe_third.VALUE, \"parent\": os.getppid()}))\n",
            ),
        ]);
        let site = scratch();
        let data = scratch();
        let state = data.path().join("state");
        std::fs::write(&state, "one").unwrap();
        let state_literal = format!("{:?}", state.to_str().unwrap());
        std::fs::write(
            site.path().join("volant_third_party.py"),
            format!("with open({state_literal}) as fh:\n    VALUE = fh.read()\n"),
        )
        .unwrap();
        // No bytecode written into the site directory: that would move it, and the server would
        // be replaced for that reason instead.
        let environment = BTreeMap::from([
            (
                "PYTHONPATH".to_string(),
                site.path().to_str().unwrap().to_string(),
            ),
            ("PYTHONDONTWRITEBYTECODE".to_string(), "1".to_string()),
        ]);
        let mut server =
            Server::start_in(HOST_PYTHON, blob.path(), &environment, &|| false).unwrap();
        // Two tasks before the change: the server answers the second only once it has done its
        // imports for the first, so nothing it does races the change.
        for _ in 0..2 {
            for fqn in ["ansible.modules.probe", "ansible.modules.probe_from"] {
                let before = call(&mut server, fqn);
                assert_eq!(before.0["value"], "one", "{fqn}: {:?}", before.0);
            }
        }
        std::fs::write(&state, "two").unwrap();
        for fqn in ["ansible.modules.probe", "ansible.modules.probe_from"] {
            let after = call(&mut server, fqn);
            assert_eq!(after.0["value"], "two", "{fqn}: {:?}", after.0);
            assert_eq!(
                after.0["parent"],
                u64::from(server.child.id()),
                "the server was replaced"
            );
        }
    }
    {
        let blob = runpy_blob(&[
            (
                "ansible/module_utils/uses_elsewhere.py",
                "import volant_served_elsewhere\nVALUE = volant_served_elsewhere.VALUE\n",
            ),
            (
                "ansible/modules/probe.py",
                "import json\nfrom ansible.module_utils.uses_elsewhere import VALUE\nprint(json.dumps({\"value\": VALUE}))\n",
            ),
        ]);
        let site = scratch();
        let elsewhere = scratch();
        let state = elsewhere.path().join("state");
        std::fs::write(&state, "one").unwrap();
        let state_literal = format!("{:?}", state.to_str().unwrap());
        let source = elsewhere.path().join("served.py");
        std::fs::write(
            &source,
            format!("with open({state_literal}) as fh:\n    VALUE = fh.read()\n"),
        )
        .unwrap();
        let source_literal = format!("{:?}", source.to_str().unwrap());
        std::fs::write(
        site.path().join("sitecustomize.py"),
        format!(
            "import importlib.util, sys\n\n\nclass Elsewhere:\n    def find_spec(self, name, path=None, target=None):\n        if name == \"volant_served_elsewhere\":\n            return importlib.util.spec_from_file_location(name, {source_literal})\n        return None\n\n\nsys.meta_path.append(Elsewhere())\n"
        ),
    )
    .unwrap();
        let environment = BTreeMap::from([
            (
                "PYTHONPATH".to_string(),
                site.path().to_str().unwrap().to_string(),
            ),
            ("PYTHONDONTWRITEBYTECODE".to_string(), "1".to_string()),
        ]);
        let mut server =
            Server::start_in(HOST_PYTHON, blob.path(), &environment, &|| false).unwrap();
        for _ in 0..2 {
            let before = call(&mut server, "ansible.modules.probe");
            assert_eq!(before.0["value"], "one", "{:?}", before.0);
        }
        std::fs::write(&state, "two").unwrap();
        let after = call(&mut server, "ansible.modules.probe");
        assert_eq!(after.0["value"], "two", "{:?}", after.0);
    }
    {
        let vendor = scratch();
        std::fs::write(
            vendor.path().join("volant_vendored.py"),
            "VALUE = \"vendored\"\n",
        )
        .unwrap();
        let vendor_literal = format!("{:?}", vendor.path().to_str().unwrap());
        let blob = runpy_blob(&[
            (
                "ansible/module_utils/basic.py",
                "from ansible.module_utils.common import warnings as _warnings\n",
            ),
            ("ansible/module_utils/common/__init__.py", ""),
            (
                "ansible/module_utils/common/warnings.py",
                "_global_warnings = {}\n_global_deprecations = {}\n",
            ),
            (
                "ansible/module_utils/deprecating.py",
                "from ansible.module_utils.common import warnings\nwarnings._global_deprecations[\"old\"] = None\n",
            ),
            (
                "ansible/modules/noisy.py",
                "import json\nimport ansible.module_utils.deprecating\nprint(json.dumps({}))\n",
            ),
            (
                "ansible/modules/reader.py",
                "import json\nfrom ansible.module_utils.common import warnings\nprint(json.dumps({\"deprecations\": sorted(warnings._global_deprecations)}))\n",
            ),
            (
                "ansible/modules/vendoring.py",
                "import json\nimport ansible.module_utils.vendoring\n\n\ndef main():\n    import volant_vendored\n    print(json.dumps({\"value\": volant_vendored.VALUE}))\n\n\nmain()\n",
            ),
            (
                "ansible/modules/vendoring_from.py",
                "import json\nfrom ansible.module_utils import vendoring\n\n\ndef main():\n    import volant_vendored\n    print(json.dumps({\"value\": volant_vendored.VALUE}))\n\n\nmain()\n",
            ),
        ]);
        std::fs::write(
            blob.path().join("ansible/module_utils/vendoring.py"),
            format!("import sys\nsys.path.insert(0, {vendor_literal})\n"),
        )
        .unwrap();
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        for _ in 0..2 {
            for fqn in [
                "ansible.modules.vendoring",
                "ansible.modules.vendoring_from",
            ] {
                let vendored = call(&mut server, fqn);
                assert_eq!(vendored.0["value"], "vendored", "{fqn}: {:?}", vendored.0);
            }
            let noisy = call(&mut server, "ansible.modules.noisy");
            assert!(!noisy.failed(), "{:?}", noisy.0);
        }
        let reader = call(&mut server, "ansible.modules.reader");
        assert_eq!(reader.0["deprecations"], json!([]), "{:?}", reader.0);
    }
}

/// The server imports ahead only what the module always imports, only once the result is sent,
/// and only the payload and the standard library.
#[test]
fn the_server_imports_ahead_only_what_the_module_always_imports() {
    {
        let logs = scratch();
        let log = logs.path().join("imports");
        let log_literal = format!("{:?}", log.to_str().unwrap());
        let logged = format!(
            "import os\nwith open({log_literal}, \"a\") as log:\n    log.write(__name__ + \"\\n\")\n"
        );
        let blob = runpy_blob(&[
            ("ansible/module_utils/after_failure.py", logged.as_str()),
            ("ansible/module_utils/in_except.py", logged.as_str()),
            ("ansible/module_utils/in_if.py", logged.as_str()),
            (
                "ansible/modules/probe.py",
                "import json\ntry:\n    import volant_no_such_module\n    import ansible.module_utils.after_failure\nexcept ImportError:\n    pass\ntry:\n    import os\nexcept ImportError:\n    import ansible.module_utils.in_except\nif json is None:\n    import ansible.module_utils.in_if\nprint(json.dumps({}))\n",
            ),
        ]);
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        for _ in 0..2 {
            let result = call(&mut server, "ansible.modules.probe");
            assert!(!result.failed(), "{:?}", result.0);
        }
        assert_eq!(std::fs::read_to_string(&log).unwrap_or_default(), "");
    }
    {
        let gates = scratch();
        let gate = gates.path().join("open");
        let gate_literal = format!("{:?}", gate.to_str().unwrap());
        let blob = runpy_blob(&[(
            "ansible/modules/probe.py",
            "import json\nimport ansible.module_utils.gated\nprint(json.dumps({}))\n",
        )]);
        std::fs::write(
        blob.path().join("ansible/module_utils/gated.py"),
        format!(
            "import os, sys, time\nif getattr(sys.modules[\"__main__\"], \"__file__\", None) is None:\n    while not os.path.exists({gate_literal}):\n        time.sleep(0.01)\n"
        ),
    )
    .unwrap();
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let first = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context {
                timeout: Some(Duration::from_secs(5)),
                ..Context::default()
            },
            &|| false,
        ));
        std::fs::write(&gate, "").unwrap();
        assert!(!first.failed(), "{:?}", first.0);
        let second = call(&mut server, "ansible.modules.probe");
        assert!(!second.failed(), "{:?}", second.0);
    }
    {
        const CHECK: &str = r#"
import json, sys, sysconfig
ns = {"__name__": "server"}
exec(sys.argv[1], ns)
outside = ns["Outside"]("/payload")
stdlib = sysconfig.get_path("stdlib")
print(json.dumps([
    outside.allowed("/payload/ansible/module_utils/basic.py"),
    outside.allowed(stdlib + "/json/__init__.py"),
    outside.allowed(stdlib + "/dist-packages/apt/__init__.py"),
    outside.allowed(stdlib + "/site-packages/yaml/__init__.py"),
    outside.allowed("/usr/lib/python3/dist-packages/apt/__init__.py"),
    outside.allowed("/payloadx/ansible/__init__.py"),
]))
"#;
        let out = Command::new(HOST_PYTHON)
            .args(["-c", CHECK, SERVER])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let seen: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(seen, json!([true, true, false, false, false, false]));
    }
}

/// A loop alternating two environments in one batch runs every item.
#[test]
fn a_loop_alternating_two_environments_runs_every_item() {
    let root = scratch();
    let tmp = root.path().to_str().unwrap().to_string();
    let payload = cached_payload(root.path(), "\n    print(json.dumps({}))\n");
    batch_started();
    for proxy in ["p1", "p2", "p1", "p2", "p1"] {
        let mut context = Context {
            remote_tmp: tmp.clone(),
            ..Context::default()
        };
        context
            .environment
            .insert("https_proxy".into(), proxy.into());
        let result = done(run(&payload, &args(json!({})), &context, &|| false).0);
        assert!(!result.failed(), "{proxy}: {:?}", result.0);
    }
}

/// A server still importing ahead is given up on after a bounded wait (300 ms here): the task
/// runs under a fresh server, which is asked not to import that module ahead again.
#[test]
fn a_server_still_importing_ahead_is_given_up_on_not_waited_for() {
    let (blob, gate) = gated_blob();
    let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
    server.preload_wait = Duration::from_millis(300);
    let first = call(&mut server, "ansible.modules.probe");
    assert!(!first.failed(), "{:?}", first.0);
    let second = call(&mut server, "ansible.modules.probe");
    assert!(!second.failed(), "{:?}", second.0);
    assert_ne!(second.0["parent"], first.0["parent"]);
    let third = call(&mut server, "ansible.modules.probe");
    assert!(!third.failed(), "{:?}", third.0);
    assert_eq!(
        third.0["parent"], second.0["parent"],
        "the replacement imported the module ahead again"
    );
    std::fs::write(gate, "").unwrap();
}

/// A payload whose `probe` imports a dependency that, in the server alone, blocks until the
/// returned gate file exists. The module reports the server it ran under.
fn gated_blob() -> (tempfile::TempDir, PathBuf) {
    let blob = runpy_blob(&[(
        "ansible/modules/probe.py",
        "import json, os\nimport ansible.module_utils.gated\nprint(json.dumps({\"parent\": os.getppid()}))\n",
    )]);
    // In the payload itself, which the server does not watch for changes.
    let gate = blob.path().join("gate-open");
    let gate_literal = format!("{:?}", gate.to_str().unwrap());
    std::fs::write(
        blob.path().join("ansible/module_utils/gated.py"),
        format!(
            "import os, sys, time\nif getattr(sys.modules[\"__main__\"], \"__file__\", None) is None:\n    while not os.path.exists({gate_literal}):\n        time.sleep(0.01)\n"
        ),
    )
    .unwrap();
    (blob, gate)
}

/// A replacement that cannot start fails the task with its own words, not the old server's.
#[test]
#[cfg(target_os = "linux")]
fn a_replacement_that_cannot_start_speaks_for_itself() {
    let blob = runpy_blob(&[(
        "ansible/modules/probe.py",
        "import json, os\nwith open(\"/proc/%d/fd/2\" % os.getppid(), \"w\") as err:\n    err.write(\"old server words\\n\")\nprint(json.dumps({}))\n",
    )]);
    let site = scratch();
    let environment = BTreeMap::from([(
        "PYTHONPATH".to_string(),
        site.path().to_str().unwrap().to_string(),
    )]);
    let mut server = Server::start_in(HOST_PYTHON, blob.path(), &environment, &|| false).unwrap();
    // Two tasks before the change, so the server is done with its imports for the first.
    for _ in 0..2 {
        let before = call(&mut server, "ansible.modules.probe");
        assert!(!before.failed(), "{:?}", before.0);
    }
    std::fs::create_dir(site.path().join("volant_moved")).unwrap();
    std::fs::remove_dir_all(blob.path().join("ansible")).unwrap();
    let failed = call(&mut server, "ansible.modules.probe");
    let msg = failed.0["msg"].as_str().unwrap();
    assert!(msg.contains("could not start a python server"), "{msg}");
    assert!(!msg.contains("old server words"), "{msg}");
}

/// What the server imports for a module leaves the process as it found it.
#[test]
fn what_the_server_imports_for_a_module_leaves_it_as_it_was() {
    let blob = runpy_blob(&[
        (
            "ansible/modules/reader.py",
            "import json, os, sys\nmask = os.umask(0)\nprint(json.dumps({\"leak\": os.environ.get(\"VOLANT_LEAK\"), \"cwd\": os.getcwd(), \"umask\": mask, \"path\": \"/leaked\" in sys.path, \"parent\": os.getppid()}))\n",
        ),
        (
            "ansible/modules/leaky.py",
            "import json\nimport ansible.module_utils.leaky\nprint(json.dumps({}))\n",
        ),
    ]);
    let cwd_literal = format!("{:?}", blob.path().to_str().unwrap());
    std::fs::write(
        blob.path().join("ansible/module_utils/leaky.py"),
        format!(
            "import os, sys\nos.environ[\"VOLANT_LEAK\"] = \"leaked\"\nos.chdir({cwd_literal})\nos.umask(0o077)\nsys.path.append(\"/leaked\")\nsys.stdin.read()\nprint(\"noise on stdout\")\nsys.stderr.write(\"noise on stderr\\n\")\nsys.stdout = open(os.devnull, \"w\")\n"
        ),
    )
    .unwrap();
    let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
    let before = call(&mut server, "ansible.modules.reader");
    assert!(!before.failed(), "{:?}", before.0);
    call(&mut server, "ansible.modules.leaky");
    let after = call(&mut server, "ansible.modules.reader");
    assert!(!after.failed(), "{:?}", after.0);
    assert_eq!(
        after.0, before.0,
        "the server kept what an import changed, or was replaced over it"
    );

    server.child.kill().unwrap();
    server.child.wait().unwrap();
    let died = call(&mut server, "ansible.modules.reader");
    assert!(
        !died.0["msg"].as_str().unwrap().contains("noise on stderr"),
        "{:?}",
        died.0
    );
}

/// An import that fails in the server leaves it running and fails only the module's child.
#[test]
fn an_import_that_fails_in_the_server_fails_only_in_the_child() {
    let blob = runpy_blob(&[
        (
            "ansible/module_utils/broken.py",
            "raise SystemExit(\"broken on purpose\")\n",
        ),
        (
            "ansible/modules/broken.py",
            "import ansible.module_utils.broken\n",
        ),
        ("ansible/modules/unparsable.py", "def (:\n"),
        (
            "ansible/modules/probe.py",
            "import json\nprint(json.dumps({\"ok\": True}))\n",
        ),
    ]);
    let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
    for (fqn, said) in [
        ("ansible.modules.broken", "broken on purpose"),
        ("ansible.modules.unparsable", "SyntaxError"),
    ] {
        let first = call(&mut server, fqn);
        let second = call(&mut server, fqn);
        assert!(first.failed() && second.failed(), "{:?}", first.0);
        assert!(
            first.0["msg"].as_str().unwrap().contains(said),
            "{:?}",
            first.0
        );
        assert_eq!(first.0["msg"], second.0["msg"]);
    }
    let after = call(&mut server, "ansible.modules.probe");
    assert_eq!(after.0["ok"], true, "{:?}", after.0);
    assert!(server.alive());
}

/// A package installed after a module's imports is seen by that module's next task.
#[test]
fn a_package_installed_after_a_module_s_imports_is_seen_by_its_next_task() {
    let blob = runpy_blob(&[
        (
            "ansible/module_utils/late.py",
            "try:
    import volant_late
    HAS = True
except ImportError:
    HAS = False
",
        ),
        (
            "ansible/modules/probe.py",
            "import json, os
from ansible.module_utils.late import HAS
print(json.dumps({\"has\": HAS, \"parent\": os.getppid()}))
",
        ),
    ]);
    let site = scratch();
    let environment = BTreeMap::from([(
        "PYTHONPATH".to_string(),
        site.path().to_str().unwrap().to_string(),
    )]);
    let mut server = Server::start_in(HOST_PYTHON, blob.path(), &environment, &|| false).unwrap();
    // Two tasks before the package comes, so the server is done with its imports for the
    // first: nothing it does races the install.
    let before = call(&mut server, "ansible.modules.probe");
    assert_eq!(before.0["has"], false, "{:?}", before.0);
    let still = call(&mut server, "ansible.modules.probe");
    assert_eq!(still.0["has"], false, "{:?}", still.0);

    std::fs::create_dir(site.path().join("volant_late")).unwrap();
    std::fs::write(site.path().join("volant_late/__init__.py"), "").unwrap();
    let installed = call(&mut server, "ansible.modules.probe");
    let again = call(&mut server, "ansible.modules.probe");
    assert_eq!(installed.0["has"], true, "{:?}", installed.0);
    assert_ne!(installed.0["parent"], before.0["parent"]);
    assert_eq!(again.0["has"], true, "{:?}", again.0);
    assert_eq!(
        again.0["parent"], installed.0["parent"],
        "a server that had not moved was replaced"
    );
}

/// A task whose environment sets `PYTHONPATH` runs under an interpreter started with it, as the
/// reference's does, and the warm server behind it is left as it was.
#[test]
fn a_task_that_sets_pythonpath_gets_an_interpreter_started_with_it() {
    let root = scratch();
    let tmp = root.path().to_str().unwrap().to_string();
    let payload = cached_payload(
        root.path(),
        "\n    import os\n    try:\n        import volant_task_path_probe\n        found = volant_task_path_probe.VALUE\n    except ImportError:\n        found = None\n    print(json.dumps({\"found\": found, \"parent\": os.getppid()}))\n",
    );
    let path = scratch();
    std::fs::write(
        path.path().join("volant_task_path_probe.py"),
        "VALUE = \"from the task path\"\n",
    )
    .unwrap();
    let mut context = Context {
        remote_tmp: tmp.clone(),
        ..Context::default()
    };
    context.environment.insert(
        "PYTHONPATH".into(),
        path.path().to_str().unwrap().to_string(),
    );
    batch_started();
    let task = |context: &Context| done(run(&payload, &args(json!({})), context, &|| false).0);

    let warm = task(&Context {
        remote_tmp: tmp.clone(),
        ..Context::default()
    });
    let with = task(&context);
    let again = task(&Context {
        remote_tmp: tmp.clone(),
        ..Context::default()
    });
    assert_eq!(with.0["found"], "from the task path", "{:?}", with.0);
    assert_ne!(with.0["parent"], warm.0["parent"]);
    assert_eq!(warm.0["found"], Value::Null, "{:?}", warm.0);
    assert_eq!(again.0["found"], Value::Null, "{:?}", again.0);
    assert_eq!(
        again.0["parent"], warm.0["parent"],
        "the warm server was replaced"
    );
}

/// The oversized-result sentence quotes the bound the server sent in its frame.
#[test]
fn the_oversized_result_sentence_quotes_the_server_s_own_limit() {
    let result = result(&json!({
        "exit": 0,
        "signal": null,
        "stdout": "",
        "stderr": "",
        "truncated": true,
        "limit": 1234,
    }));
    assert!(
        result.0["msg"]
            .as_str()
            .unwrap()
            .starts_with("the module wrote more than 1234 bytes"),
        "{:?}",
        result.0
    );
}

/// A cancel while the server starts or imports is answered as a cancel, without waiting for it.
#[test]
fn a_cancel_during_start_or_import_is_a_cancel() {
    {
        let root = scratch();
        let tmp = root.path().to_str().unwrap().to_string();
        let payload = cached_payload(root.path(), "\n\nimport time\ntime.sleep(120)\n");
        batch_started();
        let run = run(
            &payload,
            &args(json!({})),
            &Context {
                remote_tmp: tmp.clone(),
                ..Context::default()
            },
            &|| true,
        )
        .0;
        assert!(
            matches!(run, Run::Cancelled),
            "the start was not cancelled: {:?}",
            match run {
                Run::Done(result) => Value::Object(result.0),
                Run::Cancelled => Value::Null,
            }
        );
    }
    {
        let (blob, gate) = gated_blob();
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let first = call(&mut server, "ansible.modules.probe");
        assert!(!first.failed(), "{:?}", first.0);
        let cancelled = server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| true,
        );
        assert!(matches!(cancelled, Run::Cancelled));
        std::fs::write(&gate, "").unwrap();
        let after = call(&mut server, "ansible.modules.probe");
        assert!(!after.failed(), "{:?}", after.0);
        assert_eq!(
            after.0["parent"], first.0["parent"],
            "the server was replaced"
        );
    }
}

/// A payload is hashed once while its server lives and verified again when replaced under it.
#[test]
fn a_payload_is_hashed_once_and_verified_again_when_replaced() {
    {
        let root = scratch();
        let tmp = root.path().to_str().unwrap().to_string();
        let payload = cached_payload(root.path(), "\n    print(json.dumps({}))\n");
        batch_started();
        let before = crate::blobs::HASHED.load(Ordering::Relaxed);
        for _ in 0..3 {
            let result = done(
                run(
                    &payload,
                    &args(json!({})),
                    &Context {
                        remote_tmp: tmp.clone(),
                        ..Context::default()
                    },
                    &|| false,
                )
                .0,
            );
            assert!(!result.failed(), "{:?}", result.0);
        }
        assert_eq!(crate::blobs::HASHED.load(Ordering::Relaxed) - before, 1);
    }
    {
        for renamed in [true, false] {
            let root = scratch();
            let tmp = root.path().to_str().unwrap().to_string();
            let payload = cached_payload(root.path(), "\n    print(json.dumps({}))\n");
            batch_started();
            let first = done(
                run(
                    &payload,
                    &args(json!({})),
                    &Context {
                        remote_tmp: tmp.clone(),
                        ..Context::default()
                    },
                    &|| false,
                )
                .0,
            );
            assert!(!first.failed(), "{:?}", first.0);

            let at = crate::blobs::path(root.path().to_str().unwrap(), &payload.blob).unwrap();
            let old = std::fs::metadata(&at).unwrap();
            let mut bytes = std::fs::read(&at).unwrap();
            let last = bytes.len() - 1;
            bytes[last] ^= 0xff;
            let target = if renamed {
                at.with_extension("fresh")
            } else {
                at.clone()
            };
            std::fs::write(&target, &bytes).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&target)
                .unwrap()
                .set_modified(old.modified().unwrap())
                .unwrap();
            if renamed {
                std::fs::rename(&target, &at).unwrap();
            }

            let second = done(
                run(
                    &payload,
                    &args(json!({})),
                    &Context {
                        remote_tmp: tmp.clone(),
                        ..Context::default()
                    },
                    &|| false,
                )
                .0,
            );
            assert_eq!(
                second.0["msg"],
                format!("payload {} is not on this host", payload.blob),
                "renamed {renamed}: {:?}",
                second.0
            );
        }
    }
}

/// The server reports where a task's time went: the fork, the import up to the module building
/// its `AnsibleModule`, and the rest.
#[test]
fn the_server_reports_where_the_time_went() {
    let blob = stub_blob(
        "\n    import time\n    from ansible.module_utils.basic import AnsibleModule\n    time.sleep(0.02)\n    if not args.get(\"bare\"):\n        AnsibleModule()\n    time.sleep(0.03)\n    print(json.dumps({}))\n",
    );
    std::fs::write(
        blob.path().join("ansible/module_utils/basic.py"),
        "class AnsibleModule:\n    def __init__(self):\n        pass\n",
    )
    .unwrap();
    let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
    let result = done(server.run(
        &payload("ansible.modules.probe"),
        &args(json!({})),
        &Context::default(),
        &|| false,
    ));
    assert!(!result.failed(), "{:?}", result.0);
    let timing = server.timing;
    assert!(timing.fork.is_some(), "{timing:?}");
    assert!(timing.import.is_some_and(|us| us >= 20_000), "{timing:?}");
    assert!(timing.module.is_some_and(|us| us >= 30_000), "{timing:?}");

    let result = done(server.run(
        &payload("ansible.modules.probe"),
        &args(json!({"bare": true})),
        &Context::default(),
        &|| false,
    ));
    assert!(!result.failed(), "{:?}", result.0);
    let timing = server.timing;
    assert_eq!(timing.import, None, "{timing:?}");
    assert!(timing.module.is_some_and(|us| us >= 50_000), "{timing:?}");

    assert_eq!(
        super::timing(&json!({"exit": 0})),
        Timing::default(),
        "a frame from a server that measured nothing"
    );
}

/// A cancelled module is killed and the server survives; a cancel is still a cancel when the
/// server cannot finish reaping.
#[test]
fn a_cancelled_module_is_reaped_and_the_server_survives_or_is_given_up() {
    {
        let blob = stub_blob(SLEEP_IF_ASKED);
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        let run = server.run(
            &payload("ansible.modules.probe"),
            &args(json!({"sleep": 120})),
            &Context::default(),
            &|| true,
        );
        // Not an assertion about elapsed time: the cancelled call only answers `Cancelled` once
        // the server has handed back the child's result, so a kill that missed would have ended
        // this call as a failed task instead.
        assert!(matches!(run, Run::Cancelled), "the task was not cancelled");
        assert!(server.alive());
        // The server answers again, which it could not do if the cancelled task had left a
        // frame of its own unread in the pipe.
        let after = done(server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| false,
        ));
        assert_eq!(after.0["slept"], false, "{:?}", after.0);
    }
    {
        let blob = stub_blob(
            "\n    import os, subprocess\n    subprocess.Popen(['sleep', '120'], preexec_fn=os.setsid)\n    import time\n    time.sleep(120)\n",
        );
        let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
        server.reap_timeout = Duration::from_millis(300);
        let run = server.run(
            &payload("ansible.modules.probe"),
            &args(json!({})),
            &Context::default(),
            &|| true,
        );
        assert!(
            matches!(run, Run::Cancelled),
            "a cancel became something else"
        );
    }
}

/// A module that outlives its `timeout` is killed and reported as timed out, in the same
/// words a native module uses.
#[test]
fn a_module_that_outlives_its_timeout_is_killed() {
    let blob = stub_blob(SLEEP_IF_ASKED);
    let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
    let context = Context {
        timeout: Some(Duration::from_secs(1)),
        ..Context::default()
    };
    let result = done(server.run(
        &payload("ansible.modules.probe"),
        &args(json!({"sleep": 120})),
        &context,
        &|| false,
    ));
    assert!(result.failed());
    assert!(
        result.0["msg"].as_str().unwrap().contains("Timed out"),
        "{:?}",
        result.0
    );
    assert!(server.alive());
}

/// A payload that is not cached fails naming it; one no server can start against refuses the
/// rest of the batch.
#[test]
fn an_uncached_or_unstartable_payload_is_refused_naming_it() {
    {
        let hash = "0".repeat(64);
        let result = done(
            run(
                &PythonPayload {
                    blob: hash.clone(),
                    module_fqn: "ansible.modules.ping".into(),
                    profile: "legacy".into(),
                    rlimit_nofile: 0,
                    extensions: Map::new(),
                    interpreter: HOST_PYTHON.into(),
                },
                &args(json!({})),
                &Context::default(),
                &|| false,
            )
            .0,
        );
        assert!(result.failed());
        assert_eq!(
            result.0["msg"],
            format!("payload {hash} is not on this host")
        );
    }
    {
        let root = scratch();
        let tmp = root.path().to_str().unwrap().to_string();
        let zip = b"not a payload at all";
        let hash = blake3::hash(zip).to_hex().to_string();
        crate::blobs::store(root.path().to_str().unwrap(), &hash, zip, false).unwrap();

        let payload = PythonPayload {
            blob: hash,
            module_fqn: "ansible.modules.ping".into(),
            profile: "legacy".into(),
            rlimit_nofile: 0,
            extensions: Map::new(),
            interpreter: HOST_PYTHON.into(),
        };
        batch_started();
        let first = done(
            run(
                &payload,
                &args(json!({})),
                &Context {
                    remote_tmp: tmp.clone(),
                    ..Context::default()
                },
                &|| false,
            )
            .0,
        );
        let second = done(
            run(
                &payload,
                &args(json!({})),
                &Context {
                    remote_tmp: tmp.clone(),
                    ..Context::default()
                },
                &|| false,
            )
            .0,
        );

        assert!(first.failed() && second.failed());
        assert!(
            first.0["msg"].as_str().unwrap().contains(HOST_PYTHON),
            "{:?}",
            first.0
        );
        assert_eq!(
            first.0["msg"], second.0["msg"],
            "every task behind the first one is answered with the same sentence"
        );
    }
}

/// The interpreter the task names is the one the module runs under, not whatever the agent
/// would have picked for itself.
#[test]
fn the_payload_names_the_interpreter_the_module_runs_under() {
    let blob = stub_blob(
        r#"
    import sys
    print(json.dumps({"executable": sys.executable}))
"#,
    );
    let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
    let result = done(server.run(
        &payload("ansible.modules.probe"),
        &args(json!({})),
        &Context::default(),
        &|| false,
    ));
    // What that interpreter calls itself, asked of the interpreter rather than assumed: on
    // macOS `/usr/bin/python3` is a shim that hands over to the one inside the command line
    // tools, so the path the module reports is not the path the agent spawned.
    let itself = Command::new(HOST_PYTHON)
        .args(["-c", "import sys; sys.stdout.write(sys.executable)"])
        .output()
        .expect("the host python says where it is");
    assert_eq!(
        result.0["executable"],
        String::from_utf8(itself.stdout).unwrap(),
        "the module ran under an interpreter the payload did not name"
    );
}

/// A server that has died fails the task that needed it, saying so, instead of waiting on a
/// pipe nobody is answering.
#[test]
fn a_server_that_died_fails_the_task_it_was_needed_for() {
    let blob = stub_blob(SLEEP_IF_ASKED);
    let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
    server.child.kill().unwrap();
    server.child.wait().unwrap();

    let result = done(server.run(
        &payload("ansible.modules.probe"),
        &args(json!({})),
        &Context::default(),
        &|| false,
    ));
    assert!(result.failed());
    assert!(
        result.0["msg"].as_str().unwrap().contains("stopped"),
        "{:?}",
        result.0
    );
    assert!(!server.alive());
}

/// One server serves every task of a batch: the second module's parent is the first's.
#[test]
fn the_server_is_reused_across_the_tasks_of_a_batch() {
    let root = scratch();
    let tmp = root.path().to_str().unwrap().to_string();
    // The module reports the server it ran under, and can take that server down with it -
    // which is how the death is made to happen at a moment the test knows, rather than
    // racing a signal against the agent's next look at the table.
    let payload = cached_payload(
        root.path(),
        "\n    import os, signal\n    if args.get(\"kill_parent\"):\n        os.kill(os.getppid(), signal.SIGKILL)\n    print(json.dumps({\"parent\": os.getppid()}))\n",
    );
    let task = |args_: Value| {
        done(
            run(
                &payload,
                &args(args_),
                &Context {
                    remote_tmp: tmp.clone(),
                    ..Context::default()
                },
                &|| false,
            )
            .0,
        )
    };
    batch_started();

    let first = task(json!({}));
    assert!(!first.failed(), "{:?}", first.0);
    let second = task(json!({}));
    assert_eq!(
        second.0.get("parent"),
        first.0.get("parent"),
        "a second server was started for the second task: {:?}",
        second.0
    );

    // The task that kills its own server fails, saying so; the one behind it gets the batch's
    // one restart, under a server that is not the first.
    let died = task(json!({"kill_parent": true}));
    assert!(died.failed(), "{:?}", died.0);
    let restarted = task(json!({}));
    assert!(
        !restarted.failed(),
        "the dead server was not replaced: {:?}",
        restarted.0
    );
    assert_ne!(
        restarted.0.get("parent"),
        first.0.get("parent"),
        "the task ran under a server that is gone"
    );

    // A second death in the same batch is refused rather than restarted, so a server that
    // will not stay up costs one start and not one per task.
    assert!(task(json!({"kill_parent": true})).failed());
    let refused = task(json!({}));
    assert!(refused.failed(), "{:?}", refused.0);
    assert!(
        refused.0["msg"]
            .as_str()
            .unwrap()
            .contains("stopped twice in one batch"),
        "{:?}",
        refused.0
    );
}

/// The server's own imports do not come from the agent's working directory.
#[test]
fn the_agents_working_directory_is_not_on_the_servers_path() {
    let root = scratch();
    std::fs::write(
        root.path().join("selectors.py"),
        "raise SystemExit('a file in the working directory was imported')\n",
    )
    .unwrap();
    // nextest runs each test in its own process, so this reaches no other test.
    std::env::set_current_dir(root.path()).unwrap();

    let blob = stub_blob("\n    print(json.dumps({\"clean\": True}))\n");
    let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
    let result = done(server.run(
        &payload("ansible.modules.probe"),
        &args(json!({})),
        &Context::default(),
        &|| false,
    ));
    assert_eq!(result.0["clean"], true, "{:?}", result.0);
}

/// `rlimit_nofile` on the payload reaches the module's own process.
#[test]
fn the_payload_raises_the_open_file_limit_of_the_module() {
    // Every figure here comes from this process rather than from the host's defaults. A
    // runner whose soft limit already equals its hard limit leaves nothing to raise, so the
    // test lowers its own soft limit first and then asks for the value it started with,
    // which is reachable wherever it runs. Asserting a fixed number would assert the runner.
    let (soft, hard) = open_files();
    assert!(soft > 1, "this host grants too few open files to raise one");
    let floor = if soft > 128 { soft / 2 } else { soft - 1 };
    set_open_files(floor, hard);

    let blob = stub_blob(
        "\n    import resource\n    print(json.dumps({\"soft\": resource.getrlimit(resource.RLIMIT_NOFILE)[0]}))\n",
    );
    let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
    let inherited = done(server.run(
        &payload("ansible.modules.probe"),
        &args(json!({})),
        &Context::default(),
        &|| false,
    ));
    assert_eq!(
        inherited.0["soft"], floor,
        "the server did not inherit the limit this process set: {:?}",
        inherited.0
    );

    let mut wants = payload("ansible.modules.probe");
    wants.rlimit_nofile = soft;
    let raised = done(server.run(&wants, &args(json!({})), &Context::default(), &|| false));
    assert_eq!(
        raised.0["soft"], soft,
        "the payload's rlimit_nofile never reached the module: {:?}",
        raised.0
    );

    // And only for the task that asked: the next module is back where the server is, because
    // the limit was raised in a process that has since exited.
    let next = done(server.run(
        &payload("ansible.modules.probe"),
        &args(json!({})),
        &Context::default(),
        &|| false,
    ));
    assert_eq!(next.0["soft"], floor, "{:?}", next.0);

    set_open_files(soft, hard);
}

/// A daemon left behind by a module that outlives its timeout does not hold the server.
#[test]
fn a_daemon_the_module_leaves_behind_does_not_hold_the_server() {
    let blob = stub_blob(
        "\n    import os, time\n    if args.get(\"pidfile\"):\n        if os.fork() == 0:\n            null = os.open(os.devnull, os.O_RDWR)\n            for n in range(3):\n                os.dup2(null, n)\n            os.setsid()\n            with open(args[\"pidfile\"], \"w\") as f:\n                f.write(str(os.getpid()))\n            time.sleep(60)\n            os._exit(0)\n        time.sleep(30)\n    print(json.dumps({\"parent\": os.getppid()}))\n",
    );
    let dir = scratch();
    let pidfile = dir.path().join("daemon.pid");
    let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
    let task = |server: &mut Server, args_: Value, context: &Context| {
        done(server.run(
            &payload("ansible.modules.probe"),
            &args(args_),
            context,
            &|| false,
        ))
    };
    let first = task(&mut server, json!({}), &Context::default());
    assert!(!first.failed(), "{:?}", first.0);

    let started = Instant::now();
    let timed_out = task(
        &mut server,
        json!({"pidfile": pidfile}),
        &Context {
            timeout: Some(Duration::from_secs(1)),
            ..Context::default()
        },
    );
    let waited = started.elapsed();
    let next = task(&mut server, json!({}), &Context::default());
    if let Ok(pid) = std::fs::read_to_string(&pidfile) {
        // SAFETY: `kill` takes a pid and a signal and touches no memory of this process.
        unsafe { libc::kill(pid.trim().parse().unwrap(), libc::SIGKILL) };
    }
    assert_eq!(
        timed_out.0["msg"], "Task failed: Timed out after 1 second(s).",
        "{:?}",
        timed_out.0
    );
    assert_eq!(
        next.0.get("parent"),
        first.0.get("parent"),
        "the server was given up after {waited:?}: {:?}",
        next.0
    );
}

/// A module whose `AnsibleModule(...)` call raises shows Python's own traceback, without timing frames.
#[test]
fn the_timing_mark_leaves_the_module_s_traceback_alone() {
    let blob = stub_blob(
        "\n    from ansible.module_utils.basic import AnsibleModule\n    AnsibleModule(bogus=True)\n",
    );
    std::fs::write(
        blob.path().join("ansible/module_utils/basic.py"),
        "class AnsibleModule:\n    def __init__(self, argument_spec=None):\n        self.argument_spec = argument_spec\n",
    )
    .unwrap();
    let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
    let result = done(server.run(
        &payload("ansible.modules.probe"),
        &args(json!({})),
        &Context::default(),
        &|| false,
    ));
    let msg = result.0["msg"].as_str().unwrap_or_default();
    let frames: Vec<&str> = msg
        .lines()
        .filter(|line| line.trim_start().starts_with("File "))
        .filter_map(|line| line.rsplit(", in ").next())
        .collect();
    assert_eq!(frames, ["run_child", "run_module"], "{msg}");
    // The class name in the message depends on the Python version: the Linux machines write
    // `AnsibleModule.__init__()`, the macOS CI runner's interpreter writes `__init__()`.
    let last = msg.lines().last().unwrap_or_default();
    assert!(
        last.starts_with("TypeError: ")
            && last.ends_with("__init__() got an unexpected keyword argument 'bogus'"),
        "{msg}"
    );
}

/// A module that delivers no usable result fails the task carrying what it wrote, and the server lives.
#[rstest]
#[case::no_result("\n    pass\n", "the module wrote no result")]
#[case::noise(
    "\n    print('Traceback: everything is on fire')\n",
    "everything is on fire"
)]
#[case::result_followed_by_noise(
    "\n    print(json.dumps({\"changed\": True}))\n    print('and then this')\n",
    "and then this"
)]
#[case::killed_by_a_signal(
    "\n    import os, signal\n    os.kill(os.getpid(), signal.SIGKILL)\n",
    "the module was killed by signal 9"
)]
#[case::signal_after_a_result(
    "\n    import os, signal\n    print(json.dumps({\"changed\": False, \"msg\": \"all done\"}))\n    sys.stdout.flush()\n    os.kill(os.getpid(), signal.SIGKILL)\n",
    "the module was killed by signal 9"
)]
#[case::half_a_result(
    "\n    import sys\n    sys.stdout.write('{\"changed\": tr')\n",
    "{\"changed\": tr"
)]
#[case::exit_with_a_message(
    "\n    raise SystemExit('nothing to work with here')\n",
    "nothing to work with here"
)]
#[case::too_large_to_carry(
    "\n    import sys\n    sys.stdout.write(\"y\" * (5 * 1024 * 1024))\n",
    "more than 4194304 bytes, which is more than a result can carry"
)]
fn a_module_that_delivers_no_result_fails_the_task(#[case] body: &str, #[case] needle: &str) {
    let blob = stub_blob(body);
    let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
    let result = done(server.run(
        &payload("ansible.modules.probe"),
        &args(json!({})),
        &Context::default(),
        &|| false,
    ));
    assert!(result.failed(), "{:?}", result.0);
    assert!(
        result.0["msg"].as_str().unwrap().contains(needle),
        "{:?}",
        result.0
    );
    assert!(server.alive(), "the module's failure is not the server's");
}

/// A module's result is reported as it wrote it, with `changed` filled in as false when absent.
#[rstest]
#[case::own_failure(
    "\n    print(json.dumps({\"failed\": True, \"msg\": \"nothing to do here\"}))\n    raise SystemExit(1)\n",
    json!({"failed": true, "msg": "nothing to do here"}),
    false
)]
#[case::silent_about_changed(
    "\n    print(json.dumps({\"ping\": \"pong\"}))\n",
    json!({"changed": false, "ping": "pong"}),
    true
)]
#[case::a_change_is_kept(
    "\n    print(json.dumps({\"changed\": True}))\n",
    json!({"changed": true}),
    false
)]
#[case::noisy_stderr(
    "\n    import sys\n    sys.stderr.write(\"x\" * (1024 * 1024))\n    sys.stderr.flush()\n    print(json.dumps({\"noisy\": True}))\n",
    json!({"noisy": true}),
    false
)]
#[case::stdin_is_empty(
    "\n    import sys\n    print(json.dumps({\"stdin\": sys.stdin.read()}))\n",
    json!({"stdin": ""}),
    false
)]
fn a_module_result_is_reported_as_it_wrote_it(
    #[case] body: &str,
    #[case] expected: Value,
    #[case] exact: bool,
) {
    let blob = stub_blob(body);
    let mut server = Server::start(HOST_PYTHON, blob.path(), &|| false).unwrap();
    let result = done(server.run(
        &payload("ansible.modules.probe"),
        &args(json!({})),
        &Context::default(),
        &|| false,
    ));
    if exact {
        assert_eq!(Value::Object(result.0), expected);
    } else {
        for (key, want) in expected.as_object().unwrap() {
            assert_eq!(&result.0[key], want, "{key}: {:?}", result.0);
        }
    }
}

/// An interpreter that cannot start, is not there, or starts and says nothing fails the task
/// naming itself and what happened. `$PY` stands for the interpreter's path.
#[rstest]
#[case::cannot_start(
    Some("#!/bin/sh\necho 'boom: no python here' >&2\nexit 3\n"),
    &["boom: no python here", "$PY"]
)]
#[case::not_there(None, &["/nonexistent/python3"])]
#[case::says_nothing(Some("#!/bin/sh\nexit 0\n"), &["$PY", "it said nothing"])]
fn an_interpreter_that_does_not_start_names_itself(
    #[case] script: Option<&str>,
    #[case] needles: &[&str],
) {
    let dir = scratch();
    let path = match script {
        Some(source) => program(dir.path(), "python3", source)
            .to_str()
            .unwrap()
            .to_string(),
        None => "/nonexistent/python3".to_string(),
    };
    let err = Server::start(&path, Path::new("/nonexistent.zip"), &|| false)
        .unwrap_err()
        .to_string();
    for needle in needles {
        let needle = needle.replace("$PY", &path);
        assert!(err.contains(&needle), "{needle:?} not in {err}");
    }
}

/// A server that answers a request with anything but the pid it forked, ends mid task, or
/// never answers fails the task and is not kept; the reap allowance is shortened to 200 ms.
#[rstest]
#[case::wrong_frame(
    "#!/bin/sh\nprintf '\\0\\0\\0\\016{\"ready\":true}'\nprintf '\\0\\0\\0\\012{\"oops\":1}'\nsleep 30\n",
    "did not say which process it started"
)]
#[case::ends_mid_task(
    "#!/bin/sh\nprintf '\\0\\0\\0\\016{\"ready\":true}'\nexit 0\n",
    "stopped"
)]
#[case::never_answers(
    "#!/bin/sh\nprintf '\\0\\0\\0\\016{\"ready\":true}'\nsleep 120\n",
    "did not answer"
)]
fn a_server_that_breaks_the_handshake_fails_the_task(#[case] script: &str, #[case] needle: &str) {
    let asked = Instant::now();
    let dir = scratch();
    let fake = program(dir.path(), "python3", script);
    let mut server = Server::start(fake.to_str().unwrap(), Path::new("/blob"), &|| false).unwrap();
    server.reap_timeout = Duration::from_millis(200);
    let result = done(server.run(
        &payload("ansible.modules.probe"),
        &args(json!({})),
        &Context::default(),
        &|| false,
    ));
    assert!(result.failed());
    assert!(
        result.0["msg"].as_str().unwrap().contains(needle),
        "{:?}",
        result.0
    );
    assert!(!server.alive(), "a server that lost the thread is not kept");
    // Not a measurement of speed: the fake sleeps for minutes, so a ceiling far under that
    // says only that the agent stopped waiting on its own terms rather than on the fake's.
    assert!(
        asked.elapsed() < Duration::from_secs(30),
        "the agent waited on the server rather than on its own allowance"
    );
}

/// A module that sleeps only when the task asks it to, so one blob serves both the task that
/// has to be interrupted and the task that proves the server still answers afterwards.
const SLEEP_IF_ASKED: &str = r#"
    if args.get("sleep"):
        import time
        time.sleep(args["sleep"])
    print(json.dumps({"slept": bool(args.get("sleep"))}))
"#;

/// The interpreter every test here runs against. It has no ansible-core, which is what makes
/// the blob the only place the module_utils can come from.
const HOST_PYTHON: &str = "/usr/bin/python3";

/// One task of `fqn`, without arguments, on `server`.
fn call(server: &mut Server, fqn: &str) -> TaskResult {
    done(server.run(
        &payload(fqn),
        &args(json!({})),
        &Context::default(),
        &|| false,
    ))
}

fn payload(fqn: &str) -> PythonPayload {
    PythonPayload {
        blob: "0".repeat(64),
        module_fqn: fqn.into(),
        profile: "legacy".into(),
        rlimit_nofile: 0,
        extensions: Map::new(),
        interpreter: HOST_PYTHON.into(),
    }
}

/// A payload directory holding a stub `_loader` whose `run_module` runs `body`.
///
/// A directory rather than a zip: `sys.path` takes either, the Rust side hands over a path
/// either way, and writing a zip by hand here would buy nothing the golden suite does not
/// already cover with the real thing.
fn stub_blob(body: &str) -> tempfile::TempDir {
    let dir = scratch();
    let package = dir.path().join("ansible/module_utils/_internal/_ansiballz");
    std::fs::create_dir_all(&package).unwrap();
    for level in [
        "ansible",
        "ansible/module_utils",
        "ansible/module_utils/_internal",
        "ansible/module_utils/_internal/_ansiballz",
    ] {
        std::fs::write(dir.path().join(level).join("__init__.py"), "").unwrap();
    }
    // The server preloads this one too, for the reason its own comment gives.
    std::fs::write(dir.path().join("ansible/module_utils/basic.py"), "# stub\n").unwrap();
    std::fs::write(
        package.join("_loader.py"),
        format!(
            "import json, sys\n\n\ndef run_module(json_params, profile, module_fqn, modlib_path, extensions):\n    args = json.loads(json_params)[\"ANSIBLE_MODULE_ARGS\"]{body}"
        ),
    )
    .unwrap();
    dir
}

/// A payload whose `_loader` runs the module as the reference's does, through `runpy` as
/// `__main__`, with `files` written into it and an empty `ansible/modules` package.
fn runpy_blob(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = stub_blob(
        "\n    import runpy\n    runpy.run_module(module_fqn, run_name=\"__main__\", alter_sys=True)\n",
    );
    std::fs::create_dir_all(dir.path().join("ansible/modules")).unwrap();
    std::fs::write(dir.path().join("ansible/modules/__init__.py"), "").unwrap();
    for (name, source) in files {
        let at = dir.path().join(name);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, source).unwrap();
    }
    dir
}

/// A payload in this agent's own cache, built as a real zip by the host interpreter.
///
/// The shape the controller sends, rather than the directory the tests above hand straight to
/// a server: `python::run` verifies the blob by hashing the file it holds, so it needs one.
/// The archive has no `.zip` in its name and does not need one - `zipimport` recognises an
/// archive by its central directory.
fn cached_payload(remote_tmp: &Path, body: &str) -> PythonPayload {
    const BUILD: &str = r##"
import base64, io, sys, zipfile

loader = 'import json, sys\n\n\ndef run_module(json_params, profile, module_fqn, modlib_path, extensions):\n    args = json.loads(json_params)["ANSIBLE_MODULE_ARGS"]' + sys.argv[1]
buf = io.BytesIO()
archive = zipfile.ZipFile(buf, "w")
for name in [
    "ansible/__init__.py",
    "ansible/module_utils/__init__.py",
    "ansible/module_utils/_internal/__init__.py",
    "ansible/module_utils/_internal/_ansiballz/__init__.py",
]:
    archive.writestr(name, "")
archive.writestr("ansible/module_utils/basic.py", "# stub\n")
archive.writestr("ansible/module_utils/_internal/_ansiballz/_loader.py", loader)
archive.close()
sys.stdout.write(base64.b64encode(buf.getvalue()).decode())
"##;
    let built = Command::new(HOST_PYTHON)
        .args(["-c", BUILD, body])
        .output()
        .expect("the host python builds the payload");
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let zip_b64 = String::from_utf8(built.stdout).unwrap();
    let zip = volant_protocol::encoding::b64_decode(&zip_b64).unwrap();
    let hash = blake3::hash(&zip).to_hex().to_string();
    crate::blobs::store(remote_tmp.to_str().unwrap(), &hash, &zip, false).unwrap();
    PythonPayload {
        blob: hash,
        module_fqn: "ansible.modules.probe".into(),
        profile: "legacy".into(),
        rlimit_nofile: 0,
        extensions: Map::new(),
        interpreter: HOST_PYTHON.into(),
    }
}

/// This process's own soft and hard limits on open files.
#[cfg(unix)]
fn open_files() -> (u64, u64) {
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: `getrlimit` fills a structure this call owns and cannot fail for a resource
    // every process has.
    let limit = unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()), 0);
        limit.assume_init()
    };
    (limit.rlim_cur as u64, limit.rlim_max as u64)
}

/// Sets this process's soft limit on open files, which its children inherit.
///
/// nextest runs each test in its own process, so this reaches no other test; lowering a soft
/// limit needs no privilege, and raising one back to where it started is allowed for the same
/// reason the payload's own raise is.
#[cfg(unix)]
fn set_open_files(soft: u64, hard: u64) {
    let limit = libc::rlimit {
        rlim_cur: soft as libc::rlim_t,
        rlim_max: hard as libc::rlim_t,
    };
    // SAFETY: `limit` is a fully initialised structure that outlives the call.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
}

fn program(dir: &Path, name: &str, source: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(dir).unwrap();
    let at = dir.join(name);
    std::fs::write(&at, source).unwrap();
    std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755)).unwrap();
    at
}
