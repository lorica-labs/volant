// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end runs over `kubernetes.core.kubectl` against a real cluster. Ignored by default:
//! they need `kubectl` reaching a cluster (`KUBECONFIG`, else `~/.kube/config`), kubernetes.core on
//! `ANSIBLE_COLLECTIONS_PATH`, and `VOLANT_AGENT_DIR` holding `volant-agent-<triple>`.
//! `VOLANT_KUBECTL_TEST_IMAGE` picks the pods' image, `python:3.12-alpine` by default.
#![cfg(target_os = "linux")]

use std::fmt::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// A namespace of this test's own, deleted with its pods when the test ends.
struct Namespace(String);

impl Namespace {
    fn new(name: &str) -> Namespace {
        let name = format!("volant-e2e-{name}-{}", std::process::id());
        kubectl(&["create", "namespace", &name]);
        Namespace(name)
    }

    /// A pod running `command`, waited for until it is ready or, for any other `phase`, until
    /// it reaches that phase.
    fn pod(&self, name: &str, command: &[&str], phase: &str) {
        let image = std::env::var("VOLANT_KUBECTL_TEST_IMAGE")
            .unwrap_or_else(|_| "python:3.12-alpine".into());
        let mut args = vec![
            "run",
            name,
            "-n",
            &self.0,
            "--image",
            &image,
            "--restart=Never",
            "--command",
            "--",
        ];
        args.extend(command);
        kubectl(&args);
        let condition = match phase {
            "Ready" => "--for=condition=Ready".to_string(),
            other => format!("--for=jsonpath={{.status.phase}}={other}"),
        };
        kubectl(&[
            "wait",
            "-n",
            &self.0,
            &condition,
            &format!("pod/{name}"),
            "--timeout=120s",
        ]);
    }

