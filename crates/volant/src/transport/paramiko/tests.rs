// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::Duration;

use rstest::rstest;
use serde_json::{Value, json};

use super::{PLUGINS, ParamikoSettings};
use crate::transport::{ConnectionDefaults, ConnectionKeywords, SshTarget, Transport};

fn defaults(settings: ParamikoSettings) -> ConnectionDefaults {
    let mut defaults = crate::testing::defaults();
    defaults.plugins.paramiko = settings;
    defaults
}

fn target_with(
    vars: Value,
    defaults: &ConnectionDefaults,
    keywords: &ConnectionKeywords,
) -> SshTarget {
    let mut vars = vars.as_object().unwrap().clone();
    vars.entry("ansible_connection")
        .or_insert_with(|| json!("paramiko_ssh"));
    match Transport::for_task("web1", &vars, defaults, keywords).unwrap() {
        Transport::Ssh(target) => target,
        other => panic!("an ssh target, got {other:?}"),
    }
}

fn target(vars: Value, settings: ParamikoSettings) -> SshTarget {
    target_with(vars, &defaults(settings), &ConnectionKeywords::default())
}

/// The options between the connection timeout and the address, as one line.
fn options(target: &SshTarget) -> String {
    let argv = target.ssh_argv("true");
    let from = argv
        .iter()
        .position(|w| w.starts_with("ConnectTimeout="))
        .unwrap()
        + 1;
    let to = argv.iter().position(|w| w == "web1").unwrap();
    argv[from..to].join(" ")
}

#[rstest]
#[case::nothing_set(json!({}), ParamikoSettings::default(), "-F /dev/null")]
#[case::the_ssh_options(
    json!({
        "ansible_paramiko_user": "ops",
        "ansible_paramiko_port": 2222,
        "ansible_paramiko_private_key_file": "/keys/id",
    }),
    ParamikoSettings::default(),
    "-p 2222 -i /keys/id -F /dev/null -l ops"
)]
#[case::no_host_key_checking(
    json!({"ansible_paramiko_host_key_checking": false}),
    ParamikoSettings { host_key_auto_add: Some(true), record_host_keys: Some(false), ..Default::default() },
    "-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -F /dev/null"
)]
#[case::host_key_auto_add(
    json!({}),
    ParamikoSettings { host_key_auto_add: Some(true), ..Default::default() },
    "-F /dev/null -o StrictHostKeyChecking=accept-new"
)]
#[case::no_recorded_host_keys(
    json!({}),
    ParamikoSettings { record_host_keys: Some(false), ..Default::default() },
    "-F /dev/null -o UserKnownHostsFile=/dev/null ~/.ssh/known_hosts ~/.ssh/known_hosts2"
)]
#[case::no_look_for_keys_with_a_key(
    json!({"ansible_private_key_file": "/keys/id"}),
    ParamikoSettings { look_for_keys: Some(false), ..Default::default() },
    "-i /keys/id -F /dev/null -o IdentitiesOnly=yes"
)]
#[case::no_look_for_keys_without_a_key(
    json!({}),
    ParamikoSettings { look_for_keys: Some(false), ..Default::default() },
    "-F /dev/null"
)]
#[case::no_rsa_sha2(
    json!({"ansible_paramiko_use_rsa_sha2_algorithms": "no"}),
    ParamikoSettings::default(),
    "-F /dev/null -o HostKeyAlgorithms=+ssh-rsa -o PubkeyAcceptedAlgorithms=+ssh-rsa"
)]
#[case::proxy_command(
    json!({"ansible_paramiko_proxy_command": "ssh -W %h:%p jump"}),
    ParamikoSettings { proxy_command: Some("ignored".into()), ..Default::default() },
    "-F /dev/null -o ProxyCommand=ssh -W %h:%p jump"
)]
#[case::an_empty_proxy_command(
    json!({}),
    ParamikoSettings { proxy_command: Some(String::new()), ..Default::default() },
    "-F /dev/null"
)]
#[case::a_password(
    json!({"ansible_paramiko_pass": "secret"}),
    ParamikoSettings::default(),
    "-o NumberOfPasswordPrompts=1 -F /dev/null"
)]
fn each_option_becomes_its_ssh_option(
    #[case] vars: Value,
    #[case] settings: ParamikoSettings,
    #[case] want: &str,
) {
    assert_eq!(options(&target(vars, settings)), want);
}

