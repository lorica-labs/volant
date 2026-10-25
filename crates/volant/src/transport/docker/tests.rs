// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::Duration;

use rstest::rstest;
use serde_json::{Value, json};

use super::{DockerSettings, Options, exec_argv, extra_env, inspect_argv, program};
use crate::transport::{ConnectionDefaults, ConnectionKeywords, Transport};

fn words(argv: &[String]) -> String {
    argv.join(" ")
}

#[test]
fn the_exec_command_line_keeps_the_plugin_s_order() {
    let options = Options {
        container: "app1".into(),
        user: Some("nobody".into()),
        extra_args: Some("--log-level debug".into()),
        env: vec![("FOO".into(), "bar".into()), ("A".into(), "b c".into())],
        working_dir: Some("/tmp".into()),
        privileged: true,
    };
    assert_eq!(
        exec_argv("/usr/bin/docker", &options),
        [
            "/usr/bin/docker",
            "--log-level",
            "debug",
            "exec",
            "-u",
            "nobody",
            "-e",
            "FOO=bar",
            "-e",
            "A=b c",
            "-w",
            "/tmp",
            "--privileged",
            "-i",
            "app1"
        ]
    );
    assert_eq!(
        words(&inspect_argv("docker", &options)),
        "docker --log-level debug inspect --format {{.Config.User}} app1"
    );
}

#[rstest]
#[case("--tls  --host x", &["--tls", "", "--host", "x"])]
#[case("'--host' x", &["'--host'", "x"])]
#[case("", &[])]
fn the_extra_arguments_are_split_on_single_spaces(#[case] args: &str, #[case] want: &[&str]) {
    let options = Options {
        container: "c".into(),
        extra_args: Some(args.into()),
        ..Options::default()
    };
    let argv = exec_argv("docker", &options);
    assert_eq!(&argv[1..argv.len() - 3], want);
}

#[rstest]
#[case(json!({"N": 1}), "Non-string value found for extra_env option. Ambiguous env options must be wrapped in quotes to avoid them being interpreted. Value: 1")]
#[case(json!({"N": true}), "Non-string value found for extra_env option. Ambiguous env options must be wrapped in quotes to avoid them being interpreted. Value: True")]
#[case(json!("FOO=bar"), "Config 'extra_env' for 'ansible_collections.community.docker.plugins.connection.docker' connection plugin from 'var: ansible_docker_extra_env' has an invalid value: Invalid value provided for 'dict': 'FOO=bar'")]
fn an_extra_env_that_is_not_text_to_text_is_refused_in_the_plugin_s_words(
    #[case] value: Value,
    #[case] want: &str,
) {
    assert_eq!(extra_env(Some(&value)).unwrap_err(), want);
}

/// A directory with an executable `docker` in it, put first on `PATH` for this test's process.
fn fake_docker() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let docker = dir.path().join("docker");
    std::fs::write(&docker, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

fn target(
    vars: Value,
    defaults: &ConnectionDefaults,
) -> anyhow::Result<crate::transport::ExecTarget> {
    let mut vars = vars.as_object().unwrap().clone();
    vars.insert(
        "ansible_connection".into(),
        json!("community.docker.docker"),
    );
    match Transport::for_task("h1", &vars, defaults, &ConnectionKeywords::default())? {
        Transport::Exec(target) => Ok(target),
        other => panic!("an exec target, got {other:?}"),
    }
}

#[test]
fn each_option_is_read_from_its_variables_over_the_settings() {
    let dir = fake_docker();
    let path = std::env::join_paths([dir.path()]).unwrap();
    // nextest gives each test a process of its own, so no other thread reads the variable.
    unsafe { std::env::set_var("PATH", &path) };
    let docker = dir.path().join("docker").display().to_string();
    let defaults = ConnectionDefaults {
        remote_user: Some("from-cli".into()),
        docker: DockerSettings {
            extra_cli_args: Some("--host unix:///cfg".into()),
            privileged: Some(true),
            working_dir: Some("/cfg".into()),
            timeout: Some(Duration::from_secs(30)),
        },
        ..crate::testing::defaults()
    };
    let from_settings = target(json!({}), &defaults).unwrap();
    assert_eq!(
        from_settings.prefix,
        [
            &docker,
            "--host",
            "unix:///cfg",
            "exec",
            "-u",
            "from-cli",
            "-w",
            "/cfg",
            "--privileged",
            "-i",
            "h1"
        ]
    );
    assert_eq!(from_settings.connect_timeout, Duration::from_secs(30));
    assert_eq!(from_settings.user_probe, None);
    let from_vars = target(
        json!({
            "ansible_host": "by-host",
            "ansible_docker_host": "by-docker-host",
            "ansible_user": "plain",
            "ansible_docker_user": "docker-user",
            "ansible_docker_extra_args": "--debug",
            "ansible_docker_extra_env": {"K": "v"},
            "ansible_docker_privileged": "no",
            "ansible_docker_working_dir": "/srv",
            "ansible_docker_timeout": "7",
        }),
        &defaults,
    )
    .unwrap();
    assert_eq!(
        from_vars.prefix,
        [
            &docker,
            "--debug",
            "exec",
            "-u",
            "docker-user",
            "-e",
            "K=v",
            "-w",
            "/srv",
            "-i",
            "by-docker-host"
        ]
    );
    assert_eq!(from_vars.name, "by-docker-host");
    assert_eq!(from_vars.connect_timeout, Duration::from_secs(7));
    let bare = target(
        json!({"ansible_host": "by-host"}),
        &crate::testing::defaults(),
    )
    .unwrap();
    assert_eq!(bare.prefix, [&docker, "exec", "-i", "by-host"]);
    assert_eq!(
        bare.user_probe.as_deref().map(words),
        Some(format!(
            "{docker} inspect --format {{{{.Config.User}}}} by-host"
        ))
    );
}

#[test]
fn a_controller_without_docker_says_so() {
    assert_eq!(program(Some(std::ffi::OsStr::new("/nonexistent"))), None);
    let dir = fake_docker();
    assert_eq!(
        program(Some(
            &std::env::join_paths(["/nonexistent".as_ref(), dir.path()]).unwrap()
        )),
        Some(dir.path().join("docker"))
    );
}
