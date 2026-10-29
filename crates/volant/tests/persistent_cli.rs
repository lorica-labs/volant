// SPDX-License-Identifier: GPL-3.0-or-later
//! Hosts whose connection keeps a persistent process, as `network_cli`, `netconf` and `httpapi`
//! do: ansible-core's `start_connection` runs from the Python helper, and the process is reused,
//! reset and ended as ansible-core does. Skipped without the recorded ansible-core.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

#[path = "common/python.rs"]
mod python;

/// A connection that forces persistence and runs its commands locally, as `NetworkConnectionBase`
/// does for a network module. The process writes its pid to `starts` beside it each time the
/// stub sets its options, which it does again for every task that reuses it, so the distinct
/// pids are the processes started; with `refuse` beside it, the process does not start.
const FAKE_NET: &str = r#"
import os

from ansible.errors import AnsibleConnectionFailure
from ansible.plugins.connection import NetworkConnectionBase

DOCUMENTATION = """
    name: fake_net
    short_description: a persistent connection that runs locally
    description:
        - A connection kept in a persistent process, whose commands run on the controller.
    author: volant
    options:
        password:
            description: A password.
            vars:
                - name: ansible_password
        persistent_connect_timeout:
            type: int
            description: As network_cli.
            default: 30
            env:
                - name: ANSIBLE_PERSISTENT_CONNECT_TIMEOUT
            vars:
                - name: ansible_connect_timeout
        persistent_command_timeout:
            type: int
            description: As network_cli.
            default: 30
            env:
                - name: ANSIBLE_PERSISTENT_COMMAND_TIMEOUT
            vars:
                - name: ansible_command_timeout
        persistent_log_messages:
            type: boolean
            description: As network_cli.
            default: false
            env:
                - name: ANSIBLE_PERSISTENT_LOG_MESSAGES
            vars:
                - name: ansible_persistent_log_messages
"""

HERE = os.path.dirname(os.path.abspath(__file__))


class Connection(NetworkConnectionBase):
    transport = "fake_net"
    has_pipelining = False

    def set_options(self, task_keys=None, var_options=None, direct=None):
        super().set_options(task_keys=task_keys, var_options=var_options, direct=direct)
        if direct is not None:
            if os.path.exists(os.path.join(HERE, "refuse")):
                raise AnsibleConnectionFailure("the fake device refused us")
            with open(os.path.join(HERE, "starts"), "a") as starts:
                starts.write("%d\n" % os.getpid())

    def _connect(self):
        self._connected = True
"#;

const PASSWORD: &str = "n3t-s3cret";

struct Run {
    code: Option<i32>,
    text: String,
    starts: usize,
}

/// `volant playbook` under the recorded ansible-core in `dir`, with the fake plugin beside
/// `play` and its sockets under `dir/pc`.
fn run(
    dir: &Path,
    inventory: &str,
    play: &str,
    args: &[&str],
    env: &[(&str, &str)],
) -> Option<Run> {
    let python = python::reference_python()?;
    std::fs::create_dir_all(dir.join("connection_plugins")).unwrap();
    std::fs::write(dir.join("connection_plugins/fake_net.py"), FAKE_NET).unwrap();
    std::fs::write(
        dir.join("inv.ini"),
        inventory.replace("{python}", &python.display().to_string()),
    )
    .unwrap();
    std::fs::write(dir.join("play.yml"), play).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(["playbook", "-i", "inv.ini", "play.yml"])
        .args(args)
        .current_dir(dir)
        .env("VOLANT_PYTHON", &python)
        .env("NO_COLOR", "1")
        .env("ANSIBLE_PERSISTENT_CONTROL_PATH_DIR", dir.join("pc"))
        .env("ANSIBLE_LOCAL_TEMP", dir.join("tmp"))
        .envs(env.iter().copied())
        .output()
        .unwrap();
    let starts = std::fs::read_to_string(dir.join("connection_plugins/starts")).map_or(0, |text| {
        text.lines()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    });
    Some(Run {
        code: out.status.code(),
        text: format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        starts,
    })
}

