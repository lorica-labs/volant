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
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

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

impl Container {
    fn processes(&self) -> String {
        let out = Command::new("docker")
            .args(["exec", &self.0, "ps", "-o", "args"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}

/// Runs `site.yml` from `dir` against `hosts`, each line of it the host's inventory variables.
fn play(dir: &Path, hosts: &[(&str, &str)], tasks: &str) -> Output {
    playbook(dir, hosts, tasks).output().expect("volant runs")
}

/// [`play`]'s command, written and not yet started.
fn playbook(dir: &Path, hosts: &[(&str, &str)], tasks: &str) -> Command {
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_volant"));
    command
        .args(["playbook", "-i", "inventory.yml", "site.yml"])
        .current_dir(dir);
    command
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
#[ignore = "needs docker, community.docker and a musl agent"]
fn docker_reuses_the_cached_agent_on_a_second_run() {
    let app = Container::running("cache");
    let dir = tempfile::tempdir().unwrap();
    let cached = || ran(&["docker", "exec", &app.0, "sh", "-c", CACHED_AGENT]);
    let mut before = String::new();
    for round in 0..2 {
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
#[ignore = "needs docker, community.docker and a musl agent"]
fn docker_runs_as_another_user_and_becomes_root_through_sudo() {
    let app = Container::running("sudo");
    ran(&["docker", "exec", &app.0, "sh", "-c", WITH_SUDO_USER]);
    let dir = tempfile::tempdir().unwrap();
    let out = play(
        dir.path(),
        &[(
            "app1",
            &format!("ansible_docker_host: {}, ansible_user: volant", app.0),
        )],
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

/// The controller killed with SIGKILL closes `docker exec -i`'s stdin, and the agent behind it
/// stops on end of input, so nothing is left running in the container.
#[test]
#[ignore = "needs docker, community.docker and a musl agent"]
fn docker_leaves_no_agent_behind_a_killed_controller() {
    let app = Container::running("killed");
    let dir = tempfile::tempdir().unwrap();
    let mut controller = playbook(
        dir.path(),
        &[("app1", &format!("ansible_host: {}", app.0))],
        "    - ansible.builtin.command: sleep 30
",
    )
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
