// SPDX-License-Identifier: GPL-3.0-or-later
//! The native `setup` against the Python one, on the machine under test.
//!
//! Facts belong to the machine, so no recorded answer can stand in for them: the expected value
//! is measured when the test runs, by the reference, on the machine the native just ran on. One
//! play gathers facts and writes `ansible_facts` to a file, once under `--facts native` and once
//! under `--facts python`, and the two are compared key by key.
//!
//! The machine under test is localhost over ssh (`just ssh-test`, the `ssh` CI job), or the host
//! `VOLANT_FACTS_HOST` names, reached through the caller's own ssh configuration
//! (`just facts-compare <host>`).
#![cfg(unix)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::{Map, Value, json};
use volant_protocol::facts::NATIVE_FACT_KEYS;

#[path = "../../volant-agent/tests/setup_exits/mod.rs"]
mod setup_exits;

const PLAY: &str = r#"- name: Gather and write the facts
  hosts: all
  gather_facts: true
  tasks:
    - name: Name the machine
      shell: '. /etc/os-release; echo "$PRETTY_NAME"; uname -r; uname -m'
      register: machine
    - name: Write the facts
      copy:
        content: "{{ {'machine': machine.stdout_lines, 'facts': ansible_facts} | to_json }}"
        dest: "{{ out }}"
    - name: Bring them back
      fetch:
        src: "{{ out }}"
        dest: "{{ local }}"
        flat: true
    - name: Leave nothing behind
      file:
        path: "{{ out }}"
        state: absent
"#;

/// Values that move between two gathers a second apart. Each is compared by its JSON type only.
/// `SSH_CLIENT`, `SSH_CONNECTION` and `XDG_SESSION_ID` belong to the ssh session: two runs
/// share one only while the first run's control master lingers, and measured without it they
/// differ by the client port and the session number. `env._` is not here: both paths start
/// from the same agent, so it is the same program on both. A `.*` entry covers every key below
/// it.
const LIVE: &[&str] = &[
    "date_time.*",
    "memfree_mb",
    "memory_mb.real.free",
    "memory_mb.real.used",
    "memory_mb.nocache.*",
    "memory_mb.swap.free",
    "memory_mb.swap.used",
    "memory_mb.swap.cached",
    "swapfree_mb",
    "env.SSH_CLIENT",
    "env.SSH_CONNECTION",
    "env.XDG_SESSION_ID",
];

/// Lists whose order the reference does not fix: `interfaces` goes through a set, measured in
/// three orders over three runs of the reference alone.
const UNORDERED: &[&str] = &["interfaces"];

/// What one run of the play left behind.
struct Gathered {
    machine: Vec<String>,
    facts: Map<String, Value>,
    /// The profile line of the gather: `path`, and `reason` on a hand-back.
    setup: Map<String, Value>,
}

