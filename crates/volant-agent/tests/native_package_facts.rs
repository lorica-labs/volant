// SPDX-License-Identifier: GPL-3.0-or-later
//! The native `package_facts` through the real agent binary, on the machine that runs the test.
//!
//! Packages depend on the machine, so the answer is held to python-apt's own reading of it,
//! taken here the way the reference module takes it. The whole reference module is compared
//! elsewhere, live, by the native golden.
#![cfg(target_os = "linux")]

#[expect(
    dead_code,
    reason = "the shared helpers include some this file does not use"
)]
mod common;

use std::process::Command;

use serde_json::{Map, Value, json};
use volant_protocol::{
    ExecPath, FromAgent, PROTOCOL_VERSION, PythonPayload, Ran, Task, TaskResult, ToAgent,
};

/// What `package_facts` makes of python-apt's cache: every installed package, with its
/// installed version's details and first origin.
const PYTHON_APT: &str = r#"
import apt, json
cache = apt.Cache()
out = {}
for name in cache.keys():
    package = cache[name]
    if package.is_installed:
        version = package.installed
        out[name] = [dict(name=name, version=version.version, arch=version.architecture,
                          category=version.section, origin=version.origins[0].origin,
                          source="apt")]
print(json.dumps(out))
"#;

/// On this machine, the agent answers `package_facts` natively, and its packages are
/// python-apt's; or it hands back, and the task goes to the Python path.
///
/// What would make this red: a package, version, section or origin that python-apt reads
/// differently from the native, or a native answer on a machine where python-apt, which the
/// reference needs, is missing.
#[test]
fn package_facts_answers_as_python_apt_reads_this_machine() {
    let mut agent = common::spawn_agent();
    if !hello(&mut agent).contains(&"package_facts".to_string()) {
        eprintln!("this agent does not run package_facts natively: nothing to compare");
        return;
    }
    let (result, ran) = run_one(&mut agent, task(json!({})));
    match ran.path {
        ExecPath::Native => {
            let reference = python_apt()
                .expect("the native answered on a machine where python3 cannot import apt");
            let ours = &result.0["ansible_facts"]["packages"];
            let differ: Vec<&String> = reference
                .as_object()
                .unwrap()
                .keys()
                .chain(ours.as_object().unwrap().keys())
                .filter(|name| reference.get(name.as_str()) != ours.get(name.as_str()))
                .take(5)
                .collect();
            assert!(differ.is_empty(), "packages read differently: {differ:?}");
            assert_eq!(
                result.0["invocation"],
                json!({"module_args": {"manager": ["auto"], "strategy": "first"}})
            );
        }
        ExecPath::Fallback => eprintln!(
            "this machine is outside the native package_facts ({}): the Python path ran",
            ran.reason.unwrap_or_default()
        ),
        ExecPath::Python => panic!("an enabled native was not consulted"),
    }
}

/// python-apt's reading of this machine, or `None` when `/usr/bin/python3` cannot import it.
fn python_apt() -> Option<Value> {
    let out = Command::new("/usr/bin/python3")
        .args(["-c", PYTHON_APT])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| serde_json::from_slice(&out.stdout).expect("python-apt's answer is JSON"))
}

fn hello(agent: &mut common::Agent) -> Vec<String> {
    agent.send(&ToAgent::Hello {
        protocol: PROTOCOL_VERSION,
    });
    match agent.recv() {
        Some(FromAgent::Ready { natives, .. }) => natives,
        other => panic!("expected Ready, got {other:?}"),
    }
}

fn run_one(agent: &mut common::Agent, task: Task) -> (TaskResult, Ran) {
    agent.send(&ToAgent::RunBatch {
        id: 1,
        tasks: vec![task],
    });
    let answer = loop {
        match agent.recv() {
            Some(FromAgent::TaskResult { result, ran, .. }) => {
                break (result, ran.expect("the agent says how it ran the task"));
            }
            Some(FromAgent::Log { .. }) => {}
            other => panic!("expected a task result, got {other:?}"),
        }
    };
    assert!(matches!(
        agent.recv(),
        Some(FromAgent::BatchDone { batch: 1, .. })
    ));
    answer
}

/// A `package_facts` task whose payload the agent does not hold: the Python path shows as that
/// path's own failure.
fn task(args: Value) -> Task {
    Task {
        module: "package_facts".into(),
        args: args.as_object().unwrap().clone(),
        ignore_errors: true,
        payload: Some(PythonPayload {
            blob: "0".repeat(64),
            module_fqn: "ansible.modules.package_facts".into(),
            profile: "legacy".into(),
            rlimit_nofile: 0,
            extensions: Map::new(),
            interpreter: "/usr/bin/python3".into(),
        }),
        ..Task::default()
    }
}
