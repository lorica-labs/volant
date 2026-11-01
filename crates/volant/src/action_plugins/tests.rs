// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use serde_json::json;

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
    assert_eq!(kind("yum"), Some(Kind::Dnf));
    assert_eq!(kind("ansible.builtin.yum"), Some(Kind::Dnf));
    assert_eq!(kind("ansible.legacy.yum"), Some(Kind::Dnf));
    assert_eq!(kind("yum_repository"), None);
    assert_eq!(kind("community.general.yum"), None);
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

/// A word that is not `key=value` is left over for a plugin that does not take raw parameters,
/// and is its `_raw_params` for one that does.
#[test]
fn a_plugin_that_takes_raw_params_keeps_its_words() {
    let task = |module: &str| {
        let mut task = crate::playbook::PlayTask::empty();
        task.module = module.to_string();
        task.args.insert("_raw_params".into(), json!("junk"));
        task
    };
    note_raw_params("ns.coll.takes_raw");
    assert_eq!(
        crate::playbook::stray_raw_params(&task("ns.coll.takes_raw")),
        None
    );
    assert_eq!(
        crate::playbook::stray_raw_params(&task("ns.coll.no_raw")),
        Some("junk")
    );
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

/// The reference's list of actions that run under `async`, as measured, and the ones it fails.
#[rstest::rstest]
#[case::command("command", true)]
#[case::shell("ansible.builtin.shell", true)]
#[case::stat("stat", true)]
#[case::setup("setup", true)]
#[case::package("package", true)]
#[case::service("service", true)]
#[case::dnf("dnf", true)]
#[case::uri("uri", true)]
#[case::a_collection_module("ns.coll.thing", true)]
#[case::copy("copy", false)]
#[case::template("template", false)]
#[case::debug("debug", false)]
#[case::set_fact("set_fact", false)]
#[case::assert("assert", false)]
#[case::raw("raw", false)]
#[case::script("script", false)]
#[case::fetch("fetch", false)]
#[case::add_host("add_host", false)]
#[case::unarchive("unarchive", false)]
#[case::assemble("assemble", false)]
#[case::reboot("reboot", false)]
#[case::async_status("async_status", false)]
fn async_is_supported_where_the_reference_supports_it(#[case] module: &str, #[case] ok: bool) {
    assert_eq!(supports_async(module), ok, "{module}");
}

/// What a plugin reads of its link: an ssh link carries key, user and `ssh` arguments (quoted
/// as `shlex.quote`), a `local` one none; escalation and delegate are carried on any transport.
#[rstest::rstest]
#[case::ssh(
    json!({"ansible_connection": "ssh", "ansible_user": "deploy", "ansible_ssh_private_key_file": "/k/id",
           "ansible_ssh_common_args": "-o StrictHostKeyChecking=no -o 'ProxyCommand=nc %h 22'"}),
    None,
    None,
    Some("/k/id"),
    Some("deploy"),
    Some("-o StrictHostKeyChecking=no -o 'ProxyCommand=nc %h 22'"),
)]
#[case::local(json!({"ansible_connection": "local"}), None, None, None, None, None)]
#[case::delegate_and_escalation(
    json!({"ansible_connection": "local"}),
    Some(("sudo", "root")),
    Some("jump"),
    None,
    None,
    None,
)]
fn the_link_facts_follow_the_transport(
    #[case] vars: Value,
    #[case] escalation: Option<(&str, &str)>,
    #[case] delegate: Option<&str>,
    #[case] key: Option<&str>,
    #[case] user: Option<&str>,
    #[case] common: Option<&str>,
) {
    let vars = vars.as_object().expect("a map").clone();
    let transport =
        crate::transport::Transport::for_vars("h1", &vars, &crate::testing::defaults()).unwrap();
    let escalation = escalation.map(|(method, user)| crate::transport::Escalation {
        method: crate::transport::BecomeMethod::named(method).unwrap(),
        user: user.to_string(),
        password: None,
        exe: Vec::new(),
        flags: None,
        prompt_l10n: Vec::new(),
    });
    let delegated = delegate.map(|name| (name.to_string(), HostVars::default()));
    let facts = LinkFacts::of(&transport, escalation.as_ref(), delegated.as_ref());
    assert_eq!(facts.private_key.as_deref(), key);
    assert_eq!(facts.remote_user.as_deref(), user);
    assert_eq!(facts.ssh_common_args.as_deref(), common);
    assert_eq!(facts.ssh_extra_args, None);
    assert_eq!(
        facts.escalation,
        escalation.map(|e| (e.method.name().to_string(), e.user))
    );
    assert_eq!(facts.delegate.as_deref(), delegate);
}