#[test]
fn every_name_of_the_plugin_runs_on_ssh() {
    for name in PLUGINS {
        let target = target(
            json!({"ansible_connection": name}),
            ParamikoSettings::default(),
        );
        assert!(target.own.paramiko, "{name}");
        assert_eq!(target.own.executable, "ssh", "{name}");
        assert!(target.extra_args.is_empty(), "{name}");
    }
    let vars = json!({"ansible_connection": "ssh"});
    assert!(!target(vars, ParamikoSettings::default()).own.paramiko);
}

/// The plugin reads the `ssh` names too, its own beating them, and none of the `ssh` plugin's
/// other settings: `ansible_timeout`, the `ssh` arguments and `ansible_ssh_executable`.
#[test]
fn the_plugin_s_own_variables_beat_the_ssh_ones() {
    let target = target(
        json!({
            "ansible_host": "a",
            "ansible_paramiko_host": "b",
            "ansible_ssh_user": "ssh-user",
            "ansible_ssh_timeout": 7,
            "ansible_ssh_common_args": "-o A=1",
            "ansible_ssh_extra_args": "-o B=2",
            "ansible_ssh_executable": "/opt/ssh",
        }),
        ParamikoSettings::default(),
    );
    assert_eq!(target.address, "b");
    assert_eq!(target.user.as_deref(), Some("ssh-user"));
    assert_eq!(target.connect_timeout, Duration::from_secs(7));
    assert_eq!(target.common_args, ["-F", "/dev/null"]);
    assert_eq!(target.own.executable, "ssh");
    let plain = target_with(
        json!({"ansible_timeout": 7}),
        &defaults(ParamikoSettings::default()),
        &ConnectionKeywords::default(),
    );
    assert_eq!(plain.connect_timeout, Duration::from_secs(10));
}

/// A host variable beats the task's keyword, which beats `ansible.cfg` and the environment, the
/// plugin's own setting beating the generic one.
#[test]
fn the_settings_apply_when_the_host_is_silent() {
    let settings = ParamikoSettings {
        remote_user: Some("cfg-user".into()),
        port: Some(2200),
        defaults_port: Some(2300),
        host_key_checking: Some(false),
        pty: Some(false),
        timeout: Some(Duration::from_secs(4)),
        ssh_timeout: Some(Duration::from_secs(5)),
        ..Default::default()
    };
    let silent = target(json!({}), settings.clone());
    assert_eq!(silent.user.as_deref(), Some("cfg-user"));
    assert_eq!(silent.port, Some(2200));
    assert!(!silent.host_key_checking);
    assert!(!silent.use_tty);
    assert_eq!(silent.connect_timeout, Duration::from_secs(4));
    let keywords = ConnectionKeywords {
        user: Some("kw-user".into()),
        port: Some(2100),
        ..Default::default()
    };
    let with_keywords = target_with(json!({}), &defaults(settings.clone()), &keywords);
    assert_eq!(with_keywords.user.as_deref(), Some("kw-user"));
    assert_eq!(with_keywords.port, Some(2100));
    let fallback = target(
        json!({"ansible_host_key_checking": true}),
        ParamikoSettings {
            port: None,
            timeout: None,
            ..settings
        },
    );
    assert_eq!(fallback.port, Some(2300));
    assert_eq!(fallback.connect_timeout, Duration::from_secs(5));
    assert!(fallback.host_key_checking);
}

/// `ansible.builtin.paramiko` loads nothing in ansible-core 2.19.12, so it stays refused.
#[test]
fn the_name_ansible_core_does_not_load_is_refused() {
    let mut vars = serde_json::Map::new();
    vars.insert(
        "ansible_connection".into(),
        json!("ansible.builtin.paramiko"),
    );
    let err =
        Transport::for_vars("web1", &vars, &defaults(ParamikoSettings::default())).unwrap_err();
    assert_eq!(
        err.to_string(),
        "host 'web1': connection 'ansible.builtin.paramiko' is not supported"
    );
}