fn tmp() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("volant-facts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn user() -> String {
    let out = Command::new("id").arg("-un").output().unwrap();
    assert!(out.status.success(), "'id -un' failed");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// The inventory and the file the play writes on the host. A named host keeps its agent in a
/// directory of this test's own, so a throwaway agent never lands where a real run caches one.
fn inventory(dir: &Path) -> (PathBuf, String) {
    let host = std::env::var("VOLANT_FACTS_HOST").unwrap_or_default();
    let (line, out) = if host.is_empty() {
        let key = std::env::var("VOLANT_SSH_TEST_KEY")
            .expect("VOLANT_SSH_TEST_KEY names a key accepted by localhost");
        let remote = dir.join("remote");
        (
            format!(
                "box ansible_host=127.0.0.1 ansible_user={} ansible_ssh_private_key_file={key} ansible_remote_tmp={} ansible_ssh_common_args='-F /dev/null'",
                user(),
                remote.display()
            ),
            remote.join("facts.json").display().to_string(),
        )
    } else {
        (
            format!("{host} ansible_remote_tmp=/tmp/volant-facts-compare"),
            "/tmp/volant-facts-compare/facts.json".to_string(),
        )
    };
    let path = dir.join("inventory.ini");
    std::fs::write(&path, format!("{line}\n")).unwrap();
    (path, out)
}

fn volant_within(args: &[&str], envs: &[(&str, &Path)], deadline: Duration) -> Output {
    let child = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(args)
        .envs(envs.iter().copied())
        .env("NO_COLOR", "1")
        .env("ANSIBLE_HOST_KEY_CHECKING", "False")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id().to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    rx.recv_timeout(deadline).map_or_else(
        |_| {
            let _ = Command::new("kill").args(["-KILL", &pid]).status();
            panic!("volant did not finish within {deadline:?}");
        },
        Result::unwrap,
    )
}

fn gather(dir: &Path, facts: &str) -> Gathered {
    let (inv, out) = inventory(dir);
    let play = dir.join("play.yml");
    std::fs::write(&play, PLAY).unwrap();
    let local = dir.join(format!("{facts}.json"));
    let profile = dir.join(format!("{facts}.profile"));
    let run = volant_within(
        &[
            "playbook",
            "-i",
            inv.to_str().unwrap(),
            "--facts",
            facts,
            "-e",
            &format!("out={out}"),
            "-e",
            &format!("local={}", local.display()),
            play.to_str().unwrap(),
        ],
        &[("VOLANT_PROFILE_JSON", &profile)],
        Duration::from_secs(300),
    );
    assert_eq!(
        run.status.code(),
        Some(0),
        "--facts {facts}: {}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    let mut written: Map<String, Value> =
        serde_json::from_slice(&std::fs::read(&local).unwrap()).unwrap();
    let machine = serde_json::from_value(written.remove("machine").unwrap()).unwrap();
    let Some(Value::Object(facts_map)) = written.remove("facts") else {
        panic!("--facts {facts}: the written file holds no facts object");
    };
    let setups: Vec<Map<String, Value>> = std::fs::read_to_string(&profile)
        .unwrap()
        .lines()
        .filter_map(|line| match serde_json::from_str(line).unwrap() {
            Value::Object(o)
                if o.get("module")
                    .and_then(Value::as_str)
                    .is_some_and(|m| m.ends_with("setup")) =>
            {
                Some(o)
            }
            _ => None,
        })
        .collect();
    let [setup] = <[_; 1]>::try_from(setups)
        .unwrap_or_else(|s| panic!("--facts {facts}: not one gather in the profile: {s:?}"));
    Gathered {
        machine,
        facts: facts_map,
        setup,
    }
}

fn is_live(path: &str) -> bool {
    LIVE.iter().any(|live| {
        live.strip_suffix(".*").map_or(path == *live, |parent| {
            path.strip_prefix(parent)
                .is_some_and(|rest| rest.starts_with('.'))
        })
    })
}

/// `value` with every live value replaced by its JSON type and every unordered list sorted.
fn normalised(path: &str, value: &Value) -> Value {
    if is_live(path) {
        let kind = match value {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        return json!(format!("<live {kind}>"));
    }
    match value {
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), normalised(&format!("{path}.{k}"), v)))
                .collect(),
        ),
        Value::Array(a) if UNORDERED.contains(&path) => {
            let mut sorted: Vec<String> = a.iter().map(Value::to_string).collect();
            sorted.sort();
            json!(sorted)
        }
        Value::Array(a) => Value::Array(a.iter().map(|v| normalised(path, v)).collect()),
        other => other.clone(),
    }
}

/// Whether `NATIVE_FACT_KEYS` holds this `ansible_facts` key, which drops the `ansible_` prefix
/// the module's result carries.
fn listed(key: &str) -> bool {
    NATIVE_FACT_KEYS.contains(&key) || NATIVE_FACT_KEYS.contains(&format!("ansible_{key}").as_str())
}

