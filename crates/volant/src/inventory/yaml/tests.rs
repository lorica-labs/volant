// SPDX-License-Identifier: GPL-3.0-or-later
use rstest::rstest;

use super::*;
use crate::inventory::PluginConfig;

fn read(text: &str, dialect: Dialect) -> (Inventory, Result<(), String>) {
    let config = PluginConfig::default();
    let cx = ParseCx::new(&config, &|| unreachable!("no helper is started"));
    let mut inv = Inventory::default();
    let result = match dialect {
        Dialect::Yaml => {
            let docs = yaml::load(text, "t.yml").unwrap();
            lower(&docs[0]).and_then(|g| read_groups(&g, dialect, &mut inv, None, &cx))
        }
        Dialect::Toml => super::super::toml::read_text(text, &mut inv, &cx, "t.toml"),
    };
    (inv, result)
}

#[rstest]
#[case::cycle(
    "a: {children: {b: {children: {a: ~}}}}",
    "Adding group 'b' as child to 'a' creates a recursive dependency loop."
)]
#[case::own_child("a: {children: {a: ~}}", "can't add group to itself")]
#[case::int_host(
    "g: {hosts: {456: ~}}",
    "Host pattern 456 must be a string. Enclose integers/floats in quotation marks."
)]
#[case::list_hosts(
    "g: {hosts: [a]}",
    "Invalid \"hosts\" entry for \"g\" group, requires a dictionary, found \"<class \
         'ansible.module_utils._internal._datatag._AnsibleTaggedList'>\" instead."
)]
#[case::skipped_child("a: {children: {m: 5}}", "m is not a known host nor group")]
#[case::empty("{}", "Parsed empty YAML file")]
#[case::plugin("plugin: x", "Plugin configuration YAML file, not YAML inventory")]
fn yaml_refusals(#[case] text: &str, #[case] message: &str) {
    let (_, result) = read(text, Dialect::Yaml);
    assert_eq!(result.unwrap_err(), message);
}

/// A group read before the error stays, as the reference's partial state does.
#[test]
fn a_cycle_keeps_what_was_read() {
    let (inv, result) = read(
        "x: {hosts: {h: ~}}\na: {children: {b: {children: {a: ~}}}}",
        Dialect::Yaml,
    );
    assert!(result.is_err());
    assert!(inv.group("x").is_some_and(|g| g.hosts == ["h"]));
}
