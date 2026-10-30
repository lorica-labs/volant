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

const W1: &str = "{
    \"ansible_host\": \"10.0.0.1\",
    \"dns\": \"1.1.1.1\",
    \"env\": \"production\",
    \"http_port\": 80,
    \"list\": [
        1,
        2
    ],
    \"tier\": \"front\"
}
";

const D1_BOB: &str = "{
    \"ansible_user\": \"bob\",
    \"dns\": \"9.9.9.9\",
    \"env\": \"production\",
    \"list\": [
        1,
        2
    ],
    \"role\": \"primary\"
}
";

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
#[case::host_is_a_pattern_of_one(&["-i", "hosts.ini", "--host", "w1*"], 0, W1, "")]
#[case::host_pattern_with_an_intersection(&["-i", "hosts.ini", "--host", "w1:&web"], 0, W1, "")]
#[case::extra_vars_win_over_group_and_host_vars(
    &["-i", "hosts.ini", "--host", "d1", "-e", "dns=9.9.9.9", "-e", "ansible_user=bob"],
    0,
    D1_BOB,
    ""
)]
#[case::host_is_a_group(&["-i", "hosts.ini", "--host", "web"], 5, "", NO_HOST)]
#[case::bad_pattern(
    &["-i", "hosts.ini", "--list", "-l", "~("],
    1,
    "",
    "[ERROR]: Invalid host list pattern: ~(\n"
)]
#[case::graph_to_a_host(&["-i", "hosts.ini", "--graph", "d1"], 5, "", "[ERROR]: Pattern must be valid group name when using --graph\n")]
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

/// A template literal's backslashes print as each source holds them, whatever the template engine
/// is given to read them as written.
#[test]
fn template_backslashes_print_as_written() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path();
    write(
        root,
        "inv.ini",
        "h1 i=\"{{ 'a\\.b' | regex_replace('\\.', '-') }}\" j=\"{{ 'a\\nb' | length }}\"\n",
    );
    write(
        root,
        "inv.yml",
        "all:\n  hosts:\n    h2:\n      r: \"{{ \\\"a.b\\\" | regex_replace(\\\"\\\\\\\\.\\\", \\\"-\\\") }}\"\n      j: '{{\"a\\nb\"|length}}'\n",
    );
    write(
        root,
        "inv.toml",
        "[ungrouped.hosts.h3]\nt = '{{ \"a\\nb\" | length }}'\n",
    );
    let sources = ["-i", "inv.ini", "-i", "inv.yml", "-i", "inv.toml"];
    let host = |name: &str, extra: &[&str]| {
        let args: Vec<&str> = sources
            .iter()
            .copied()
            .chain(["--host", name])
            .chain(extra.iter().copied())
            .collect();
        run_in(root, &args)
    };
    assert_eq!(
        host("h1", &[]),
        (
            0,
            "{\n    \"i\": \"{{ 'a\\\\.b' | regex_replace('\\\\.', '-') }}\",\n    \"j\": \"{{ 'a\\\\nb' | length }}\"\n}\n".to_string(),
            String::new()
        )
    );
    assert_eq!(
        host("h2", &["--yaml"]),
        (
            0,
            "j: '{{\"a\\nb\"|length}}'\nr: '{{ \"a.b\" | regex_replace(\"\\\\.\", \"-\") }}'\n"
                .to_string(),
            String::new()
        )
    );
    assert_eq!(
        host("h3", &[]),
        (
            0,
            "{\n    \"t\": \"{{ \\\"a\\\\nb\\\" | length }}\"\n}\n".to_string(),
            String::new()
        )
    );
}

/// `-e` that cannot be read fails each plugin that takes the source, then the run, measured on
/// ansible-core 2.19.12: exit 4 for invalid YAML, 5 for a value that is no mapping.
#[rstest]
#[case::invalid_yaml(
    "{bad",
    4,
    "YAML parsing failed: While parsing a flow mapping did not find expected ',' or '}'.",
    "\nOrigin: <CLI option '-e'>:2:1\n\n(source not shown: TypeError)\n"
)]
#[case::not_a_mapping(
    "[1]",
    5,
    "Invalid extra vars data supplied. '[1]' could not be made into a dictionary",
    ""
)]
fn unreadable_extra_vars_fail_each_plugin(
    #[case] extra: &str,
    #[case] code: i32,
    #[case] message: &str,
    #[case] origin: &str,
) {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path().canonicalize().expect("the scratch path");
    write(&root, "inv.ini", "h1\n");
    let source = root.join("inv.ini").display().to_string();
    let failed = |plugin: &str| {
        let origin = if origin.is_empty() {
            format!("\nOrigin: <inventory plugin '{plugin}' with source '{source}'>\n")
        } else {
            origin.to_string()
        };
        format!(
            "[WARNING]: Failed to parse inventory with '{plugin}' plugin: {message}\n\nFailed to parse inventory with '{plugin}' plugin.\n\n<<< caused by >>>\n\n{message}{origin}\n"
        )
    };
    let want = format!(
        "{}[WARNING]: Unable to parse {source} as an inventory source\n[WARNING]: No inventory was parsed, only implicit localhost is available\n[ERROR]: {message}{origin}\n",
        failed("ini"),
    );
    let (got, stdout, stderr) = run_in(&root, &["-i", &source, "--list", "-e", extra]);
    assert_eq!(
        (got, stdout.as_str(), stderr.as_str()),
        (code, "", want.as_str())
    );
}