/// Every difference between the two gathers that the native is answerable for.
fn differences(native: &Map<String, Value>, python: &Map<String, Value>) -> Vec<String> {
    let mut found = Vec::new();
    for (key, value) in native {
        if !listed(key) {
            found.push(format!(
                "{key}: produced natively and not in NATIVE_FACT_KEYS"
            ));
        }
        match python.get(key) {
            None => found.push(format!(
                "{key}: produced natively, absent from the reference"
            )),
            Some(reference) => {
                let (n, p) = (normalised(key, value), normalised(key, reference));
                if n != p {
                    found.push(format!("{key}: native {n} reference {p}"));
                }
            }
        }
    }
    for key in python.keys().filter(|k| !native.contains_key(*k)) {
        if listed(key) {
            found.push(format!(
                "{key}: in NATIVE_FACT_KEYS and in the reference, absent natively"
            ));
        }
    }
    found
}

/// The test's prerequisites, checked before anything runs so a missing one is named.
fn check_the_environment() {
    let python = std::env::var("VOLANT_PYTHON")
        .expect("VOLANT_PYTHON names a python with ansible-core, the reference's setup");
    let imports = Command::new(&python)
        .args(["-c", "import ansible"])
        .status()
        .is_ok_and(|s| s.success());
    assert!(imports, "VOLANT_PYTHON ({python}) cannot import ansible");
    assert!(
        std::env::var_os("VOLANT_AGENT_DIR").is_some(),
        "VOLANT_AGENT_DIR names the directory holding the agent built for the machine under test"
    );
}

#[test]
#[ignore = "needs ansible-core and ssh to the machine under test, run through just ssh-test or just facts-compare"]
fn native_facts_equal_python_facts_on_this_machine() {
    check_the_environment();
    let dir = tmp();
    let native = gather(&dir, "native");
    // First, so a result is always read against the machine it was measured on.
    println!("machine: {}", native.machine.join(" | "));
    let python = gather(&dir, "python");
    assert_eq!(
        python.setup.get("path"),
        Some(&json!("python")),
        "--facts python did not run the Python setup: {:?}",
        python.setup
    );
    assert_eq!(
        python.machine, native.machine,
        "the two runs reached different machines"
    );
    match native.setup.get("path").and_then(Value::as_str) {
        Some("native") => {}
        Some("fallback") => {
            let reason = native
                .setup
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert!(
                setup_exits::is_host_exit(reason),
                "the native setup handed back for a reason that is not a host outside its subset: {reason}"
            );
            println!("the native setup handed back on this machine ({reason}): nothing compared");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        _ => panic!(
            "--facts native did not reach the native setup: {:?}",
            native.setup
        ),
    }
    let found = differences(&native.facts, &python.facts);
    assert!(
        found.is_empty(),
        "native and reference facts differ on this machine:\n{}",
        found.join("\n")
    );
    let reference_only: BTreeSet<&String> = python
        .facts
        .keys()
        .filter(|k| !native.facts.contains_key(*k))
        .collect();
    println!(
        "{} native keys equal to the reference's; {} reference-only keys, none listed: {:?}",
        native.facts.len(),
        reference_only.len(),
        reference_only
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn live_paths_are_matched_below_their_parent_only() {
    assert!(is_live("date_time.epoch"));
    assert!(is_live("memory_mb.nocache.free"));
    assert!(is_live("env.SSH_CONNECTION"));
    assert!(!is_live("env._"));
    assert!(!is_live("date_time"));
    assert!(!is_live("date_timezone"));
    assert!(!is_live("memory_mb.real.total"));
    assert!(!is_live("env.HOME"));
}

#[test]
fn a_wrong_unlisted_or_missing_key_is_named() {
    let native = json!({
        "processor_vcpus": 5,
        "interfaces": ["lo", "eth0"],
        "memfree_mb": 10,
        "invented": 1,
    });
    let python = json!({
        "processor_vcpus": 4,
        "interfaces": ["eth0", "lo"],
        "memfree_mb": 12,
        "machine": "x86_64",
        "mounts": [],
    });
    let found = differences(native.as_object().unwrap(), python.as_object().unwrap());
    assert_eq!(
        found,
        [
            "processor_vcpus: native 5 reference 4",
            "invented: produced natively and not in NATIVE_FACT_KEYS",
            "invented: produced natively, absent from the reference",
            "machine: in NATIVE_FACT_KEYS and in the reference, absent natively",
        ]
    );
}
