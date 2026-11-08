// SPDX-License-Identifier: GPL-3.0-or-later
use rstest::rstest;
use serde_json::json;

use super::*;

#[rstest]
#[case::extra_vars(json!({"use_extra_vars": "yes"}), "use_extra_vars is not supported yet")]
#[case::strict_type(
    json!({"strict": [1]}),
    "Invalid type for configuration option strict: expected a boolean, got <class 'list'>"
)]
#[case::compose_type(
    json!({"compose": "x"}),
    "Invalid type for configuration option compose: expected a dict, got <class 'str'>"
)]
fn an_option_it_cannot_honour_is_refused(#[case] config: Value, #[case] message: &str) {
    let Value::Object(config) = config else {
        unreachable!()
    };
    assert_eq!(Options::read(&config).err().as_deref(), Some(message));
}

/// `use_vars_plugins` reads the `group_vars` and `host_vars` beside the sources, as
/// ansible-core 2.19.12's `inventory_constructed` target checks with keyed groups.
#[test]
fn keyed_groups_read_the_files_vars_plugins_find() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, text: &str| {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write(
        "1/one.yml",
        "all:
  children:
    stuff:
      hosts:
        testing:
",
    );
    write(
        "1/group_vars/stuff.yml",
        "iamdefined: group4testing
",
    );
    write(
        "1/host_vars/testing.yml",
        "hola: lola
",
    );
    write(
        "2/constructed.yml",
        "plugin: ansible.builtin.constructed
use_vars_plugins: true
keyed_groups:
  - key: iamdefined
    prefix: c
  - key: hola
    prefix: c
",
    );
    let source = |name: &str| Source::new(&dir.path().join(name).display().to_string());
    let loaded = super::super::load_sources(
        &[source("1/one.yml"), source("2/constructed.yml")],
        &super::super::PluginConfig::default(),
    );
    assert_eq!(loaded.error, None);
    let groups = loaded.inventory.group_names_of("testing");
    assert!(groups.contains(&"c_lola".to_string()), "{groups:?}");
    assert!(
        groups.contains(&"c_group4testing".to_string()),
        "{groups:?}"
    );
}

/// Measured on ansible-core 2.19.12: `group_vars/all.yml` reaches only the hosts a source put in
/// `all` while the sources are read, and the plugin's own directory is not read.
#[test]
fn group_vars_of_all_reach_only_the_hosts_a_source_put_in_all() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, text: &str| {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write("group_vars/all.yml", "only_all: yes\n");
    write("a.ini", "top1\n[web]\nw1\n");
    write(
        "b.yml",
        "all:\n  hosts:\n    y1:\n  children:\n    kids:\n      hosts:\n        y2:\nother:\n  hosts:\n    y3:\n",
    );
    write("own/group_vars/web.yml", "only_all: own\n");
    write(
        "own/c.yml",
        "plugin: constructed\nuse_vars_plugins: true\ncompose:\n  s: only_all | default('-')\n",
    );
    let source = |name: &str| Source::new(&dir.path().join(name).display().to_string());
    let loaded = super::super::load_sources(
        &[source("a.ini"), source("b.yml"), source("own/c.yml")],
        &super::super::PluginConfig::default(),
    );
    assert_eq!(loaded.error, None);
    let seen: Vec<(&str, Value)> = ["top1", "w1", "y1", "y2", "y3"]
        .into_iter()
        .map(|host| {
            let vars = loaded.inventory.host_line_vars(host).unwrap();
            (host, vars.get("s").cloned().unwrap_or(Value::Null))
        })
        .collect();
    let (yes, no) = (json!(true), json!("-"));
    assert_eq!(
        seen,
        [
            ("top1", yes.clone()),
            ("w1", no.clone()),
            ("y1", yes.clone()),
            ("y2", yes),
            ("y3", no)
        ]
    );
}
