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
