// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end runs over `containers.podman.buildah` against real working containers. Ignored by
//! default: they need `buildah`, containers.podman on `ANSIBLE_COLLECTIONS_PATH`, and
//! `VOLANT_AGENT_DIR` holding `volant-agent-<triple>` for the images' architecture.
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
        Working::from("docker.io/library/alpine:3.20")
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

/// The inode and modification time of the agent the first run cached under root's `remote_tmp`:
/// an upload writes a new copy and renames it over the old one, so a second run that uploaded
/// nothing leaves both as they were.
const CACHED_AGENT: &str = "stat -c '%i %Y' /root/.ansible/tmp/volant-agent-*/volant-agent";
/// A user of the image, `volant`, who may run anything through `sudo` with no password.
const WITH_SUDO_USER: &str = "adduser -D volant && apk add --no-cache sudo >/dev/null && echo 'volant ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/volant";

const AS_USER_AND_ROOT: &str = "    - ansible.builtin.command: id -un\n      register: who\n    - ansible.builtin.command: id -un\n      become: true\n      register: root\n    - ansible.builtin.debug:\n        msg: \"{{ who.stdout }} then {{ root.stdout }}\"\n";

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
#[ignore = "needs buildah, containers.podman and a musl agent"]
fn buildah_reuses_the_cached_agent_on_a_second_run() {
    let working = Working::alpine();
    let dir = tempfile::tempdir().unwrap();
    let cached = || ran(&["buildah", "run", &working.0, "--", "sh", "-c", CACHED_AGENT]);
    let mut before = String::new();
    for round in 0..2 {
        let out = play(
            dir.path(),
            &[(&working.0, "")],
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

/// `ansible_user` names a user of the image other than root, and `become` reaches root through
/// the image's `sudo`.
#[test]
#[ignore = "needs buildah, containers.podman and a musl agent"]
fn buildah_runs_as_another_user_and_becomes_root_through_sudo() {
    let working = Working::alpine();
    ran(&[
        "buildah",
        "run",
        &working.0,
        "--",
        "sh",
        "-c",
        WITH_SUDO_USER,
    ]);
    let dir = tempfile::tempdir().unwrap();
    let out = play(
        dir.path(),
        &[(&working.0, ", ansible_user: volant")],
        AS_USER_AND_ROOT,
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("\"msg\": \"volant then root\""), "{stdout}");
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
    let working = Working::from("gcr.io/distroless/static-debian12:debug-nonroot");
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
