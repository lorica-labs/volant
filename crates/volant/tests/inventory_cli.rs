// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant inventory --list` and `--host`: stdout, stderr and the exit code. Every row was
//! measured on ansible-core 2.19.12 with the same inventory.
#![cfg(unix)]
use std::path::Path;
use std::process::Command;

use rstest::rstest;

#[path = "common/process.rs"]
mod process;

const HOSTS: &str = "[web]\nw1 ansible_host=10.0.0.1 http_port=80\nw2 http_port=8080\n\n[db]\nd1 role=primary\n\n[prod:children]\nweb\ndb\n\n[web:vars]\ntier=front\n\n[prod:vars]\nenv=production\nlist=[1, 2]\n";

const EXTRA: &str =
    "[web]\nw9 tpl=\"{{ inventory_hostname }}-x\" uni=\"café ü\" f=1.5 b=true n=null\n";

const D1: &str = "{\n    \"ansible_user\": \"hugo\",\n    \"dns\": \"1.1.1.1\",\n    \"env\": \"production\",\n    \"list\": [\n        1,\n        2\n    ],\n    \"role\": \"primary\"\n}\n";

const LIST_D1: &str = "{\n    \"_meta\": {\n        \"hostvars\": {\n            \"d1\": {\n                \"ansible_user\": \"hugo\",\n                \"dns\": \"1.1.1.1\",\n                \"env\": \"production\",\n                \"list\": [\n                    1,\n                    2\n                ],\n                \"role\": \"primary\"\n            }\n        },\n        \"profile\": \"inventory_legacy\"\n    },\n    \"all\": {\n        \"children\": [\n            \"ungrouped\",\n            \"prod\"\n        ]\n    },\n    \"db\": {\n        \"hosts\": [\n            \"d1\"\n        ]\n    },\n    \"prod\": {\n        \"children\": [\n            \"web\",\n            \"db\"\n        ]\n    }\n}\n";

const LIST_NOTHING: &str = "{\n    \"_meta\": {\n        \"hostvars\": {},\n        \"profile\": \"inventory_legacy\"\n    },\n    \"all\": {\n        \"children\": [\n            \"ungrouped\",\n            \"prod\"\n        ]\n    },\n    \"prod\": {\n        \"children\": [\n            \"web\",\n            \"db\"\n        ]\n    }\n}\n";

const W9: &str = "{\n    \"b\": \"true\",\n    \"dns\": \"1.1.1.1\",\n    \"f\": 1.5,\n    \"n\": \"null\",\n    \"tpl\": \"{{ inventory_hostname }}-x\",\n    \"uni\": \"café ü\"\n}\n";

const NO_ACTION: &str = "[ERROR]: No action selected, at least one of --host, --graph or --list needs to be specified.\n";
const CONFLICT: &str = "[ERROR]: Conflicting options used, only one of --host, --graph or --list can be used at the same time.\n";
const NO_HOST: &str = "[ERROR]: You must pass a single valid host to --host parameter\n";

/// Runs `volant inventory` in a directory holding both inventories and the variable files.
fn run(args: &[&str]) -> (i32, String, String) {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path();
    write(root, "hosts.ini", HOSTS);
    write(root, "extra.ini", EXTRA);
    write(root, "group_vars/all.yml", "dns: 1.1.1.1\n");
    write(root, "host_vars/d1.yml", "ansible_user: hugo\n");
    run_in(root, args)
}

fn write(root: &Path, name: &str, text: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
    std::fs::write(path, text).expect("the fixture");
}

