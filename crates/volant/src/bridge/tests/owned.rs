// SPDX-License-Identifier: GPL-3.0-or-later
//! An action run over a connection plugin the helper owns, against the controller's ansible-core:
//! skipped without one, and never skipped under `VOLANT_PYTHON`.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::bridge::{ActionCall, Bridge, Reply};

/// `local`, writing each call it is asked to a log, and saying a line, a warning and a
/// deprecation as it connects. `refuse` makes it refuse to connect, `explode` makes every
/// command raise; the password option is shown in the line and in the refusal.
const FAKE: &str = r#"
import json

from ansible.errors import AnsibleConnectionFailure
from ansible.plugins.connection.local import Connection as Local
from ansible.utils.display import Display

DOCUMENTATION = """
    name: fake
    short_description: the local connection, recording what it is asked
    description:
        - The local connection, which writes each call to a log.
    author: volant
    options:
        log:
            description: Where the calls go.
            vars:
                - name: ansible_fake_log
        refuse:
            description: Refuse to connect.
            type: bool
            default: false
            vars:
                - name: ansible_fake_refuse
        explode:
            description: Raise on every command.
            type: bool
            default: false
            vars:
                - name: ansible_fake_explode
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

display = Display()


class Connection(Local):
    transport = "fake"

    def _log(self, *call):
        with open(self.get_option("log"), "a") as log:
            log.write(json.dumps(call) + "\n")

    def _connect(self):
        if not self._connected:
            display.vvv("ESTABLISH FAKE CONNECTION WITH %s" % self.get_option("password"), host="h1")
            display.warning("a warning from the plugin")
            display.deprecated("a deprecation from the plugin", version="2.99")
            self._log("connect")
            if self.get_option("refuse"):
                raise AnsibleConnectionFailure("refused with %s" % self.get_option("password"))
        return super()._connect()

    def exec_command(self, cmd, in_data=None, sudoable=True):
        self._log("exec_command", cmd)
        if self.get_option("explode"):
            raise RuntimeError("the command exploded")
        return super().exec_command(cmd, in_data, sudoable)

    def put_file(self, in_path, out_path):
        self._log("put_file", out_path)
        return super().put_file(in_path, out_path)

    def close(self):
        self._log("close")
        return super().close()
"#;

const PASSWORD: &str = "s3cret-pw";

/// The calls the fake plugin logged, and the action's value, for a `ping` run over it with
/// `extra` variables; `None` when no helper starts and the test is skipped.
fn ping_over_fake(extra: Value) -> Option<(Vec<Value>, Value)> {
    if let Err(why) = crate::python::PythonBuilder::start() {
        assert!(
            std::env::var_os("VOLANT_PYTHON").is_none(),
            "VOLANT_PYTHON cannot start the helper: {why:#}"
        );
        eprintln!("skipped: {why:#}");
        return None;
    }
    let dir = crate::testing::scratch();
    crate::testing::write(dir.path(), "connection_plugins/fake.py", FAKE);
    let log = dir.path().join("calls.log");
    let python = std::env::var("VOLANT_PYTHON").unwrap_or_else(|_| "python3".into());
    // Both temporary directories inside the test's own: making `~/.ansible/tmp` moves the mtime
    // of a directory another test's union is traced against, which then reads as changed.
    // nextest gives each test a process of its own, so no other thread reads the variable.
    unsafe { std::env::set_var("ANSIBLE_LOCAL_TEMP", dir.path().join("local")) };
    let mut vars = json!({
        "inventory_hostname": "h1",
        "ansible_fake_log": log.display().to_string(),
        "ansible_password": PASSWORD,
        "ansible_python_interpreter": python,
        "ansible_remote_tmp": dir.path().join("remote").display().to_string(),
    });
    vars.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let bridge = Bridge::new(vec![dir.path().to_path_buf()]);
    let value = bridge
        .run_action(
            &ActionCall {
                name: "ansible.builtin.normal",
                task: &json!({"_volant_action": "ansible.builtin.ping", "args": {}}),
                task_vars: &vars,
                play_context: &json!({"remote_addr": "h1", "password": PASSWORD}),
                link: Some(&json!({"kind": "owned", "connection": "fake", "verbosity": 3})),
            },
            Instant::now() + Duration::from_secs(120),
            &mut |ask| Reply::Error(format!("an owned link asked the controller: {ask:?}")),
        )
        .expect("the action ran");
    Some((calls(&log), value))
}

fn calls(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn named<'a>(calls: &'a [Value], name: &str) -> Vec<&'a Value> {
    calls.iter().filter(|call| call[0] == name).collect()
}

/// ansible-core's own `_execute_module` builds the module and sends it through the plugin: the
/// temporary directory, the module file and its run all go through it, and nothing is asked of
/// the controller.
#[test]
fn an_owned_link_runs_the_module_through_the_connection_plugin() {
    let Some((calls, value)) = ping_over_fake(json!({})) else {
        return;
    };
    assert_eq!(value["ping"], "pong", "{value:#}");
    assert_eq!(named(&calls, "connect").len(), 1, "{calls:?}");
    assert!(
        named(&calls, "put_file")
            .iter()
            .any(|call| call[1].as_str().unwrap().ends_with("AnsiballZ_ping.py")),
        "{calls:?}"
    );
    assert!(named(&calls, "exec_command").len() >= 2, "{calls:?}");
    assert_eq!(named(&calls, "close").len(), 1, "{calls:?}");
    assert_eq!(calls.last().unwrap()[0], "close", "{calls:?}");
}

/// What `Display` says comes back in order with its level and host, and no secret is anywhere in
/// the value.
#[test]
fn an_owned_link_returns_what_display_said_without_the_password() {
    let Some((_, value)) = ping_over_fake(json!({})) else {
        return;
    };
    let display = value["_volant_display"].as_array().unwrap();
    let position = |level: Value, text: &str| {
        display
            .iter()
            .position(|event| event["level"] == level && event["text"] == text)
            .unwrap_or_else(|| panic!("no {level} {text:?} in {display:#?}"))
    };
    let line = position(json!(3), "ESTABLISH FAKE CONNECTION WITH ********");
    let warning = position(json!("warning"), "a warning from the plugin");
    let deprecation = position(json!("deprecation"), "a deprecation from the plugin");
    assert!(line < warning && warning < deprecation, "{display:#?}");
    assert_eq!(display[line]["host"], "h1");
    assert!(!value.to_string().contains(PASSWORD), "{value:#}");
}

/// A refusal to connect is the host's: unreachable, with the plugin's words, the password hidden,
/// and the connection closed once all the same.
#[test]
fn an_owned_link_that_cannot_connect_is_unreachable() {
    let Some((calls, value)) = ping_over_fake(json!({"ansible_fake_refuse": true})) else {
        return;
    };
    assert_eq!(value["unreachable"], true, "{value:#}");
    let msg = value["msg"].as_str().unwrap();
    assert!(msg.contains("refused with ********"), "{msg}");
    assert!(!value.to_string().contains(PASSWORD), "{value:#}");
    assert!(named(&calls, "exec_command").is_empty(), "{calls:?}");
    assert_eq!(named(&calls, "close").len(), 1, "{calls:?}");
}

/// A plugin that raises fails the task, and the connection is still closed once.
#[test]
fn an_owned_link_is_closed_once_when_the_plugin_raises() {
    let Some((calls, value)) = ping_over_fake(json!({"ansible_fake_explode": true})) else {
        return;
    };
    assert_eq!(value["failed"], true, "{value:#}");
    assert!(value.get("unreachable").is_none(), "{value:#}");
    assert_eq!(named(&calls, "close").len(), 1, "{calls:?}");
}
