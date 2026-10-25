// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant shadow replay` on a run ansible-core 2.19.12 recorded with the `volant.compat.record`
//! callback (`tests/shadow`, recorded in `/tmp/volant-shadow`): every task is answered from the
//! recording, no agent starts, and the output is the one ansible-playbook printed.
#![cfg(unix)]

#[path = "common/process.rs"]
mod process;

use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

/// Where the recording was made, as its paths and the reference's output name it.
const RECORDED_IN: &str = "/tmp/volant-shadow";

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    copy(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/shadow"),
        dir.path(),
    );
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

/// The replay of `recording` in `dir`, with no agent anywhere it could be started from.
fn replay(dir: &Path, recording: &str) -> Output {
    let no_agents = dir.join("no-agents");
    std::fs::create_dir_all(&no_agents).expect("mkdir");
    let mut command = Command::new(env!("CARGO_BIN_EXE_volant"));
    command
        .args(["shadow", "replay", recording])
        .current_dir(dir)
        .env("HOME", dir)
        .env("VOLANT_AGENT_DIR", &no_agents)
        .env_remove("ANSIBLE_CONFIG")
        .env_remove("VOLANT_PYTHON");
    process::run_within(&mut command, Duration::from_secs(60), None)
}

/// Every kind of task the fixture holds (role, handler, loop, retried `until`, `when` on a
/// `register`, `no_log`, `delegate_to`, include loop): printed and ended as ansible-playbook did.
#[test]
fn a_replay_prints_what_ansible_playbook_printed() {
    let dir = fixture();
    let out = replay(dir.path(), "recording.jsonl");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    // The directory as the replay sees it: macOS reaches its temporary one through a link.
    let here = std::fs::canonicalize(dir.path()).expect("canonical");
    let printed =
        String::from_utf8_lossy(&out.stdout).replace(&*here.to_string_lossy(), RECORDED_IN);
    let reference = std::fs::read_to_string(dir.path().join("ansible.out")).expect("reference");
    assert_eq!(printed, reference);
}

/// A playbook edited after the recording refuses with exit 4 and names the file.
#[test]
fn an_edited_playbook_refuses_the_replay() {
    let dir = fixture();
    let site = dir.path().join("site.yml");
    let mut text = std::fs::read_to_string(&site).expect("site.yml");
    text.push_str("# edited\n");
    std::fs::write(&site, text).expect("write");
    let out = replay(dir.path(), "recording.jsonl");
    assert_eq!(out.status.code(), Some(4));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("site.yml changed since the recording"),
        "{stderr}"
    );
}

/// A recording cut after the first task of the play replays up to there, says it is truncated,
/// and stops the hosts where the recording does.
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
    let out = replay(dir.path(), "cut.jsonl");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("truncated"), "{stderr}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("TASK [r1 : role task]"), "{stdout}");
    assert!(
        stdout.contains("the recording is truncated before this task"),
        "{stdout}"
    );
    assert_ne!(out.status.code(), Some(0));
}
