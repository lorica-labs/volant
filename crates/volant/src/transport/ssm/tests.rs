// SPDX-License-Identifier: GPL-3.0-or-later

use std::fmt::Write as _;
use std::time::Duration;

use serde_json::{Value, json};

use super::{Shell, SsmTarget};
use crate::transport::{ConnectionKeywords, Transport};

/// The target of `vars`, with a `session-manager-plugin` on `PATH`.
fn target(vars: Value) -> SsmTarget {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session-manager-plugin");
    std::fs::write(&file, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    // nextest gives each test a process of its own, so no other thread reads the variables.
    unsafe { std::env::set_var("PATH", dir.path()) };
    let mut vars = vars.as_object().unwrap().clone();
    vars.insert("ansible_connection".into(), json!("amazon.aws.aws_ssm"));
    match Transport::for_task(
        "web1",
        &vars,
        &crate::testing::defaults(),
        &ConnectionKeywords::default(),
    )
    .unwrap()
    {
        Transport::Ssm(target) => *target,
        other => panic!("an ssm target, got {other:?}"),
    }
}

#[test]
fn the_instance_is_the_address_unless_its_own_variable_names_one() {
    let plain = target(json!({}));
    assert_eq!(plain.instance, "web1");
    assert_eq!(plain.region, "us-east-1");
    assert!(plain.plugin.ends_with("/session-manager-plugin"));
    assert!(plain.unavailable.is_none());
    assert_eq!(plain.connect_timeout, Duration::from_secs(60));
    let named = target(json!({
        "ansible_host": "i-0abc",
        "ansible_aws_ssm_instance_id": "i-0def",
        "ansible_aws_ssm_region": "eu-north-1",
        "ansible_aws_ssm_profile": "tests",
        "ansible_aws_ssm_document": "MyShell",
        "ansible_aws_ssm_timeout": "30",
    }));
    assert_eq!(named.name, "i-0abc");
    assert_eq!(named.instance, "i-0def");
    assert_eq!(named.region, "eu-north-1");
    assert_eq!(named.profile.as_deref(), Some("tests"));
    assert_eq!(named.document.as_deref(), Some("MyShell"));
    assert_eq!(named.connect_timeout, Duration::from_secs(30));
}

#[test]
fn credentials_come_from_variables_then_the_environment_and_never_print() {
    unsafe {
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKIAFROMENV");
        std::env::set_var("AWS_DEFAULT_REGION", "eu-west-3");
        std::env::set_var("AWS_REGION", "eu-west-1");
    }
    let target = target(json!({"ansible_aws_ssm_secret_access_key": "s3cr3t"}));
    let carried: Vec<(&str, &str)> = target
        .credentials
        .iter()
        .map(|(name, secret)| (*name, secret.expose()))
        .collect();
    assert_eq!(
        carried,
        [
            ("VOLANT_SSM_ACCESS_KEY_ID", "AKIAFROMENV"),
            ("VOLANT_SSM_SECRET_ACCESS_KEY", "s3cr3t"),
        ]
    );
    assert_eq!(target.region, "eu-west-3");
    let printed = format!("{target:?}");
    assert!(!printed.contains("s3cr3t") && !printed.contains("AKIAFROMENV"));
}

#[test]
fn no_session_manager_plugin_makes_the_host_unreachable_with_the_plugin_s_words() {
    unsafe { std::env::set_var("PATH", "/nonexistent") };
    let mut vars = serde_json::Map::new();
    vars.insert("ansible_connection".into(), json!("amazon.aws.aws_ssm"));
    let Transport::Ssm(target) = Transport::for_task(
        "web1",
        &vars,
        &crate::testing::defaults(),
        &ConnectionKeywords::default(),
    )
    .unwrap() else {
        panic!("an ssm target")
    };
    if std::path::Path::new("/usr/local/bin/session-manager-plugin").is_file() {
        return;
    }
    assert_eq!(
        target.unavailable.as_deref(),
        Some("session-manager-plugin command not found in PATH")
    );
}

/// A stand-in for a session: the plugin's first line, then a shell reading its stdin, which is
/// not a terminal, so `stty` complains on a stderr nobody reads.
fn fake_session() -> tokio::process::Command {
    let mut command = tokio::process::Command::new("sh");
    command
        .args([
            "-c",
            "echo 'Starting session with SessionId: test-0'; exec sh",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    command
}

#[tokio::test]
async fn each_command_says_its_output_its_stderr_and_its_status_apart() {
    let child = fake_session().spawn().unwrap();
    let mut shell = Shell::open(child, Duration::from_secs(10)).await.unwrap();
    let ran = shell
        .run("echo hi; echo 'it went wrong' >&2; exit 3", None)
        .await
        .unwrap();
    assert_eq!(ran.stdout, "hi\n");
    assert_eq!(ran.stderr, "it went wrong\n");
    assert_eq!(ran.code, Some(3));
    let payload: Vec<u8> = (0..=255u8).collect();
    let read = shell
        .run(
            "head -c 256 | od -An -v -tx1 | tr -d ' \\n'",
            Some(&payload),
        )
        .await
        .unwrap();
    let hex = payload.iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    });
    assert_eq!(read.stdout, hex);
    assert_eq!(read.code, Some(0));
    let quiet = shell.run("printf 'no newline'", None).await.unwrap();
    assert_eq!((quiet.stdout.as_str(), quiet.code), ("no newline", Some(0)));
}

#[tokio::test]
async fn a_session_that_never_starts_says_what_the_starter_said() {
    let mut command = tokio::process::Command::new("sh");
    command
        .args([
            "-c",
            "echo 'An error occurred (TargetNotConnected) when calling the StartSession operation: i-0 is not connected.' >&2; exit 1",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let Err(err) = Shell::open(command.spawn().unwrap(), Duration::from_secs(10)).await else {
        panic!("no session")
    };
    assert_eq!(
        err.to_string(),
        "An error occurred (TargetNotConnected) when calling the StartSession operation: i-0 is not connected."
    );
}
