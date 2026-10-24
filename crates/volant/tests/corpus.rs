// SPDX-License-Identifier: GPL-3.0-or-later
//! One corpus entry's logs from `just corpus`, compared between the two engines. Ignored: it
//! reads the run `just corpus` left under `VOLANT_CORPUS_DIR` for the entry `VOLANT_CORPUS` names.
#![cfg(target_os = "linux")]

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

#[path = "common/compare.rs"]
#[expect(dead_code, reason = "the reference tests use the rest")]
mod compare;

#[path = "../../volant-agent/tests/setup_exits/mod.rs"]
mod setup_exits;

/// Status (`ok`, `changed`, `fatal`) and body of each result line, by (task banner, host).
type Results = BTreeMap<(String, String), (String, Value)>;

/// Like `compare::results_by_task`, but one entry per host instead of the last host's.
fn results_by_host(log: &str) -> Results {
    let mut results = Results::new();
    let mut task = String::new();
    let mut lines = log.lines();
    while let Some(line) = lines.next() {
        if let Some(rest) = line.strip_prefix("TASK [")
            && let Some(end) = rest.find(']')
        {
            task = rest[..end].to_string();
        } else if let Some(status) = ["ok", "changed", "fatal"]
            .into_iter()
            .find(|s| line.strip_prefix(s).is_some_and(|r| r.starts_with(": [")))
            && let Some((head, json)) = line.split_once(" => ")
        {
            let host = head
                .split_once('[')
                .and_then(|(_, r)| r.split_once(']'))
                .map_or("", |(h, _)| h);
            let json = match json.strip_prefix("(item=") {
                Some(rest) => rest.split_once(") => ").map_or(json, |(_, body)| body),
                None => json,
            };
            let mut body = json.to_string();
            if body == "{" {
                for next in lines.by_ref() {
                    body.push('\n');
                    body.push_str(next);
                    if next == "}" {
                        break;
                    }
                }
            }
            let value = serde_json::from_str(&body).unwrap_or_else(|e| {
                panic!("task {task}: a result body that is not JSON ({e}): {body}")
            });
            results.insert(
                (task.clone(), host.to_string()),
                (status.to_string(), value),
            );
        }
    }
    results
}

/// The host lines of the recap, spaces collapsed, in a fixed order.
fn recap(log: &str) -> Vec<String> {
    let mut lines: Vec<String> = log
        .lines()
        .skip_while(|l| !l.starts_with("PLAY RECAP"))
        .skip(1)
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect();
    lines.sort();
    lines
}

fn without_volatile(body: &Value, volatile: &[&str]) -> Value {
    let mut body = body.clone();
    if let Some(map) = body.as_object_mut() {
        map.retain(|k, _| k != "invocation" && !volatile.contains(&k.as_str()));
    }
    body
}

#[test]
#[ignore = "reads the logs of a `just corpus` run"]
fn corpus_entry() {
    let name = std::env::var("VOLANT_CORPUS").expect("VOLANT_CORPUS names the corpus entry");
    let root = std::env::var_os("VOLANT_CORPUS_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/corpus"),
        PathBuf::from,
    );
    let dir = root.join(&name);
    let read = |file: &str| {
        std::fs::read_to_string(dir.join(file))
            .unwrap_or_else(|e| panic!("{}: {e}", dir.join(file).display()))
    };
    let entry: Value = serde_json::from_str(&read("entry.json")).expect("entry.json is JSON");
    let volatile: Vec<&str> = entry
        .get("volatile")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();

    let mut strict = Vec::new();
    let mut body_diff = String::new();
    for pass in ["first", "second"] {
        let reference = read(&format!("reference-{pass}.log"));
        let volant = read(&format!("volant-{pass}.log"));
        let codes = (
            read(&format!("reference-{pass}.rc")).trim().to_string(),
            read(&format!("volant-{pass}.rc")).trim().to_string(),
        );
        if codes.0 != codes.1 {
            strict.push(format!(
                "{pass} pass: exit code {} (reference) against {} (volant)",
                codes.0, codes.1
            ));
        }
        let recaps = (recap(&reference), recap(&volant));
        if recaps.0 != recaps.1 {
            strict.push(format!(
                "{pass} pass: PLAY RECAP differs: {:?} (reference) against {:?} (volant)",
                recaps.0, recaps.1
            ));
        }
        let failed = (
            compare::failed_tasks(&reference),
            compare::failed_tasks(&volant),
        );
        if failed.0 != failed.1 {
            strict.push(format!(
                "{pass} pass: failed tasks differ: {:?} (reference) against {:?} (volant)",
                failed.0, failed.1
            ));
        }
        let (want, got) = (results_by_host(&reference), results_by_host(&volant));
        let keys: BTreeSet<_> = want.keys().chain(got.keys()).collect();
        for key in keys {
            let (task, host) = key;
            match (want.get(key), got.get(key)) {
                (Some((a, _)), Some((b, _))) if a != b => strict.push(format!(
                    "{pass} pass: task [{task}] on {host}: {a} (reference) against {b} (volant)"
                )),
                (Some((_, a)), Some((_, b))) => {
                    let (a, b) = (without_volatile(a, &volatile), without_volatile(b, &volatile));
                    if !compare::same(&a, &b) {
                        let _ = writeln!(
                            body_diff,
                            "{pass} pass: task [{task}] on {host}\n  reference: {a}\n  volant:    {b}"
                        );
                    }
                }
                (Some(_), None) => strict.push(format!(
                    "{pass} pass: task [{task}] on {host}: a result in the reference, none in volant"
                )),
                _ => strict.push(format!(
                    "{pass} pass: task [{task}] on {host}: a result in volant, none in the reference"
                )),
            }
        }
    }
    std::fs::write(dir.join("body-diff.txt"), &body_diff).expect("body-diff.txt is writable");

    if let Some(first) = strict.first() {
        let mut result: Value = serde_json::from_str(&read("result.json")).expect("result.json");
        result["status"] = Value::from("red");
        let text = serde_json::to_string_pretty(&result).expect("result.json serialises");
        std::fs::write(dir.join("result.json"), text + "\n").expect("result.json is writable");
        panic!("{name}: {first} ({} difference(s) in all)", strict.len());
    }
}
