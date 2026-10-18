// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

/// A gathered fact sits in its own layer beside the host's map, not in it, and the backend
/// `package` and `service` pick is read off it all the same.
#[test]
fn a_gathered_fact_is_read_from_the_facts_layer() {
    let facts: Map<String, Value> =
        serde_json::from_str(r#"{"ansible_facts": {"pkg_mgr": "apt"}}"#).unwrap();
    let vars = HostVars {
        facts: Arc::new(facts),
        ..HostVars::default()
    };
    assert_eq!(fact(&vars, "pkg_mgr"), Some("apt"));
}

/// The two prefixes that name the same modules are read as such, and another collection's
/// module of the same name is not this one.
#[test]
fn the_builtin_prefixes_name_the_same_modules() {
    assert_eq!(
        kind("ansible.builtin.script"),
        Some(Kind::Bridged("script"))
    );
    assert_eq!(kind("ansible.legacy.uri"), Some(Kind::Bridged("uri")));
    assert_eq!(kind("community.general.script"), None);
    assert_eq!(kind("setup"), None);
    assert_eq!(kind("ansible.builtin.package"), Some(Kind::Package));
    assert_eq!(kind("ansible.legacy.service"), Some(Kind::Service));
    assert_eq!(kind("community.general.package"), None);
    assert_eq!(kind("ansible.builtin.copy"), Some(Kind::Copy));
    assert_eq!(kind("ansible.legacy.dnf"), Some(Kind::Dnf));
    assert_eq!(kind("community.general.dnf"), None);
    assert_eq!(kind("ansible.builtin.template"), Some(Kind::Template));
    assert_eq!(kind("ansible.builtin.unarchive"), Some(Kind::Unarchive));
    assert_eq!(kind("ansible.legacy.unarchive"), Some(Kind::Unarchive));
    assert_eq!(kind("community.general.unarchive"), None);
    assert_eq!(kind("ansible.builtin.fetch"), Some(Kind::Fetch));
    assert_eq!(kind("community.general.fetch"), None);
    assert_eq!(kind("ns.coll.shutdown"), None);
    note_collection_action("ns.coll.shutdown", "ns.coll.act", false);
    assert_eq!(kind("ns.coll.shutdown"), Some(Kind::Bridged("ns.coll.act")));
    assert_eq!(kind("ansible.builtin.reboot"), Some(Kind::Reboot));
    assert_eq!(kind("community.general.reboot"), None);
}

/// Every plugin the modules page lists is one this release dispatches.
#[test]
fn every_listed_plugin_has_a_kind() {
    for (name, _) in ACTION_PLUGINS {
        assert!(kind(name).is_some(), "{name}");
    }
}

/// Every module a plugin can run is a builtin module ansible-core builds a payload for, and
/// none is one this engine runs natively or on the controller.
#[test]
fn a_plugin_runs_only_what_the_union_can_hold() {
    for kind in [
        Kind::Copy,
        Kind::Dnf,
        Kind::Fetch,
        Kind::Package,
        Kind::Reboot,
        Kind::Service,
        Kind::Template,
        Kind::Unarchive,
        Kind::Bridged("assemble"),
        Kind::Bridged("gather_facts"),
        Kind::Bridged("uri"),
        Kind::Bridged("wait_for_connection"),
    ] {
        for module in modules_for(kind) {
            assert!(volant_protocol::modules::is_builtin(module), "{module}");
            assert!(!volant_protocol::modules::is_known(module), "{module}");
        }
    }
}