/// The sockets left under `dir/pc`, after the processes had `wait` to go.
fn sockets_left(dir: &Path, wait: Duration) -> Vec<String> {
    let until = Instant::now() + wait;
    loop {
        let left: Vec<String> = std::fs::read_dir(dir.join("pc"))
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| !name.starts_with(".ansible_pc_lock"))
            .collect();
        if left.is_empty() || Instant::now() >= until {
            return left;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

const HOST: &str =
    "ansible_connection=fake_net ansible_host=127.0.0.1 ansible_python_interpreter={python}";

fn play(tasks: &str) -> String {
    format!("- hosts: all\n  gather_facts: false\n  tasks:\n{tasks}")
}

/// Ten tasks on two hosts that reach the same address, run two at a time so two helpers ask at
/// once, share one process, which ends with the play.
#[test]
fn ten_tasks_and_two_helpers_share_one_process() {
    let dir = tempfile::tempdir().unwrap();
    let inventory = format!("h1 {HOST}\nh2 {HOST}\n");
    let tasks = "    - ping:\n".repeat(5);
    let Some(run) = run(dir.path(), &inventory, &play(&tasks), &["-f", "2"], &[]) else {
        return;
    };
    assert_eq!(run.code, Some(0), "{}", run.text);
    assert_eq!(run.text.matches("ok: [h").count(), 10, "{}", run.text);
    assert_eq!(run.starts, 1, "{}", run.text);
    assert_eq!(
        sockets_left(dir.path(), Duration::from_secs(5)),
        Vec::<String>::new()
    );
}

/// `meta: reset_connection` ends the process: the next task starts another.
#[test]
fn reset_connection_ends_the_process() {
    let dir = tempfile::tempdir().unwrap();
    let tasks = "    - ping:\n    - meta: reset_connection\n    - ping:\n";
    let Some(run) = run(dir.path(), &format!("h1 {HOST}\n"), &play(tasks), &[], &[]) else {
        return;
    };
    assert_eq!(run.code, Some(0), "{}", run.text);
    assert_eq!(run.starts, 2, "{}", run.text);
}

/// The lines ansible-core's `TaskExecutor` and the stub say at `-vvvv`, in order, under the
/// address; the password is in none of them.
#[test]
fn the_start_is_said_at_vvvv_without_the_password() {
    let dir = tempfile::tempdir().unwrap();
    let inventory = format!("h1 {HOST} ansible_password={PASSWORD}\n");
    let Some(run) = run(
        dir.path(),
        &inventory,
        &play("    - ping:\n"),
        &["-vvvv"],
        &[],
    ) else {
        return;
    };
    assert_eq!(run.code, Some(0), "{}", run.text);
    let mut at = 0;
    for line in [
        "<127.0.0.1> attempting to start connection",
        "<127.0.0.1> using connection plugin fake_net",
        "<127.0.0.1> local domain socket does not exist, starting it",
        "<127.0.0.1> control socket path is ",
        "<127.0.0.1> local domain socket listeners started successfully",
        "<127.0.0.1> local domain socket path is ",
        "<127.0.0.1> ESTABLISH LOCAL CONNECTION FOR USER: ",
    ] {
        let found = run.text[at..].find(line);
        assert!(
            found.is_some(),
            "no {line:?} after byte {at}:\n{}",
            run.text
        );
        at += found.unwrap() + line.len();
    }
    assert!(!run.text.contains(PASSWORD), "{}", run.text);
}

/// A process that cannot start fails the task, not the host, with its error; `-vvv` adds the
/// traceback.
#[test]
fn a_process_that_cannot_start_fails_the_task() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("connection_plugins")).unwrap();
    std::fs::write(dir.path().join("connection_plugins/refuse"), "").unwrap();
    let Some(run) = run(
        dir.path(),
        &format!("h1 {HOST}\n"),
        &play("    - ping:\n"),
        &["-vvv"],
        &[],
    ) else {
        return;
    };
    assert_eq!(run.code, Some(2), "{}", run.text);
    assert!(
        run.text.contains("the fake device refused us"),
        "{}",
        run.text
    );
    assert!(run.text.contains("The full traceback is:"), "{}", run.text);
    assert!(run.text.contains("failed=1"), "{}", run.text);
    assert!(run.text.contains("unreachable=0"), "{}", run.text);
}

/// `persistent_log_messages` is warned about with ansible-core's text.
#[test]
fn logging_the_messages_is_warned_about() {
    let dir = tempfile::tempdir().unwrap();
    let env = [("ANSIBLE_PERSISTENT_LOG_MESSAGES", "true")];
    let Some(run) = run(
        dir.path(),
        &format!("h1 {HOST}\n"),
        &play("    - ping:\n"),
        &[],
        &env,
    ) else {
        return;
    };
    assert_eq!(run.code, Some(0), "{}", run.text);
    assert!(
        run.text
            .contains("Persistent connection logging is enabled for 127.0.0.1"),
        "{}",
        run.text
    );
    assert!(
        run.text.contains("WILL NOT redact sensitive configuration"),
        "{}",
        run.text
    );
}
