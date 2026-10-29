// SPDX-License-Identifier: GPL-3.0-or-later
//! Network devices through ansible.netcommon's connections, against the fake device of
//! `tests/network`: an SSH command line for `network_cli` and RESTCONF over HTTP for `httpapi`.
//! Each run gets fresh fakes, and Volant's output must be `ansible-playbook`'s. Skipped without
//! the recorded ansible-core, ansible.netcommon and paramiko.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

#[path = "common/python.rs"]
mod python;

const PLAY: &str = r#"- hosts: sw1
  gather_facts: false
  tasks:
    - ansible.netcommon.cli_command:
        command: show version
      register: out
    - debug:
        msg: "{{ out.stdout_lines | first }}"
    - name: a change
      ansible.netcommon.cli_config:
        config: hostname fake1
    - name: the same change again
      ansible.netcommon.cli_config:
        config: hostname fake1
    - ansible.netcommon.cli_command:
        command: show running-config
    - name: a backup
      ansible.netcommon.cli_config:
        config: hostname fake1
        backup: true
        backup_options:
          dir_path: backups
          filename: sw1.cfg
    - stat:
        path: backups/sw1.cfg
      register: saved
      delegate_to: localhost
      no_log: true
    - debug:
        msg: "{{ saved.stat.exists }}"
- hosts: sw1
  gather_facts: true
  tasks: []
- hosts: api1
  gather_facts: false
  tasks:
    - ansible.netcommon.restconf_config:
        path: /data/fake:settings
        content: '{"fake:settings": {"name": "a"}}'
        method: put
    - ansible.netcommon.restconf_get:
        path: /data/fake:settings
"#;

/// The fake device's servers, killed when dropped.
struct Fakes(Vec<Child>);

impl Drop for Fakes {
    fn drop(&mut self) {
        for child in &mut self.0 {
            _ = child.kill();
            _ = child.wait();
        }
    }
}

/// One server of `fake_device.py`, and the port it listens on.
fn fake(python: &Path, dir: &Path, kind: &str) -> (Child, u16) {
    let mut child = Command::new(python)
        .arg(dir.join("fake_device.py"))
        .arg(kind)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(child.stdout.take().unwrap()),
        &mut line,
    )
    .unwrap();
    let port = line
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("{kind} fake said {line:?}"));
    (child, port)
}

/// ansible.netcommon on the collections path and paramiko beside the recorded ansible-core.
fn ready(python: &Path) -> bool {
    let netcommon = std::env::var_os("ANSIBLE_COLLECTIONS_PATH").is_some_and(|paths| {
        std::env::split_paths(&paths)
            .any(|path| path.join("ansible_collections/ansible/netcommon").is_dir())
    });
    let paramiko = Command::new(python)
        .args(["-c", "import paramiko"])
        .status()
        .is_ok_and(|status| status.success());
    if !(netcommon && paramiko) {
        eprintln!("skipped: ansible.netcommon on ANSIBLE_COLLECTIONS_PATH and paramiko are needed");
    }
    netcommon && paramiko
}

