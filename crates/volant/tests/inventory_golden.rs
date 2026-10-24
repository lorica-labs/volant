// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant inventory` against the bytes `ansible-inventory` printed for every case of
//! `golden/inventories/cases.yml`: `--list` for each, `--host` for the first host, both with
//! `--yaml`, `--export` and the two, and for the `cli` cases `--host nosuch` and `--output`.
//! Regenerate with `just golden inventory`.
#![cfg(unix)]
use std::path::Path;
use std::process::Command;

use serde_json::Value;

#[path = "common/process.rs"]
mod process;

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Runs `volant inventory` in `cwd` with the case's sources and environment: exit code, stdout
/// and stderr, the directory's path shown as the recording shows it.
fn run(cwd: &Path, case: &Value, extra: &[&str]) -> (i32, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_volant"));
    command.arg("inventory");
    for source in case["sources"].as_array().unwrap() {
        command.args(["-i", source.as_str().unwrap()]);
    }
    for arg in case["args"].as_array().unwrap() {
        command.arg(arg.as_str().unwrap());
    }
    command
        .args(extra)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .env("NO_COLOR", "1")
        .env_remove("ANSIBLE_INVENTORY")
        .env_remove("ANSIBLE_CONFIG")
        .env_remove("VIRTUAL_ENV")
        .env_remove("VOLANT_PYTHON")
        .env("VOLANT_RUNTIME", "system");
    for (name, value) in case["env"].as_object().unwrap() {
        command.env(name, value.as_str().unwrap());
    }
    let out = process::run_within(&mut command, std::time::Duration::from_secs(60), None);
    let shown = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes).replace(&cwd.display().to_string(), "<golden-tmp>")
    };
    (
        out.status.code().expect("the run ends with a code"),
        shown(&out.stdout),
        shown(&out.stderr),
    )
}

/// `--yaml`, `--export` and both, for `--list` and for the first host: how many ran.
fn compare_dumps(root: &Path, name: &str, case: &Value, failures: &mut Vec<String>) -> usize {
    let Some(dumps) = case["dumps"].as_object() else {
        return 0;
    };
    for (key, want) in dumps {
        let (mut extra, options) = match key.strip_prefix("host_") {
            Some(options) => (
                vec!["--host", case["host"]["name"].as_str().unwrap()],
                options,
            ),
            None => (vec!["--list"], key.as_str()),
        };
        extra.extend_from_slice(match options {
            "yaml" => &["--yaml"][..],
            "export" => &["--export"],
            "yaml_export" => &["--yaml", "--export"],
            other => panic!("{name}: no dump is recorded as {other}"),
        });
        let (code, stdout, _) = run(root, case, &extra);
        let wanted = (
            i32::try_from(want["rc"].as_i64().unwrap()).unwrap(),
            want["stdout"].as_str().unwrap(),
        );
        if (code, stdout.as_str()) != wanted {
            failures.push(format!("{name}: {key} exit {code}\n{stdout}"));
        }
    }
    dumps.len()
}

#[test]
fn the_command_prints_the_bytes_the_reference_printed() {
    let expected: Value =
        serde_json::from_str(include_str!("golden/expected_inventory.json")).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("inventories");
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/inventories"),
        &root,
    );
    std::fs::create_dir(root.join("empty")).unwrap();
    let mut failures = Vec::new();
    let mut compared = 0;
    for (name, case) in expected["sources"].as_object().unwrap() {
        // A collection plugin runs in ansible-core's own Python.
        if !case["needs"].as_array().unwrap().is_empty() {
            continue;
        }
        if let Some(want) = case["list_stdout"].as_str() {
            let (code, stdout, _) = run(&root, case, &["--list"]);
            compared += 1;
            if (code, stdout.as_str())
                != (i32::try_from(case["rc"].as_i64().unwrap()).unwrap(), want)
            {
                failures.push(format!("{name}: --list exit {code}\n{stdout}"));
            }
        }
        let host = &case["host"];
        if let Some(want) = host["stdout"].as_str() {
            let name_arg = host["name"].as_str().unwrap();
            let (code, stdout, _) = run(&root, case, &["--host", name_arg]);
            compared += 1;
            if (i64::from(code), stdout.as_str()) != (host["rc"].as_i64().unwrap(), want) {
                failures.push(format!("{name}: --host {name_arg} exit {code}\n{stdout}"));
            }
        }
        let nosuch = &case["nosuch"];
        if nosuch.is_object() {
            let got = run(&root, case, &["--host", "nosuch"]);
            let want = (
                i32::try_from(nosuch["rc"].as_i64().unwrap()).unwrap(),
                nosuch["stdout"].as_str().unwrap().to_string(),
                nosuch["stderr"].as_str().unwrap().to_string(),
            );
            compared += 1;
            if got != want {
                failures.push(format!("{name}: --host nosuch {got:?} want {want:?}"));
            }
        }
        // `--graph`, `--graph --vars` and with `--export` for each case, then the cases that name
        // a group and the one run without the options the case was given.
        if let Some(graphs) = case["graphs"].as_object() {
            for (key, want) in graphs {
                let mut bare = case.clone();
                let extra: Vec<&str> = match key.as_str() {
                    "graph" => vec!["--graph"],
                    "graph_vars" => vec!["--graph", "--vars"],
                    "graph_vars_export" => vec!["--graph", "--vars", "--export"],
                    "graph_vars_bare" => {
                        bare["args"] = serde_json::json!([]);
                        vec!["--graph", "--vars"]
                    }
                    group => vec!["--graph", group.strip_prefix("graph_").unwrap(), "--vars"],
                };
                let (code, stdout, stderr) = run(&root, &bare, &extra);
                compared += 1;
                let wanted = (
                    i32::try_from(want["rc"].as_i64().unwrap()).unwrap(),
                    want["stdout"].as_str().unwrap(),
                );
                if (code, stdout.as_str()) != wanted {
                    failures.push(format!("{name}: {key} exit {code}\n{stdout}"));
                }
                if stderr != want["stderr"].as_str().unwrap() {
                    failures.push(format!("{name}: {key} stderr {stderr:?}"));
                }
            }
        }
        compared += compare_dumps(&root, name, case, &mut failures);
        if case["cli_only"] == true {
            // The JSON without the newline `print` adds, the YAML with the one it ends with.
            let yaml = case["dumps"]["yaml"]["stdout"].as_str().unwrap();
            let json = case["list_stdout"].as_str().unwrap().trim_end_matches('\n');
            for (options, want) in [(&["--list"][..], json), (&["--list", "--yaml"], yaml)] {
                let file = root.join("out.txt");
                let (code, stdout, _) =
                    run(&root, case, &[options, &["--output", "out.txt"]].concat());
                let written = std::fs::read_to_string(&file).unwrap_or_default();
                compared += 1;
                if (code, stdout.as_str(), written.as_str()) != (0, "", want) {
                    failures.push(format!(
                        "{name}: {options:?} --output exit {code}, wrote {written:?}"
                    ));
                }
                let _ = std::fs::remove_file(file);
            }
        }
    }
    assert!(compared > 100, "only {compared} comparisons ran");
    assert!(failures.is_empty(), "{}", failures.join("\n---\n"));
}
