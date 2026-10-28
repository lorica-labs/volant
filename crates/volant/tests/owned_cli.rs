// SPDX-License-Identifier: GPL-3.0-or-later
//! Hosts whose connection plugin only ansible-core has, run through the Python helper: skipped
//! without the recorded ansible-core, and never skipped under `VOLANT_PYTHON`.
#![cfg(unix)]

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

#[path = "common/python.rs"]
mod python;

/// `local` under another name, saying the password it was given at `-vvv` as it connects.
const CHATTY: &str = r#"
from ansible.plugins.connection.local import Connection as Local
from ansible.utils.display import Display

DOCUMENTATION = """
    name: chatty_owned
    short_description: the local connection, saying its password
    description:
        - The local connection, which prints the password it was given.
    author: volant
    options:
        password:
            description: A password.
            vars:
                - name: ansible_password
        become_success_timeout:
            description: As local.
            type: int
            default: 10
        become_strip_preamble:
            description: As local.
            type: bool
            default: true
    extends_documentation_fragment:
        - connection_pipelining
"""


class Connection(Local):
    transport = "chatty_owned"

    def _connect(self):
        if not self._connected:
            Display().vvv("PASSWORD IS %s" % self.get_option("password"), host="h")
        return super()._connect()
"#;

const PASSWORD: &str = "s3cret-pw";

/// `volant playbook` under the recorded ansible-core in `dir`, with `play` and `inventory`, and
/// its exit code, stdout and stderr together.
fn run(dir: &Path, inventory: &str, play: &str, args: &[&str]) -> Option<(Option<i32>, String)> {
    let python = python::reference_python()?;
    std::fs::create_dir_all(dir.join("connection_plugins")).unwrap();
    std::fs::write(dir.join("connection_plugins/chatty_owned.py"), CHATTY).unwrap();
    std::fs::write(dir.join("inv.ini"), inventory).unwrap();
    std::fs::write(dir.join("play.yml"), play).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(["playbook", "-i", "inv.ini", "play.yml"])
        .args(args)
        .current_dir(dir)
        .env("VOLANT_PYTHON", python)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    Some((
        out.status.code(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    ))
}

const PLAY: &str = "- hosts: all\n  gather_facts: false\n  tasks:\n    - command: echo hi\n";

/// A secret the host's variables hold is in no line, at any verbosity: the plugin's own line that
/// prints it shows `********`.
#[test]
fn an_owned_host_s_password_is_in_no_output() {
    let dir = tempfile::tempdir().unwrap();
    let inventory = format!(
        "h ansible_connection=chatty_owned ansible_password={PASSWORD} ansible_python_interpreter=/usr/bin/python3 ansible_remote_tmp={}\n",
        dir.path().join("remote").display()
    );
    let Some((code, text)) = run(dir.path(), &inventory, PLAY, &["-vvvv"]) else {
        return;
    };
    assert_eq!(code, Some(0), "{text}");
    assert!(text.contains("<h> PASSWORD IS ********"), "{text}");
    assert!(!text.contains(PASSWORD), "{text}");
}

/// Fifty owned hosts under five forks all run, each task in a helper of the pool.
#[test]
fn fifty_owned_hosts_run_under_the_fork_limit() {
    let dir = tempfile::tempdir().unwrap();
    let mut inventory = String::new();
    for n in 0..50 {
        let _ = writeln!(
            inventory,
            "h{n} ansible_connection=chatty_owned ansible_python_interpreter=/usr/bin/python3 ansible_remote_tmp={}",
            dir.path().join("remote").display()
        );
    }
    let started = Instant::now();
    let Some((code, text)) = run(dir.path(), &inventory, PLAY, &["-f", "5"]) else {
        return;
    };
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(text.matches("changed: [h").count(), 50, "{text}");
    eprintln!("fifty owned hosts: {:?}", started.elapsed());
}
