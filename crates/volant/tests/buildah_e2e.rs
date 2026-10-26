// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end runs over `containers.podman.buildah` against real working containers. Ignored by
//! default: they need `buildah`, containers.podman on `ANSIBLE_COLLECTIONS_PATH`, and
//! `VOLANT_AGENT_DIR` holding `volant-agent-<triple>` for the images' architecture.
//! `VOLANT_BUILDAH_TEST_IMAGE` picks the image, `docker.io/library/alpine:3.20` by default, and
//! `VOLANT_BUILDAH_TEST_NO_SHELL_IMAGE` the one with no `/bin/sh`.
#![cfg(target_os = "linux")]

use std::fmt::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// A working container of this test's own, removed when the test ends.
struct Working(String);

impl Working {
    fn from(image: &str) -> Working {
        let out = Command::new("buildah")
            .args(["from", "-q", image])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Working(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn alpine() -> Working {
        Working::from(
            &std::env::var("VOLANT_BUILDAH_TEST_IMAGE")
                .unwrap_or_else(|_| "docker.io/library/alpine:3.20".into()),
        )
    }
}

impl Drop for Working {
    fn drop(&mut self) {
        let _ = Command::new("buildah").args(["rm", &self.0]).output();
    }
}

fn write_play(dir: &Path, hosts: &[(&str, &str)], tasks: &str) {
    let mut inventory = String::from("all:\n  hosts:\n");
    for (host, vars) in hosts {
        let _ = writeln!(
            inventory,
            "    {host}: {{ansible_connection: buildah{vars}}}"
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

/// The play writes a file in the working container, and the image committed from it holds it.
#[test]
#[ignore = "needs buildah, containers.podman and a musl agent"]
fn buildah_builds_an_image_with_what_the_play_wrote() {
    let working = Working::alpine();
    let dir = tempfile::tempdir().unwrap();
    let out = play(
        dir.path(),
        &[(&working.0, "")],
        "    - ansible.builtin.command: id -un\n      register: who\n    - ansible.builtin.debug:\n        var: who.stdout\n    - ansible.builtin.shell: printf 'built by a play' > /etc/volant-e2e\n",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("\"who.stdout\": \"root\""), "{stdout}");
    let image = format!("localhost/volant-e2e-{}", std::process::id());
    let committed = Command::new("buildah")
        .args(["commit", "-q", &working.0, &image])
        .output()
        .unwrap();
    assert!(
        committed.status.success(),
        "{}",
        String::from_utf8_lossy(&committed.stderr)
    );
    let check = Working::from(&image);
    let read = Command::new("buildah")
        .args(["run", &check.0, "--", "cat", "/etc/volant-e2e"])
        .stderr(Stdio::null())
        .output()
        .unwrap();
    let _ = Command::new("buildah").args(["rmi", "-f", &image]).output();
    assert_eq!(String::from_utf8_lossy(&read.stdout), "built by a play");
}

#[test]
#[ignore = "needs buildah, containers.podman and a musl agent"]
fn buildah_reports_a_container_it_does_not_know() {
    let dir = tempfile::tempdir().unwrap();
    let out = play(
        dir.path(),
        &[("volant-e2e-no-such-container", "")],
        "    - ansible.builtin.ping:\n",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .find(|line| line.starts_with("fatal: [volant-e2e-no-such-container]: UNREACHABLE!"))
        .unwrap_or_else(|| panic!("no unreachable line: {stdout}"));
    assert!(line.contains("container not known"), "{line}");
}

#[test]
#[ignore = "needs buildah, containers.podman and a musl agent"]
fn buildah_reaches_a_working_container_with_no_shell() {
    let working = Working::from(
        &std::env::var("VOLANT_BUILDAH_TEST_NO_SHELL_IMAGE")
            .unwrap_or_else(|_| "gcr.io/distroless/static-debian12:debug-nonroot".into()),
    );
    let dir = tempfile::tempdir().unwrap();
    let out = play(
        dir.path(),
        &[(&working.0, "")],
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

/// The controller killed with SIGKILL closes `buildah run`'s stdin, and the agent behind it
/// stops on end of input, so nothing is left running in the working container.
#[test]
#[ignore = "needs buildah, containers.podman and a musl agent"]
fn buildah_leaves_no_agent_behind_a_killed_controller() {
    let working = Working::alpine();
    let dir = tempfile::tempdir().unwrap();
    write_play(
        dir.path(),
        &[(&working.0, "")],
        "    - ansible.builtin.command: sleep 30\n",
    );
    let running = || {
        let out = Command::new("pgrep")
            .args(["-f", "volant-agent-.*/volant-agent$"])
            .output()
            .unwrap();
        !out.stdout.is_empty()
    };
    let mut controller = volant(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("volant runs");
    let started = Instant::now();
    while !running() {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the agent never started"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    controller.kill().unwrap();
    let _ = controller.wait();
    let killed = Instant::now();
    while running() {
        assert!(
            killed.elapsed() < Duration::from_secs(5),
            "an agent outlived its controller"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}
