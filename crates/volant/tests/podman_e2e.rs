// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end runs over `containers.podman.podman` against real containers, rootless or not.
//! Ignored by default: they need `podman`, containers.podman on `ANSIBLE_COLLECTIONS_PATH`, and
//! `VOLANT_AGENT_DIR` holding `volant-agent-<triple>` for the images' architecture.
//! `VOLANT_PODMAN_TEST_IMAGE` picks the image, `docker.io/library/alpine:3.20` by default, and
//! `VOLANT_PODMAN_TEST_NO_SHELL_IMAGE` the one with no `/bin/sh`.
#![cfg(target_os = "linux")]

use std::fmt::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Container(String);

impl Container {
    fn new(name: &str, args: &[&str]) -> Container {
        let name = format!("volant-e2e-{name}-{}", std::process::id());
        let out = Command::new("podman")
            .args(args.iter().map(|arg| arg.replace("{name}", &name)))
            .output()
            .expect("podman runs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Container(name)
    }

    fn image() -> String {
        std::env::var("VOLANT_PODMAN_TEST_IMAGE")
            .unwrap_or_else(|_| "docker.io/library/alpine:3.20".into())
    }

    fn running(name: &str) -> Container {
        let image = Container::image();
        Container::new(
            name,
            &["run", "-d", "--name", "{name}", &image, "sleep", "infinity"],
        )
    }

    fn stopped(name: &str) -> Container {
        let image = Container::image();
        Container::new(
            name,
            &["create", "--name", "{name}", &image, "sleep", "infinity"],
        )
    }

    fn without_a_shell(name: &str) -> Container {
        let image = std::env::var("VOLANT_PODMAN_TEST_NO_SHELL_IMAGE")
            .unwrap_or_else(|_| "gcr.io/distroless/static-debian12:debug-nonroot".into());
        Container::new(
            name,
            &[
                "run",
                "-d",
                "--name",
                "{name}",
                "--entrypoint",
                "/busybox/sleep",
                &image,
                "infinity",
            ],
        )
    }

    /// The command lines of the processes running in the container.
    fn processes(&self) -> String {
        let out = Command::new("podman")
            .args(["top", &self.0, "args"])
            .output()
            .expect("podman runs");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("podman")
            .args(["rm", "-f", "-t", "0", &self.0])
            .output();
    }
}

fn write_play(dir: &Path, hosts: &[(&str, &str)], tasks: &str) {
    let mut inventory = String::from("all:\n  hosts:\n");
    for (host, vars) in hosts {
        let _ = writeln!(
            inventory,
            "    {host}: {{ansible_connection: podman, {vars}}}"
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

fn play(dir: &Path, hosts: &[(&str, &str)], tasks: &str) -> Output {
    write_play(dir, hosts, tasks);
    volant(dir).output().expect("volant runs")
}

#[test]
#[ignore = "needs podman, containers.podman and a musl agent"]
fn podman_runs_a_play_in_a_running_container() {
    let app = Container::running("app");
    let dir = tempfile::tempdir().unwrap();
    let out = play(
        dir.path(),
        &[("app1", &format!("ansible_host: {}", app.0))],
        "    - ansible.builtin.command: id -un\n      register: who\n    - ansible.builtin.debug:\n        var: who.stdout\n",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("\"who.stdout\": \"root\""), "{stdout}");
    assert!(
        stdout.contains(
            "app1                       : ok=2    changed=1    unreachable=0    failed=0"
        ),
        "{stdout}"
    );
}

#[test]
#[ignore = "needs podman, containers.podman and a musl agent"]
fn podman_reports_what_it_cannot_reach_in_its_own_words() {
    let stopped = Container::stopped("stopped");
    let dir = tempfile::tempdir().unwrap();
    let out = play(
        dir.path(),
        &[
            ("gone", "ansible_host: volant-e2e-no-such-container"),
            ("stopped", &format!("ansible_host: {}", stopped.0)),
        ],
        "    - ansible.builtin.command: id -un\n",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for (host, said) in [("gone", "no such container"), ("stopped", "state improper")] {
        let line = stdout
            .lines()
            .find(|line| line.starts_with(&format!("fatal: [{host}]: UNREACHABLE!")))
            .unwrap_or_else(|| panic!("no unreachable line for {host}: {stdout}"));
        assert!(line.contains(said), "{host}: {line}");
    }
}

#[test]
#[ignore = "needs podman, containers.podman and a musl agent"]
fn podman_reaches_a_container_with_no_shell() {
    let bare = Container::without_a_shell("noshell");
    let dir = tempfile::tempdir().unwrap();
    let out = play(
        dir.path(),
        &[("bare", &format!("ansible_host: {}", bare.0))],
        "    - ansible.builtin.command: /busybox/whoami\n      register: who\n    - ansible.builtin.debug:\n        var: who.stdout\n",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("\"who.stdout\": \"nonroot\""), "{stdout}");
}

/// The controller killed with SIGKILL closes `podman exec`'s stdin, and the agent behind it
/// stops on end of input, so nothing is left running in the container.
#[test]
#[ignore = "needs podman, containers.podman and a musl agent"]
fn podman_leaves_no_agent_behind_a_killed_controller() {
    let app = Container::running("killed");
    let dir = tempfile::tempdir().unwrap();
    write_play(
        dir.path(),
        &[("app1", &format!("ansible_host: {}", app.0))],
        "    - ansible.builtin.command: sleep 30\n",
    );
    let mut controller = volant(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("volant runs");
    let started = Instant::now();
    while !app.processes().contains("volant-agent") {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the agent never started"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    controller.kill().unwrap();
    let _ = controller.wait();
    let killed = Instant::now();
    while app.processes().contains("volant-agent") {
        assert!(
            killed.elapsed() < Duration::from_secs(5),
            "an agent outlived its controller: {}",
            app.processes()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}
