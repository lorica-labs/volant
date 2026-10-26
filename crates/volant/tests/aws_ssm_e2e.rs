// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end runs over `amazon.aws.aws_ssm` against a real EC2 instance. Ignored by default, and
//! never run in CI, which has no AWS account. They need:
//! - `VOLANT_SSM_TEST_INSTANCE`, the id of an instance the SSM agent manages, whose user can
//!   `sudo` without a password, as `ssm-user` can on Amazon Linux;
//! - `VOLANT_SSM_TEST_REGION`, its region;
//! - credentials boto3 finds (`AWS_PROFILE`, or the `AWS_*` variables), allowed to start and end
//!   sessions on it;
//! - `session-manager-plugin` on `PATH`, `VOLANT_PYTHON` naming a Python with ansible-core and
//!   boto3, amazon.aws on `ANSIBLE_COLLECTIONS_PATH`, and `VOLANT_AGENT_DIR` holding
//!   `volant-agent-<triple>` for the instance's architecture.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn instance() -> String {
    std::env::var("VOLANT_SSM_TEST_INSTANCE").expect("VOLANT_SSM_TEST_INSTANCE names an instance")
}

fn write_play(dir: &Path, instance: &str, play: &str) {
    let region = std::env::var("VOLANT_SSM_TEST_REGION").expect("VOLANT_SSM_TEST_REGION is set");
    std::fs::write(
        dir.join("inventory.yml"),
        format!(
            "all:\n  hosts:\n    web1:\n      ansible_connection: amazon.aws.aws_ssm\n      \
             ansible_aws_ssm_instance_id: {instance}\n      ansible_aws_ssm_region: {region}\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("site.yml"), play).unwrap();
}

fn volant(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_volant"));
    command
        .args(["playbook", "-i", "inventory.yml", "site.yml"])
        .current_dir(dir);
    command
}

fn play(dir: &Path, instance: &str, play: &str) -> Output {
    write_play(dir, instance, play);
    volant(dir).output().expect("volant runs")
}

const WHO_AM_I: &str = "- hosts: all\n  gather_facts: false\n  tasks:\n    - ansible.builtin.command: id -un\n      register: who\n    - ansible.builtin.debug:\n        var: who.stdout\n    - ansible.builtin.command: id -un\n      become: true\n      register: root\n    - ansible.builtin.debug:\n        var: root.stdout\n";

/// How many agents talk in lines on the instance, the one that counts them included.
fn agents(instance: &str) -> usize {
    let dir = tempfile::tempdir().unwrap();
    let out = play(
        dir.path(),
        instance,
        "- hosts: all\n  gather_facts: false\n  tasks:\n    - ansible.builtin.shell: ps -eo args | grep -c '[v]olant-agent --lines'\n      register: n\n    - ansible.builtin.debug:\n        var: n.stdout\n",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("\"n.stdout\": \""))
        .and_then(|rest| rest.trim_end_matches('"').parse().ok())
        .unwrap_or_else(|| panic!("no count: {stdout}"))
}

/// As the session's user, and as root through `sudo`, then again with the agent cached.
#[test]
#[ignore = "needs an AWS account and an instance"]
fn aws_ssm_runs_a_play_as_the_session_user_and_with_become() {
    let dir = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let out = play(dir.path(), &instance(), WHO_AM_I);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(stdout.contains("\"who.stdout\": \"ssm-user\""), "{stdout}");
        assert!(stdout.contains("\"root.stdout\": \"root\""), "{stdout}");
    }
}

/// An instance the SSM agent does not manage: the host is unreachable with boto3's words.
#[test]
#[ignore = "needs an AWS account"]
fn aws_ssm_reports_an_instance_it_cannot_reach() {
    let dir = tempfile::tempdir().unwrap();
    let out = play(
        dir.path(),
        "i-0123456789abcdef0",
        "- hosts: all\n  gather_facts: false\n  tasks:\n    - ansible.builtin.ping:\n",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .find(|line| line.starts_with("fatal: [web1]: UNREACHABLE!"))
        .unwrap_or_else(|| panic!("no unreachable line: {stdout}"));
    assert!(line.contains("TargetNotConnected"), "{line}");
}

/// The controller killed with SIGKILL mid-task: the session ends and takes the agent with it.
#[test]
#[ignore = "needs an AWS account and an instance"]
fn aws_ssm_leaves_no_agent_behind_a_killed_controller() {
    let instance = instance();
    let dir = tempfile::tempdir().unwrap();
    write_play(
        dir.path(),
        &instance,
        "- hosts: all\n  gather_facts: false\n  tasks:\n    - ansible.builtin.command: sleep 60\n",
    );
    let mut controller = volant(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("volant runs");
    std::thread::sleep(Duration::from_secs(15));
    controller.kill().unwrap();
    let _ = controller.wait();
    let killed = Instant::now();
    while agents(&instance) > 1 {
        assert!(
            killed.elapsed() < Duration::from_secs(60),
            "an agent outlived its controller"
        );
    }
}
