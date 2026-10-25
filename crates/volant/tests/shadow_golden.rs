// SPDX-License-Identifier: GPL-3.0-or-later
//! Every recording of `tests/golden/shadow`, made by ansible-core 2.19.12 on a scenario of
//! `tests/scenarios` (`just golden shadow`), replays with no divergence: the scenarios are a
//! second parity check, through the replay this time.
#![cfg(unix)]

#[path = "common/process.rs"]
mod process;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Scenarios the replay cannot answer yet, left out of `just golden shadow`, with the issue that
/// will bring them in.
const NOT_YET: &[(&str, &str)] = &[
    (
        "async-poll",
        "https://github.com/lorica-labs/volant/issues/920",
    ),
    (
        "async-status",
        "https://github.com/lorica-labs/volant/issues/920",
    ),
];

fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for entry in std::fs::read_dir(from).expect("read the scenario") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy");
        }
    }
}

#[test]
fn every_recorded_scenario_replays_with_no_divergence() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut recordings: Vec<_> = std::fs::read_dir(tests.join("golden/shadow"))
        .expect("the shadow goldens")
        .map(|e| e.expect("entry").path())
        .collect();
    recordings.sort();
    assert!(recordings.len() >= 17, "{recordings:?}");
    let mut diverged = Vec::new();
    for recording in &recordings {
        let name = recording
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".jsonl.gz"))
            .expect("a recording name");
        assert!(
            NOT_YET.iter().all(|(n, _)| *n != name),
            "{name} is recorded and still listed as not replayable yet"
        );
        let dir = tempfile::tempdir().expect("temp dir");
        let copied = dir.path().join(name);
        copy(&tests.join("scenarios").join(name), &copied);
        let mut command = Command::new(env!("CARGO_BIN_EXE_volant"));
        command
            .args(["shadow", "replay", "--json", "--ledger"])
            .arg(dir.path().join("ledger.jsonl"))
            .arg(recording)
            .current_dir(&copied)
            .env("HOME", dir.path())
            .env("VOLANT_AGENT_DIR", dir.path().join("no-agents"))
            .env_remove("ANSIBLE_CONFIG")
            .env_remove("VOLANT_PYTHON")
            .stdin(std::process::Stdio::null());
        let out = process::run_within(&mut command, Duration::from_secs(60), None);
        if out.status.code() != Some(0) {
            diverged.push(format!("{name}:\n{}", String::from_utf8_lossy(&out.stdout)));
        }
    }
    assert!(diverged.is_empty(), "{}", diverged.join("\n"));
}