/// The play under `program` (with `args` in front of the play's own) in a fresh copy of
/// `tests/network`, against fresh fakes: its exit code and its stdout and stderr together.
fn run(python: &Path, program: &[&Path], args: &[&str]) -> (Option<i32>, String) {
    let dir = tempfile::tempdir().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/network");
    let copied = Command::new("cp")
        .arg("-r")
        .arg(source.join("."))
        .arg(dir.path())
        .status()
        .unwrap();
    assert!(copied.success());
    let (ssh, ssh_port) = fake(python, dir.path(), "ssh");
    let (http, http_port) = fake(python, dir.path(), "http");
    let _fakes = Fakes(vec![ssh, http]);
    let device = "ansible_host=127.0.0.1 ansible_user=admin ansible_password=fake-pw";
    std::fs::write(
        dir.path().join("inv.ini"),
        format!(
            "sw1 ansible_connection=network_cli ansible_network_os=volant.fakenet.fake ansible_port={ssh_port} {device}\n\
             api1 ansible_connection=httpapi ansible_network_os=ansible.netcommon.restconf ansible_httpapi_port={http_port} {device}\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.path().join("play.yml"), PLAY).unwrap();
    let out = Command::new(program[0])
        .args(&program[1..])
        .args(["-i", "inv.ini", "play.yml"])
        .args(args)
        .current_dir(dir.path())
        .env("VOLANT_PYTHON", python)
        .env("NO_COLOR", "1")
        .env("ANSIBLE_NOCOLOR", "1")
        .env("ANSIBLE_HOST_KEY_CHECKING", "False")
        .env("ANSIBLE_CONFIG", dir.path().join("ansible.cfg"))
        .env("ANSIBLE_LOCAL_TEMP", dir.path().join("tmp"))
        .env("ANSIBLE_PERSISTENT_CONTROL_PATH_DIR", dir.path().join("pc"))
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let text = text.replace(&dir.path().display().to_string(), "<dir>");
    (out.status.code(), masked(&text))
}

/// `text` with each `YYYY-MM-DD` written `<date>` and each `HH:MM:SS` written `<time>`, which a
/// backup's result holds.
fn masked(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let shaped = |s: &str, shape: &str| {
        s.len() >= shape.len()
            && s.bytes().zip(shape.bytes()).all(|(c, want)| match want {
                b'9' => c.is_ascii_digit(),
                _ => c == want,
            })
    };
    while let Some(c) = rest.chars().next() {
        if shaped(rest, "9999-99-99") {
            out.push_str("<date>");
            rest = &rest[10..];
        } else if shaped(rest, "99:99:99") {
            out.push_str("<time>");
            rest = &rest[8..];
        } else {
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// `ansible-playbook` beside the recorded interpreter, and Volant.
fn programs(python: &Path) -> (PathBuf, PathBuf) {
    let reference = python.parent().unwrap().join("ansible-playbook");
    (reference, PathBuf::from(env!("CARGO_BIN_EXE_volant")))
}

/// A command, a configuration change and the same change again over `network_cli`, then a
/// RESTCONF write and read over `httpapi`: Volant prints what `ansible-playbook` prints.
#[test]
fn a_device_runs_as_under_ansible_playbook() {
    let Some(python) = python::reference_python() else {
        return;
    };
    if !ready(&python) {
        return;
    }
    let (reference, volant) = programs(&python);
    for args in [&[][..], &["-v"]] {
        let want = run(&python, &[&reference], args);
        let got = run(&python, &[&volant, Path::new("playbook")], args);
        assert_eq!(got, want, "at {args:?}");
        assert!(want.1.contains("Fake OS 1.0"), "{}", want.1);
        assert!(want.1.contains("changed: [sw1]"), "{}", want.1);
    }
}

/// A `network_cli` host with no network OS, or one with no terminal plugin, is unreachable, and a
/// module that needs another connection fails, with ansible.netcommon's own texts. One fork keeps
/// the hosts in order.
#[test]
fn a_device_that_cannot_be_driven_fails_as_under_ansible_playbook() {
    let Some(python) = python::reference_python() else {
        return;
    };
    if !ready(&python) {
        return;
    }
    let (reference, volant) = programs(&python);
    let dir = tempfile::tempdir().unwrap();
    let refused =
        "ansible_host=127.0.0.1 ansible_port=9 ansible_user=admin ansible_password=fake-pw";
    let default = "ansible_network_os=ansible.netcommon.default";
    std::fs::write(
        dir.path().join("inv.ini"),
        format!(
            "nos ansible_connection=network_cli {refused}\n\
             bad ansible_connection=network_cli {default} {refused}\n\
             nc ansible_connection=netconf {default} {refused}\n"
        ),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("play.yml"),
        "- hosts: all\n  gather_facts: false\n  tasks:\n    - ansible.netcommon.cli_command:\n        command: show version\n",
    )
    .unwrap();
    let run = |program: &[&Path]| {
        let out = Command::new(program[0])
            .args(&program[1..])
            .args(["-i", "inv.ini", "play.yml", "-f", "1"])
            .current_dir(dir.path())
            .env("VOLANT_PYTHON", &python)
            .env("NO_COLOR", "1")
            .env("ANSIBLE_NOCOLOR", "1")
            .env("ANSIBLE_LOCAL_TEMP", dir.path().join("tmp"))
            .env("ANSIBLE_PERSISTENT_CONTROL_PATH_DIR", dir.path().join("pc"))
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.code(), text)
    };
    let want = run(&[&reference]);
    let got = run(&[&volant, Path::new("playbook")]);
    assert_eq!(got, want);
    for text in [
        "Unable to automatically determine host network os",
        "network os ansible.netcommon.default is not supported",
        "Connection type netconf is not valid for this module",
    ] {
        assert!(want.1.contains(text), "{}", want.1);
    }
}

/// At `-vvvv` the persistent process says which SSH library `network_cli` chose, as it does
/// under `ansible-playbook`, and the device's password is in no line.
#[test]
fn the_chosen_ssh_type_is_said_and_the_password_is_not() {
    let Some(python) = python::reference_python() else {
        return;
    };
    if !ready(&python) {
        return;
    }
    let (reference, volant) = programs(&python);
    let ssh_type = |text: &str| {
        text.lines()
            .filter(|line| line.contains("ssh type is"))
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let want = run(&python, &[&reference], &["-vvvv"]);
    let got = run(&python, &[&volant, Path::new("playbook")], &["-vvvv"]);
    assert_eq!(got.0, want.0, "{}", got.1);
    assert!(!ssh_type(&want.1).is_empty(), "{}", want.1);
    assert_eq!(ssh_type(&got.1), ssh_type(&want.1), "{}", got.1);
    assert!(!got.1.contains("fake-pw"), "{}", got.1);
}
