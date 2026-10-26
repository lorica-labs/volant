// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::{Value, json};

use crate::transport::{ConnectionKeywords, ExecTarget, Lines, Transport};

/// The exec target of `vars`, with `kubectl`'s path made a bare name.
fn target(vars: Value) -> ExecTarget {
    let mut target = target_of(vars);
    target.prefix[0] = target.prefix[0].rsplit('/').next().unwrap().to_string();
    target
}

/// The exec target of `vars`, with a `kubectl` on `PATH`.
fn target_of(vars: Value) -> ExecTarget {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("kubectl");
    std::fs::write(&file, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    // nextest gives each test a process of its own, so no other thread reads the variable.
    unsafe { std::env::set_var("PATH", dir.path()) };
    let mut vars = vars.as_object().unwrap().clone();
    vars.insert(
        "ansible_connection".into(),
        json!("kubernetes.core.kubectl"),
    );
    match Transport::for_task(
        "app1",
        &vars,
        &crate::testing::defaults(),
        &ConnectionKeywords::default(),
    )
    .unwrap()
    {
        Transport::Exec(target) => target,
        other => panic!("an exec target, got {other:?}"),
    }
}

#[test]
fn kubectl_runs_in_the_pod_named_by_the_address() {
    let target = target(json!({}));
    assert_eq!(target.prefix, ["kubectl", "exec", "-i", "app1", "--"]);
    assert_eq!(target.name, "app1");
    assert_eq!(target.kind, "kubectl");
    assert_eq!(target.lines, Lines::NoUser);
    assert_eq!(target.user, None);
    assert_eq!(target.shell, "/bin/sh");
    assert!(target.copy.is_none());
    assert!(target.unavailable.is_none());
}

#[test]
fn kubectl_flags_follow_the_plugin_order_and_mask_the_secrets() {
    let target = target(json!({
        "ansible_host": "web",
        "ansible_kubectl_pod": "app2",
        "ansible_kubectl_container": "side",
        "ansible_kubectl_namespace": "demo",
        "ansible_kubectl_extra_args": "--v=0 --request-timeout=5s",
        "ansible_kubectl_kubeconfig": "/k/one",
        "ansible_kubectl_config": "/k/two",
        "ansible_kubectl_context": "ctx",
        "ansible_kubectl_host": "https://a:6443",
        "ansible_kubectl_server": "https://b:6443",
        "ansible_kubectl_username": "bob",
        "ansible_kubectl_password": "pw",
        "ansible_kubectl_token": "tok",
        "ansible_kubectl_cert_file": "/c.crt",
        "ansible_kubectl_client_key": "/c.key",
        "ansible_kubectl_ca_cert": "/ca.crt",
        "ansible_kubectl_validate_certs": true,
        "ansible_executable": "/bin/bash",
    }));
    assert_eq!(
        target.prefix,
        [
            "kubectl",
            "-n",
            "demo",
            "--kubeconfig",
            "/k/two",
            "--context",
            "ctx",
            "--server",
            "https://b:6443",
            "--username",
            "bob",
            "--password",
            "pw",
            "--token",
            "tok",
            "--client-certificate",
            "/c.crt",
            "--client-key",
            "/c.key",
            "--certificate-authority",
            "/ca.crt",
            "--insecure-skip-tls-verify",
            "True",
            "--v=0",
            "--request-timeout=5s",
            "exec",
            "-i",
            "app2",
            "-c",
            "side",
            "--",
        ]
    );
    assert_eq!(target.name, "web");
    assert_eq!(target.secrets, ["pw", "tok"]);
    assert_eq!(target.shell, "/bin/bash");
}

#[test]
fn kubectl_reads_the_environment_below_the_variables() {
    unsafe {
        std::env::set_var("K8S_AUTH_NAMESPACE", "fromenv");
        std::env::set_var("K8S_AUTH_HOST", "https://h:6443");
        std::env::set_var("K8S_AUTH_SERVER", "https://s:6443");
        std::env::set_var("K8S_AUTH_POD", "envpod");
    }
    let target = target(json!({"ansible_kubectl_pod": "varpod"}));
    assert_eq!(
        target.prefix,
        [
            "kubectl",
            "-n",
            "fromenv",
            "--server",
            "https://s:6443",
            "exec",
            "-i",
            "varpod",
            "--"
        ]
    );
}

#[test]
fn kubectl_skips_false_and_passes_other_words_as_they_are() {
    let off = target(json!({"ansible_kubectl_validate_certs": false}));
    assert!(!off.prefix.iter().any(|word| word.contains("insecure")));
    let text = target(json!({"ansible_kubectl_validate_certs": "false"}));
    assert_eq!(text.prefix[1..3], ["--insecure-skip-tls-verify", "false"]);
}

#[test]
fn kubectl_writes_a_mapping_kubeconfig_to_a_private_file() {
    use std::os::unix::fs::PermissionsExt;
    let target = target(json!({"ansible_kubectl_kubeconfig": {"apiVersion": "v1"}}));
    assert_eq!(target.prefix[1], "--kubeconfig");
    let path = std::path::PathBuf::from(&target.prefix[2]);
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text, r#"{"apiVersion":"v1"}"#);
    let mode = path.metadata().unwrap().permissions().mode();
    assert_eq!(mode & 0o077, 0, "mode {mode:o}");
    let again = target_of(json!({"ansible_kubectl_kubeconfig": {"apiVersion": "v1"}}));
    assert_eq!(
        again.prefix[1..],
        target.prefix[1..],
        "one mapping, one file, one link"
    );
    drop(target);
    assert!(path.exists(), "a target still names the file");
    drop(again);
    assert!(!path.exists(), "the file goes with the last target");
}

#[test]
fn kubectl_local_env_vars_take_a_mapping_or_its_json() {
    let mapping = target(json!({"ansible_kubectl_local_env_vars": {"A": "1", "B": 2}}));
    assert_eq!(
        mapping.env,
        [("A".into(), "1".into()), ("B".into(), "2".into())]
    );
    let text = target(json!({"ansible_kubectl_local_env_vars": "{\"C\": \"3\"}"}));
    assert_eq!(text.env, [("C".to_string(), "3".to_string())]);
}

#[test]
fn kubectl_missing_from_path_makes_the_target_unavailable() {
    unsafe { std::env::set_var("PATH", "/nonexistent") };
    let mut vars = serde_json::Map::new();
    vars.insert(
        "ansible_connection".into(),
        json!("kubernetes.core.kubectl"),
    );
    let Transport::Exec(target) = Transport::for_task(
        "app1",
        &vars,
        &crate::testing::defaults(),
        &ConnectionKeywords::default(),
    )
    .unwrap() else {
        panic!("an exec target")
    };
    assert_eq!(
        target.unavailable.as_deref(),
        Some("kubectl command not found in PATH")
    );
}
