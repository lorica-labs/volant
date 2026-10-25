// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end runs over `community.docker.docker` against real containers. Ignored by default:
//! they need a `docker` that runs without a password, community.docker on
//! `ANSIBLE_COLLECTIONS_PATH`, and `VOLANT_AGENT_DIR` holding `volant-agent-<triple>` for the
//! image's architecture. `VOLANT_DOCKER_TEST_IMAGE` picks the image, `alpine:3.20` by default,
//! and `VOLANT_DOCKER_TEST_NO_SHELL_IMAGE` the image with no `/bin/sh`, distroless's
//! `static-debian12:debug-nonroot` by default, whose busybox lives in `/busybox`.
#![cfg(target_os = "linux")]

use std::fmt::Write as _;
use std::path::Path;
use std::process::{Command, Output};

/// A container of this test's own, removed when the test ends however it ends.
struct Container(String);

impl Container {
    fn running(name: &str) -> Container {
        Container::made(name, &["run", "-d"])
    }

    fn stopped(name: &str) -> Container {
        Container::made(name, &["create"])
    }

    /// A running container of an image with no shell, kept up by busybox's `sleep`.
    fn without_a_shell(name: &str) -> Container {
        let image = std::env::var("VOLANT_DOCKER_TEST_NO_SHELL_IMAGE")
            .unwrap_or_else(|_| "gcr.io/distroless/static-debian12:debug-nonroot".into());
        Container::from_image(
            name,
            &["run", "-d", "--entrypoint", "/busybox/sleep"],
            &image,
        )
    }

    fn made(name: &str, how: &[&str]) -> Container {
        let image =
            std::env::var("VOLANT_DOCKER_TEST_IMAGE").unwrap_or_else(|_| "alpine:3.20".into());
        Container::from_image(name, how, &image)
    }

    fn from_image(name: &str, how: &[&str], image: &str) -> Container {
        let name = format!("volant-e2e-{name}-{}", std::process::id());
        let entrypoint_given = how.contains(&"--entrypoint");
        let out = Command::new("docker")
            .args(how)
            .args(["--name", &name, image])
            .args(if entrypoint_given {
                &["infinity"][..]
            } else {
                &["sleep", "infinity"][..]
            })
            .output()
            .expect("docker runs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Container(name)
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}

/// Runs `site.yml` from `dir` against `hosts`, each line of it the host's inventory variables.
fn play(dir: &Path, hosts: &[(&str, &str)], tasks: &str) -> Output {
    let mut inventory = String::from("all:\n  hosts:\n");
    for (host, vars) in hosts {
        let _ = writeln!(
            inventory,
            "    {host}: {{ansible_connection: docker, {vars}}}"
        );
    }
    std::fs::write(dir.join("inventory.yml"), inventory).unwrap();
    std::fs::write(
        dir.join("site.yml"),
        format!("- hosts: all\n  gather_facts: false\n  tasks:\n{tasks}"),
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(["playbook", "-i", "inventory.yml", "site.yml"])
        .current_dir(dir)
        .output()
        .expect("volant runs")
}

const WHO_AM_I: &str = r"    - name: Who am I
      ansible.builtin.command: id -un
      register: who

    - name: Show it
      ansible.builtin.debug:
        var: who.stdout
";

/// The minimal play of community.docker's documentation, which ansible-core 2.19.12 answers
/// with `root` and `ok=2 changed=1`.
#[test]
#[ignore = "needs docker, community.docker and a musl agent"]
fn docker_runs_a_play_in_a_running_container() {
    let app = Container::running("app");
    let dir = tempfile::tempdir().unwrap();
    let out = play(
        dir.path(),
        &[("app1", &format!("ansible_docker_host: {}", app.0))],
        WHO_AM_I,
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("TASK [Who am I] ****"), "{stdout}");
    assert!(stdout.contains("changed: [app1]"), "{stdout}");
    assert!(stdout.contains("\"who.stdout\": \"root\""), "{stdout}");
    assert!(
        stdout.contains(
            "app1                       : ok=2    changed=1    unreachable=0    failed=0"
        ),
        "{stdout}"
    );
}

/// A container that does not exist, one that is stopped, and a user the image does not have
/// each make their host unreachable in docker's own words, and the run goes on to the others.
#[test]
#[ignore = "needs docker, community.docker and a musl agent"]
fn docker_reports_what_it_cannot_reach_in_its_own_words() {
    let app = Container::running("reach");
    let stopped = Container::stopped("stopped");
    let dir = tempfile::tempdir().unwrap();
    let out = play(
        dir.path(),
        &[
            ("gone", "ansible_docker_host: volant-e2e-no-such-container"),
            ("stopped", &format!("ansible_docker_host: {}", stopped.0)),
            (
                "nobody_here",
                &format!(
                    "ansible_docker_host: {}, ansible_user: volant-no-such-user",
                    app.0
                ),
            ),
            ("fine", &format!("ansible_docker_host: {}", app.0)),
        ],
        "    - ansible.builtin.command: id -un\n",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for (host, said) in [
        ("gone", "No such container: volant-e2e-no-such-container"),
        ("stopped", "is not running"),
        ("nobody_here", "volant-no-such-user"),
    ] {
        let line = stdout
            .lines()
            .find(|line| line.starts_with(&format!("fatal: [{host}]: UNREACHABLE!")))
            .unwrap_or_else(|| panic!("no unreachable line for {host}: {stdout}"));
        assert!(line.contains(said), "{host}: {line}");
    }
    assert!(stdout.contains("changed: [fine]"), "{stdout}");
}

/// An image with no shell gets the agent through `docker cp`, and a second run reuses the agent
/// it placed.
#[test]
#[ignore = "needs docker, community.docker and a musl agent"]
fn docker_reaches_a_container_with_no_shell() {
    let app = Container::without_a_shell("noshell");
    let dir = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let out = play(
            dir.path(),
            &[("bare", &format!("ansible_docker_host: {}", app.0))],
            "    - ansible.builtin.command: /busybox/whoami
      register: who
    - ansible.builtin.debug:
        var: who.stdout
",
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(stdout.contains("\"who.stdout\": \"nonroot\""), "{stdout}");
    }
}