    fn processes(&self, pod: &str) -> String {
        let out = Command::new("kubectl")
            .args(["exec", "-n", &self.0, pod, "--", "ps", "-o", "args"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Namespace {
    fn drop(&mut self) {
        let _ = Command::new("kubectl")
            .args(["delete", "namespace", &self.0, "--wait=false"])
            .output();
    }
}

fn kubectl(args: &[&str]) {
    let out = Command::new("kubectl").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "kubectl {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write_play(dir: &Path, hosts: &[(&str, String)], tasks: &str) {
    let mut inventory = String::from("all:\n  hosts:\n");
    for (host, vars) in hosts {
        let _ = writeln!(
            inventory,
            "    {host}: {{ansible_connection: kubectl, {vars}}}"
        );
    }
    std::fs::write(dir.join("inventory.yml"), inventory).unwrap();
    std::fs::write(
        dir.join("site.yml"),
        format!("- hosts: all\n  gather_facts: false\n  tasks:\n{tasks}"),
    )
    .unwrap();
}

fn volant(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_volant"));
    command
        .args(["playbook", "-i", "inventory.yml", "site.yml"])
        .current_dir(dir);
    command
}

fn play(dir: &Path, hosts: &[(&str, String)], tasks: &str) -> Output {
    write_play(dir, hosts, tasks);
    volant(dir).output().expect("volant runs")
}

const WHO_AM_I: &str = "    - ansible.builtin.command: id -un\n      register: who\n    - ansible.builtin.debug:\n        var: who.stdout\n";

/// The host's name is the pod, or `ansible_kubectl_pod` names it; a kubeconfig given as a
/// mapping goes through a private file, one link for every task, removed at the end of the run;
/// `-vvvv` shows the plugin's lines.
#[test]
#[ignore = "needs a cluster, kubernetes.core and the agent"]
fn kubectl_runs_a_play_in_a_pod() {
    let ns = Namespace::new("run");
    ns.pod("app1", &["sleep", "infinity"], "Ready");
    let dir = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    // Where `kubectl` itself looks: `KUBECONFIG`, else `~/.kube/config`, which `kind` writes.
    let kubeconfig = std::env::var("KUBECONFIG").unwrap_or_else(|_| {
        format!(
            "{}/.kube/config",
            std::env::var("HOME").expect("HOME is set")
        )
    });
    write_play(
        dir.path(),
        &[
            ("app1", format!("ansible_kubectl_namespace: {}", ns.0)),
            (
                "other",
                format!(
                    "ansible_kubectl_namespace: {}, ansible_kubectl_pod: app1",
                    ns.0
                ),
            ),
            (
                "mapped",
                format!(
                    "ansible_kubectl_namespace: {}, ansible_kubectl_pod: app1, ansible_kubectl_kubeconfig: \"{{{{ lookup('file', '{kubeconfig}') | from_yaml }}}}\"",
                    ns.0
                ),
            ),
        ],
        &format!(
            "{WHO_AM_I}    - ansible.builtin.command: 'true'
"
        ),
    );
    let out = volant(dir.path())
        .arg("-vvvv")
        .env("TMPDIR", tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout.matches("\"who.stdout\": \"root\"").count(),
        3,
        "{stdout}"
    );
    for host in ["app1", "other", "mapped"] {
        let established = format!("<{host}> ESTABLISH kubectl CONNECTION");
        assert_eq!(stdout.matches(&established).count(), 1, "{stdout}");
    }
    assert!(stdout.contains("<other> EXEC ['"), "{stdout}");
    assert!(stdout.contains("'--kubeconfig', '"), "{stdout}");
    let left: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with("volant-kubeconfig-"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

/// The inode and modification time of the agent the first run cached under root's `remote_tmp`:
/// an upload writes a new copy and renames it over the old one, so a second run that uploaded
/// nothing leaves both as they were.
const CACHED_AGENT: &str = "stat -c '%i %Y' /root/.ansible/tmp/volant-agent-*/volant-agent";

/// `command` run to completion, its stdout trimmed.
fn ran(command: &[&str]) -> String {
    let out = Command::new(command[0])
        .args(&command[1..])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A second run finds the agent the first one cached and uploads nothing.
#[test]
#[ignore = "needs a cluster, kubernetes.core and the agent"]
fn kubectl_reuses_the_cached_agent_on_a_second_run() {
    let ns = Namespace::new("cache");
    ns.pod("app1", &["sleep", "infinity"], "Ready");
    let dir = tempfile::tempdir().unwrap();
    let cached = || {
        ran(&[
            "kubectl",
            "exec",
            "-n",
            &ns.0,
            "app1",
            "--",
            "sh",
            "-c",
            CACHED_AGENT,
        ])
    };
    let mut before = String::new();
    for round in 0..2 {
        let out = play(
            dir.path(),
            &[("app1", format!("ansible_kubectl_namespace: {}", ns.0))],
            "    - ansible.builtin.command: id -un\n",
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        if round == 0 {
            before = cached();
            assert!(!before.is_empty(), "no cached agent");
        }
    }
    assert_eq!(cached(), before, "the second run uploaded the agent again");
}

/// A pod or a namespace that does not exist, and a pod that has finished, leave the host
/// unreachable with what `kubectl` said.
#[test]
#[ignore = "needs a cluster, kubernetes.core and the agent"]
fn kubectl_reports_what_it_cannot_reach() {
    let ns = Namespace::new("refused");
    ns.pod("done", &["true"], "Succeeded");
    let dir = tempfile::tempdir().unwrap();
    let out = play(
        dir.path(),
        &[
            ("nosuch", format!("ansible_kubectl_namespace: {}", ns.0)),
            ("done", format!("ansible_kubectl_namespace: {}", ns.0)),
            (
                "nons",
                "ansible_kubectl_namespace: volant-e2e-no-such-namespace".to_string(),
            ),
        ],
        "    - ansible.builtin.ping:\n",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for (host, said) in [
        ("nosuch", r#"pods \"nosuch\" not found"#),
        ("done", "completed pod"),
        (
            "nons",
            r#"namespaces \"volant-e2e-no-such-namespace\" not found"#,
        ),
    ] {
        let line = stdout
            .lines()
            .find(|line| line.starts_with(&format!("fatal: [{host}]: UNREACHABLE!")))
            .unwrap_or_else(|| panic!("no unreachable line for {host}: {stdout}"));
        assert!(line.contains(said), "{host}: {line}");
    }
}

/// The controller killed with SIGKILL closes `kubectl exec`'s stdin, and the agent behind it
/// stops on end of input.
#[test]
#[ignore = "needs a cluster, kubernetes.core and the agent"]
fn kubectl_leaves_no_agent_behind_a_killed_controller() {
    let ns = Namespace::new("killed");
    ns.pod("app1", &["sleep", "infinity"], "Ready");
    let dir = tempfile::tempdir().unwrap();
    write_play(
        dir.path(),
        &[("app1", format!("ansible_kubectl_namespace: {}", ns.0))],
        "    - ansible.builtin.command: sleep 30\n",
    );
    let mut controller = volant(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("volant runs");
    let started = Instant::now();
    while !ns.processes("app1").contains("volant-agent") {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the agent never started"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    controller.kill().unwrap();
    let _ = controller.wait();
    let killed = Instant::now();
    while ns.processes("app1").contains("volant-agent") {
        assert!(
            killed.elapsed() < Duration::from_secs(5),
            "an agent outlived its controller"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A link left idle for ten minutes still carries the next task.
#[test]
#[ignore = "needs a cluster, kubernetes.core and the agent; takes ten minutes"]
fn kubectl_link_survives_ten_idle_minutes() {
    let ns = Namespace::new("idle");
    ns.pod("app1", &["sleep", "infinity"], "Ready");
    let dir = tempfile::tempdir().unwrap();
    let out = play(
        dir.path(),
        &[("app1", format!("ansible_kubectl_namespace: {}", ns.0))],
        "    - ansible.builtin.command: 'true'\n    - ansible.builtin.pause:\n        minutes: 10\n    - ansible.builtin.command: 'true'\n",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("ok=3"), "{stdout}");
}