fn run_in(root: &Path, args: &[&str]) -> (i32, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_volant"));
    command
        .arg("inventory")
        .args(args)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .env("NO_COLOR", "1")
        .env_remove("ANSIBLE_INVENTORY")
        .env_remove("ANSIBLE_CONFIG")
        .env_remove("VIRTUAL_ENV")
        .env_remove("VOLANT_PYTHON")
        .env("VOLANT_RUNTIME", "system");
    let out = process::run_within(&mut command, std::time::Duration::from_secs(60), None);
    (
        out.status.code().expect("the run ends with a code"),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[rstest]
#[case::list_limited(&["-i", "hosts.ini", "--list", "-l", "d1"], 0, LIST_D1, "")]
#[case::host(&["-i", "hosts.ini", "--host", "d1"], 0, D1, "")]
#[case::host_ignores_limit(&["-i", "hosts.ini", "--host", "d1", "-l", "web"], 0, D1, "")]
#[case::host_repeated_last_wins(&["-i", "hosts.ini", "--host", "w1", "--host", "d1"], 0, D1, "")]
#[case::list_nothing_matches(
    &["-i", "hosts.ini", "--list", "-l", "nosuch"],
    0,
    LIST_NOTHING,
    "[WARNING]: Could not match supplied host pattern, ignoring: nosuch\n"
)]
#[case::host_values_keep_their_text(&["-i", "extra.ini", "--host", "w9"], 0, W9, "")]
#[case::no_action(&["-i", "hosts.ini"], 5, "", NO_ACTION)]
#[case::conflict(&["-i", "hosts.ini", "--list", "--host", "d1"], 5, "", CONFLICT)]
#[case::conflict_with_graph(&["-i", "hosts.ini", "--list", "--graph"], 5, "", CONFLICT)]
#[case::no_such_host(
    &["-i", "hosts.ini", "--host", "nosuch"],
    5,
    "",
    "[WARNING]: Could not match supplied host pattern, ignoring: nosuch\n[ERROR]: You must pass a single valid host to --host parameter\n"
)]
#[case::host_is_a_group(&["-i", "hosts.ini", "--host", "web"], 5, "", NO_HOST)]
#[case::bad_pattern(
    &["-i", "hosts.ini", "--list", "-l", "~("],
    1,
    "",
    "[ERROR]: Invalid host list pattern: ~(\n"
)]
#[case::graph_is_refused(&["-i", "hosts.ini", "--graph"], 4, "", "[ERROR]: --graph is not supported yet\n")]
fn prints_what_the_reference_prints(
    #[case] args: &[&str],
    #[case] code: i32,
    #[case] stdout: &str,
    #[case] stderr: &str,
) {
    assert_eq!(run(args), (code, stdout.to_string(), stderr.to_string()));
}

/// Extra variables win over every other layer, and the list is the whole document with sorted
/// keys, the group variables folded in, and the vault value left as text.
#[test]
fn list_folds_variables_in_and_sorts_keys() {
    let (code, stdout, stderr) = run(&["-i", "hosts.ini", "--list", "-e", "dns=9.9.9.9"]);
    assert_eq!((code, stderr.as_str()), (0, ""));
    let doc: serde_json::Value = serde_json::from_str(&stdout).expect("JSON");
    assert_eq!(doc["_meta"]["hostvars"]["w2"]["dns"], "9.9.9.9");
    assert_eq!(doc["_meta"]["hostvars"]["w2"]["http_port"], 8080);
    assert_eq!(doc["web"]["hosts"], serde_json::json!(["w1", "w2"]));
    assert_eq!(
        doc["all"]["children"],
        serde_json::json!(["ungrouped", "prod"])
    );
    let keys: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("    \""))
        .map(|l| l.trim().split('"').nth(1).expect("a key"))
        .collect();
    assert_eq!(keys, ["_meta", "all", "db", "prod", "web"]);
}

/// `--output` writes the document that standard output would carry, minus the newline `print`
/// adds, and prints nothing.
#[test]
fn output_writes_the_file_and_prints_nothing() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    write(dir.path(), "hosts.ini", HOSTS);
    let (code, stdout, stderr) = run_in(
        dir.path(),
        &["-i", "hosts.ini", "--host", "w1", "--output", "out.json"],
    );
    assert_eq!((code, stdout.as_str(), stderr.as_str()), (0, "", ""));
    let (_, printed, _) = run_in(dir.path(), &["-i", "hosts.ini", "--host", "w1"]);
    let written = std::fs::read_to_string(dir.path().join("out.json")).expect("the file");
    assert_eq!(format!("{written}\n"), printed);
}
