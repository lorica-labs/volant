// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant shadow replay` on a run ansible-core 2.19.12 recorded with the `volant.compat.record`
//! callback (`tests/shadow`, recorded in `/tmp/volant-shadow`): every task is answered from the
//! recording, no agent starts, the run prints on stderr what ansible-playbook printed, and stdout
//! carries the report of what differs. Each kind of divergence comes from a copy of the
//! recording edited on purpose.
#![cfg(unix)]

#[path = "common/process.rs"]
mod process;

use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use serde_json::{Value, json};

/// Where the recording was made, as its paths and the reference's output name it.
const RECORDED_IN: &str = "/tmp/volant-shadow";

fn fixture() -> tempfile::TempDir {
    fixture_of("shadow")
}

fn fixture_of(name: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    copy(&tests.join(name), dir.path());
    dir
}

fn copy(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("read the fixture") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            std::fs::create_dir_all(&target).expect("mkdir");
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy");
        }
    }
}

/// The replay of `recording` in `dir` with `options`, with no agent anywhere it could be started
/// from and nothing on standard input.
fn replay(dir: &Path, recording: &str, options: &[&str]) -> Output {
    let no_agents = dir.join("no-agents");
    std::fs::create_dir_all(&no_agents).expect("mkdir");
    let mut command = Command::new(env!("CARGO_BIN_EXE_volant"));
    command
        .args(["shadow", "replay", recording])
        .args(options)
        .current_dir(dir)
        .env("HOME", dir)
        .env("VOLANT_AGENT_DIR", &no_agents)
        .env_remove("ANSIBLE_CONFIG")
        .env_remove("VOLANT_PYTHON")
        .stdin(std::process::Stdio::null());
    process::run_within(&mut command, Duration::from_secs(60), None)
}

/// The fixture's recording with `edit` applied to each event, written as `edited.jsonl`: an
/// event `edit` makes `null` is left out, and one it makes a list becomes those events.
fn edited(dir: &Path, edit: impl Fn(&mut Value, &dyn Fn(&str) -> String)) {
    let text = std::fs::read_to_string(dir.join("recording.jsonl")).expect("recording");
    let events: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let uuid = |name: &str| {
        events
            .iter()
            .find(|e| e["event"] == "task" && e["name"] == name)
            .map(|e| e["uuid"].as_str().unwrap().to_owned())
            .expect("a task of that name")
    };
    let lines: Vec<String> = events
        .iter()
        .cloned()
        .flat_map(|mut event| {
            edit(&mut event, &uuid);
            match event {
                Value::Null => Vec::new(),
                Value::Array(events) => events.iter().map(Value::to_string).collect(),
                event => vec![event.to_string()],
            }
        })
        .collect();
    std::fs::write(dir.join("edited.jsonl"), lines.join("\n") + "\n").expect("write");
}

