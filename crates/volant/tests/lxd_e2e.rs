// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end runs over `community.general.lxd` and `community.general.incus` against real
//! instances. Ignored by default: they need `lxc` or `incus` talking to an initialised server,
//! community.general on `ANSIBLE_COLLECTIONS_PATH`, and `VOLANT_AGENT_DIR` holding
//! `volant-agent-<triple>`. `VOLANT_LXD_TEST_IMAGE` picks LXD's image, `ubuntu-minimal:24.04` by
//! default, and `VOLANT_INCUS_TEST_IMAGE` Incus's, `images:alpine/3.21` by default.
#![cfg(target_os = "linux")]

use std::fmt::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// An instance of this test's own, deleted when the test ends: `.0` is the program, `.1` the name.
struct Instance(&'static str, String);

impl Instance {
    fn launched(name: &str) -> Instance {
        Instance::of("lxc", name)
    }

    fn of(program: &'static str, name: &str) -> Instance {
        let name = format!("volant-e2e-{name}-{}", std::process::id());
        let image = match program {
            "lxc" => std::env::var("VOLANT_LXD_TEST_IMAGE")
                .unwrap_or_else(|_| "ubuntu-minimal:24.04".into()),
            _ => std::env::var("VOLANT_INCUS_TEST_IMAGE")
                .unwrap_or_else(|_| "images:alpine/3.21".into()),
        };
        let out = Command::new(program)
            .args(["launch", "-q", &image, &name])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // An instance answers `lxc exec` once its init has started.
        let up = Instant::now();
        while !Command::new(program)
            .args(["exec", &name, "--", "true"])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            assert!(
                up.elapsed() < Duration::from_secs(60),
                "{name} never answered"
            );
            std::thread::sleep(Duration::from_millis(500));
        }
        Instance(program, name)
    }

    fn stop(&self) {
        let _ = Command::new(self.0)
            .args(["stop", "-q", "--force", &self.1])
            .output();
    }

    fn processes(&self) -> String {
        let out = Command::new(self.0)
            .args(["exec", &self.1, "--", "ps", "-eo", "args"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = Command::new(self.0)
            .args(["delete", "-q", "--force", &self.1])
            .output();
    }
}

fn write_play(dir: &Path, hosts: &[(&str, &str)], tasks: &str) {
    let mut inventory = String::from("all:\n  hosts:\n");
    for (host, vars) in hosts {
        // A host's variables name their own connection, or follow lxd's after a comma.
        let vars = if vars.starts_with("ansible_connection") {
            (*vars).to_string()
        } else {
            format!("ansible_connection: lxd{vars}")
        };
        let _ = writeln!(inventory, "    {host}: {{{vars}}}");
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

const WHO_AM_I: &str = "    - ansible.builtin.command: id -un\n      register: who\n    - ansible.builtin.debug:\n        var: who.stdout\n";

fn play(dir: &Path, hosts: &[(&str, &str)], tasks: &str) -> Output {
    write_play(dir, hosts, tasks);
    volant(dir).output().expect("volant runs")
}

/// As root, and as another user through the become method, `/bin/su` by default.
#[test]
#[ignore = "needs LXD, community.general and the agent"]
fn lxd_runs_a_play_as_root_and_as_another_user() {
    let app = Instance::launched("app");
    let made = Command::new("lxc")
        .args([
            "exec", &app.1, "--", "useradd", "-m", "-s", "/bin/sh", "volant",
        ])
        .output()
        .unwrap();
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    let dir = tempfile::tempdir().unwrap();
    let out = play(
        dir.path(),
        &[
            (&app.1, ""),
            (
                "as_volant",
                &format!(", ansible_lxd_host: {}, ansible_user: volant", app.1),
            ),
        ],
        WHO_AM_I,
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("\"who.stdout\": \"root\""), "{stdout}");
    assert!(stdout.contains("\"who.stdout\": \"volant\""), "{stdout}");
}

#[test]
#[ignore = "needs LXD, community.general and the agent"]
fn lxd_reports_what_it_cannot_reach_in_the_plugin_s_words() {
    let stopped = Instance::launched("stopped");
    stopped.stop();
    let dir = tempfile::tempdir().unwrap();
    let out = play(
        dir.path(),
        &[("volant-e2e-no-such-instance", ""), (&stopped.1, "")],
        "    - ansible.builtin.ping:\n",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for (host, said) in [
        (
            "volant-e2e-no-such-instance",
            "instance not found: volant-e2e-no-such-instance".to_string(),
        ),
        (
            stopped.1.as_str(),
            format!("instance not running: {}", stopped.1),
        ),
    ] {
        let line = stdout
            .lines()
            .find(|line| line.starts_with(&format!("fatal: [{host}]: UNREACHABLE!")))
            .unwrap_or_else(|| panic!("no unreachable line for {host}: {stdout}"));
        assert!(line.contains(&said), "{host}: {line}");
    }
}

/// The controller killed with SIGKILL closes `lxc exec`'s stdin, and the agent behind it stops
/// on end of input.
#[test]
#[ignore = "needs LXD, community.general and the agent"]
fn lxd_leaves_no_agent_behind_a_killed_controller() {
    let app = Instance::launched("killed");
    let dir = tempfile::tempdir().unwrap();
    write_play(
        dir.path(),
        &[(&app.1, "")],
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
            "an agent outlived its controller"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Incus runs the same play, and words its refusals with the remote and the project.
#[test]
#[ignore = "needs Incus, community.general and the agent"]
fn incus_runs_a_play_and_words_its_refusals_with_the_project() {
    let app = Instance::of("incus", "iapp");
    let stopped = Instance::of("incus", "istopped");
    stopped.stop();
    let dir = tempfile::tempdir().unwrap();
    let incus = "ansible_connection: community.general.incus";
    let out = play(
        dir.path(),
        &[
            (&app.1, incus),
            (&stopped.1, incus),
            ("volant-e2e-no-such-instance", incus),
        ],
        WHO_AM_I,
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"who.stdout\": \"root\""), "{stdout}");
    for (host, said) in [
        (
            stopped.1.as_str(),
            format!(
                "instance not running: {} (remote=local, project=default)",
                stopped.1
            ),
        ),
        (
            "volant-e2e-no-such-instance",
            "instance not found: volant-e2e-no-such-instance (remote=local, project=default)"
                .to_string(),
        ),
    ] {
        let line = stdout
            .lines()
            .find(|line| line.starts_with(&format!("fatal: [{host}]: UNREACHABLE!")))
            .unwrap_or_else(|| panic!("no unreachable line for {host}: {stdout}"));
        assert!(line.contains(&said), "{host}: {line}");
    }
}