/// The one divergence the edited recording gives, from `--json`, and the human report.
fn one_divergence(edit: impl Fn(&mut Value, &dyn Fn(&str) -> String)) -> (Value, String) {
    let dir = fixture();
    edited(dir.path(), edit);
    let out = replay(dir.path(), "edited.jsonl", &["--json"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let (summary, divergences) = lines.split_last().expect("a summary");
    assert!(summary.get("summary").is_some(), "{summary}");
    assert_eq!(divergences.len(), 1, "{divergences:?}");
    let human =
        String::from_utf8_lossy(&replay(dir.path(), "edited.jsonl", &[]).stdout).into_owned();
    (divergences[0].clone(), human)
}

fn is_result(event: &Value, uuid: &str, host: &str) -> bool {
    event["event"] == "result" && event["task"] == uuid && event["host"] == host
}

/// Every kind of task the fixture holds (role, handler, loop, retried `until`, `when` on a
/// `register`, `no_log`, `delegate_to`, include loop): printed and ended as ansible-playbook did.
#[test]
fn a_replay_prints_what_ansible_playbook_printed() {
    let dir = fixture();
    let out = replay(dir.path(), "recording.jsonl", &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    // The directory as the replay sees it: macOS reaches its temporary one through a link.
    let here = std::fs::canonicalize(dir.path()).expect("canonical");
    let printed =
        String::from_utf8_lossy(&out.stderr).replace(&*here.to_string_lossy(), RECORDED_IN);
    let reference = std::fs::read_to_string(dir.path().join("ansible.out")).expect("reference");
    assert_eq!(printed, reference);
    assert!(
        stdout.starts_with("Shadow replay: 23 tasks compared on 2 hosts, no divergence.\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Arguments not compared: no_log 2"),
        "{stdout}"
    );
}

/// A task the recording does not have on one host is a `flow` divergence there: that host is no
/// longer compared, and the other one is compared to its end.
#[test]
fn a_task_missing_from_the_recording_is_flow() {
    let (divergence, human) = one_divergence(|event, uuid| {
        if is_result(event, &uuid("secret"), "other") {
            *event = Value::Null;
        }
    });
    assert_eq!(divergence["kind"], "flow");
    assert_eq!(divergence["host"], "other");
    assert_eq!(divergence["task"]["path"], "site.yml:21");
    assert!(human.contains("flow  other  site.yml:21"), "{human}");
    assert!(human.contains("after a flow divergence"), "{human}");
}

/// An argument the module ran with that Volant sends otherwise is an `args` divergence.
#[test]
fn a_changed_argument_is_args() {
    let (divergence, human) = one_divergence(|event, uuid| {
        if is_result(event, &uuid("register"), "local") {
            event["result"]["invocation"]["module_args"]["_raw_params"] = json!("echo changed");
        }
    });
    assert_eq!(divergence["kind"], "args");
    assert_eq!(divergence["task"]["path"], "site.yml:14");
    assert_eq!(
        divergence["ansible"],
        json!({"_raw_params": "echo changed"})
    );
    assert_eq!(
        divergence["volant"],
        json!({"_raw_params": "echo registered"})
    );
    assert!(
        human.contains("args  local  site.yml:14  register"),
        "{human}"
    );
}

/// One more item in the recording than Volant loops over is a `loop` divergence.
#[test]
fn an_extra_item_is_loop() {
    let (divergence, human) = one_divergence(|event, uuid| {
        if is_result(event, &uuid("loop"), "local") && event["item"] == 1 {
            let mut extra = event.clone();
            extra["item"] = json!(2);
            *event = json!([event.clone(), extra]);
        }
    });
    assert_eq!(divergence["kind"], "loop");
    assert_eq!(divergence["task"]["path"], "site.yml:5");
    assert_eq!(divergence["ansible"], json!({"items": 3}));
    assert_eq!(divergence["volant"], json!({"items": 2}));
    assert!(human.contains("loop  local  site.yml:5  loop"), "{human}");
}

/// A status the recording gives otherwise than Volant decides is a `judgment` divergence.
#[test]
fn a_changed_status_is_judgment() {
    let (divergence, human) = one_divergence(|event, uuid| {
        if is_result(event, &uuid("fails"), "local") {
            event["status"] = json!("ok");
        }
    });
    assert_eq!(divergence["kind"], "judgment");
    assert_eq!(divergence["task"]["path"], "site.yml:30");
    assert_eq!(divergence["ansible"], json!({"status": "ok"}));
    assert_eq!(divergence["volant"], json!({"status": "failed"}));
    assert!(
        human.contains("judgment  local  site.yml:30  fails"),
        "{human}"
    );
}

/// A host the recorded inventory lacks is an `inventory` divergence.
#[test]
fn a_host_missing_from_the_inventory_is_inventory() {
    let (divergence, human) = one_divergence(|event, _| {
        if event["event"] == "inventory" {
            event["hosts"].as_object_mut().unwrap().remove("other");
        }
    });
    assert_eq!(divergence["kind"], "inventory");
    assert_eq!(divergence["host"], "other");
    assert_eq!(divergence["ansible"], Value::Null);
    assert_eq!(divergence["volant"], json!(["web"]));
    assert!(
        human.contains("inventory  other  inventory  groups"),
        "{human}"
    );
}

/// A playbook edited after the recording refuses with exit 4 and names the file.
#[test]
fn an_edited_playbook_refuses_the_replay() {
    let dir = fixture();
    let site = dir.path().join("site.yml");
    let mut text = std::fs::read_to_string(&site).expect("site.yml");
    text.push_str("# edited\n");
    std::fs::write(&site, text).expect("write");
    let out = replay(dir.path(), "recording.jsonl", &[]);
    assert_eq!(out.status.code(), Some(4));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("site.yml changed since the recording"),
        "{stderr}"
    );
}

/// A recording cut after the first task of the play replays up to there, stops the hosts where
/// the recording does, and the report says the recording is truncated.
#[test]
fn a_truncated_recording_replays_up_to_its_end() {
    let dir = fixture();
    let whole = std::fs::read_to_string(dir.path().join("recording.jsonl")).expect("recording");
    let results = whole
        .match_indices("\"event\": \"result\"")
        .nth(1)
        .expect("two results")
        .0;
    let cut = &whole[..whole[results..].find('\n').expect("a line end") + results + 20];
    std::fs::write(dir.path().join("cut.jsonl"), cut).expect("write");
    let out = replay(dir.path(), "cut.jsonl", &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("TASK [r1 : role task]"), "{stderr}");
    assert!(
        stderr.contains("the recording is truncated before this task"),
        "{stderr}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("The recording is truncated"), "{stdout}");
}

/// A `pause` that asked on a terminal (`tests/shadow-pause`, recorded in
/// `/tmp/volant-shadow-pause` with `yes` typed) takes the recorded answer and asks nothing.
#[test]
fn a_pause_takes_the_recorded_answer() {
    let dir = fixture_of("shadow-pause");
    let out = replay(dir.path(), "recording.jsonl", &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("\"msg\": \"yes\""), "{stderr}");
}

// `tests/shadow-unknown` was recorded in `/tmp/volant-shadow-unknown` with `VOLANT_SHADOW_CI=yes`
// and `VOLANT_SHADOW_ITEMS=a,b`, which the replay does not have.
/// A `pipe` argument and a `no_log` value are not compared; a `when`, a `loop` and an `until` on the
/// environment follow the recording. No divergence, and the output is ansible-playbook's.
#[test]
fn values_that_change_between_runs_are_not_compared() {
    let dir = fixture_of("shadow-unknown");
    let out = replay(dir.path(), "recording.jsonl", &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(stdout.contains("no divergence"), "{stdout}");
    assert!(stdout.contains("unknown: lookup 'pipe' 1"), "{stdout}");
    assert!(stdout.contains("unknown: no_log 1"), "{stdout}");
    let here = std::fs::canonicalize(dir.path()).expect("canonical");
    let printed = String::from_utf8_lossy(&out.stderr)
        .replace(&*here.to_string_lossy(), "/tmp/volant-shadow-unknown");
    let reference = std::fs::read_to_string(dir.path().join("ansible.out")).expect("reference");
    assert_eq!(printed, reference);
}

/// `volant shadow status` in `dir`, reading `dir/ansible.cfg`.
fn status(dir: &Path, options: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_volant"));
    command
        .args(["shadow", "status"])
        .args(options)
        .current_dir(dir)
        .env("HOME", dir)
        .env("ANSIBLE_CONFIG", dir.join("ansible.cfg"));
    process::run_within(&mut command, Duration::from_secs(60), None)
}

/// A replay appends one ledger line beside the `ansible.cfg` in effect, a refused one none; with
/// `shadow_runs = 3`, `status --require-ready` exits 1 after two and 0 after three.
#[test]
fn the_ledger_feeds_the_verdict() {
    let dir = fixture();
    std::fs::write(
        dir.path().join("ansible.cfg"),
        "[volant]
shadow_runs = 3
",
    )
    .unwrap();
    let command = |recording: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_volant"));
        command
            .args(["shadow", "replay", recording])
            .current_dir(dir.path())
            .env("HOME", dir.path())
            .env("ANSIBLE_CONFIG", dir.path().join("ansible.cfg"))
            .env("VOLANT_AGENT_DIR", dir.path().join("no-agents"))
            .env_remove("VOLANT_PYTHON")
            .stdin(std::process::Stdio::null());
        process::run_within(&mut command, Duration::from_secs(60), None)
    };
    let ledger = dir.path().join(".volant/shadow.jsonl");
    for _ in 0..2 {
        assert_eq!(command("recording.jsonl").status.code(), Some(0));
    }
    assert_eq!(command("missing.jsonl").status.code(), Some(4));
    let lines: Vec<Value> = std::fs::read_to_string(&ledger)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["verdict"], "clean");
    assert_eq!(lines[0]["playbook"], "site.yml");
    assert_eq!(lines[0]["ansible_core"], "2.19.12");
    assert!(
        lines[0]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == "site.yml:30")
    );

    let out = status(dir.path(), &["--require-ready"]);
    assert_eq!(out.status.code(), Some(1));
    let shown = String::from_utf8_lossy(&out.stdout);
    assert!(shown.contains("site.yml  not-ready  streak 2/3"), "{shown}");
    assert!(
        shown.contains("2 clean replays in a row, 3 wanted"),
        "{shown}"
    );

    assert_eq!(command("recording.jsonl").status.code(), Some(0));
    let out = status(dir.path(), &["--require-ready", "--json"]);
    let row: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(row["verdict"], "ready", "{row}");
    assert_eq!(out.status.code(), Some(0));
}
