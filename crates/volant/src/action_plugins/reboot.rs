// SPDX-License-Identifier: GPL-3.0-or-later
//! `reboot`: restart the host, wait until it has booted again, and check that it answers.
//!
//! Read off `plugins/action/reboot.py` of ansible-core 2.19.12, and measured with `reboot: {}`
//! under `become` on a real host, which came back on its own: a `setup` for the distribution,
//! `cat /proc/sys/kernel/random/boot_id` as a bare command, a `find` for `shutdown` in five
//! directories, `/sbin/shutdown -r 0 "Reboot initiated by Ansible"`, then the boot id read again
//! until it changed - through `Timeout (12s) waiting for privilege escalation prompt` and `No
//! route to host`, each absorbed with a growing pause - and finally `whoami`. The result was
//! `{"changed": true, "elapsed": 21, "failed": false, "rebooted": true}`, 28 s of wall time.
//!
//! The commands go through the agent's `raw`, which hands its line to `sh -c` as the reference's
//! low-level command goes to the remote shell, so a `boot_time_command` or a `test_command` the
//! playbook writes is a shell line under both engines.
//!
//! The waiting is the driver's: [`Step::Reconnect`] drops the host's links, opens a fresh one,
//! and hands back what the probe read on it. This side keeps the clock (monotonic, from the
//! moment the `shutdown` command returned) and decides when the answer means the host is back.

use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;

use super::{Context, Plugin, Step, Sub, is_gone, lost};

/// The arguments the reference's plugin accepts; any other fails the task, naming it.
const VALID_ARGS: [&str; 9] = [
    "boot_time_command",
    "connect_timeout",
    "msg",
    "post_reboot_delay",
    "pre_reboot_delay",
    "reboot_command",
    "reboot_timeout",
    "search_paths",
    "test_command",
];

const DEFAULT_SEARCH_PATHS: [&str; 5] =
    ["/sbin", "/bin", "/usr/sbin", "/usr/bin", "/usr/local/sbin"];

/// What the task's arguments ask for, with the reference's defaults.
struct Options {
    pre_reboot_delay: u64,
    post_reboot_delay: u64,
    reboot_timeout: u64,
    connect_timeout: Option<Duration>,
    test_command: String,
    boot_time_command: Option<String>,
    search_paths: Vec<String>,
    reboot_command: Option<String>,
    msg: String,
}

enum State {
    Start,
    Setup,
    BootId,
    Find { bin: String },
    Shutdown,
    Back,
    Test,
}

pub(super) struct Reboot<'a> {
    args: &'a Map<String, Value>,
    local: bool,
    state: State,
    options: Option<Options>,
    /// `(name, family)`, lowercased, off the `setup`.
    distribution: (String, String),
    /// The boot id read before the `shutdown`.
    previous: String,
    /// When the `shutdown` command returned: what `elapsed` counts from.
    started: Option<Instant>,
    /// When the wait in hand gives up.
    deadline: Option<Instant>,
}

impl<'a> Reboot<'a> {
    pub(super) fn new(ctx: Context<'a>) -> Self {
        Reboot {
            args: ctx.args,
            local: ctx.local,
            state: State::Start,
            options: None,
            distribution: (String::new(), String::new()),
            previous: String::new(),
            started: None,
            deadline: None,
        }
    }

    fn options(&self) -> &Options {
        self.options.as_ref().expect("read at the start")
    }

    /// `shutdown` unless the distribution has its own: the reference's table, the entries a Linux
    /// host can reach (the agent runs on nothing else).
    fn shutdown_bin(&self) -> String {
        match self.options().reboot_command.as_deref() {
            Some(command) => command.split(' ').next().unwrap_or("").to_string(),
            None if self.is("alpine") => "reboot".into(),
            None => "shutdown".into(),
        }
    }

    fn shutdown_args(&self) -> String {
        let options = self.options();
        if let Some(command) = &options.reboot_command {
            return command.split_once(' ').map_or("", |(_, rest)| rest).into();
        }
        let delay_min = options.pre_reboot_delay / 60;
        let message = &options.msg;
        if self.is("alpine") {
            String::new()
        } else if self.is("void") {
            format!("-r +{delay_min} \"{message}\"")
        } else {
            format!("-r {delay_min} \"{message}\"")
        }
    }

    /// Whether the distribution's name or family is `name`, which is how the reference's tables
    /// are looked up after the name with its major version.
    fn is(&self, name: &str) -> bool {
        self.distribution.0 == name || self.distribution.1 == name
    }

    fn boot_time_command(&self) -> Sub {
        raw(self
            .options()
            .boot_time_command
            .as_deref()
            .unwrap_or("cat /proc/sys/kernel/random/boot_id"))
    }

    fn elapsed(&self) -> u64 {
        self.started.map_or(0, |s| s.elapsed().as_secs())
    }

    /// Runs the shutdown command: the path `find` gave, or the one written.
    fn shutdown(&mut self, path: &str) -> Step {
        self.state = State::Shutdown;
        Step::RunDropping(raw(&format!("{path} {}", self.shutdown_args())))
    }

    /// Asks for the host again with `probe`, for what is left of the wait in hand.
    fn reconnect(&self, probe: Sub, wait: Duration) -> Step {
        let deadline = self.deadline.expect("set with the wait");
        Step::Reconnect {
            probe,
            wait,
            timeout: deadline.saturating_duration_since(Instant::now() + wait),
            attempt: self.options().connect_timeout,
        }
    }

    /// The failure a wait that ran out reports, in the reference's words.
    fn timed_out(&self, what: &str) -> Step {
        Step::Done(TaskResult(object(json!({
            "elapsed": self.elapsed(),
            "failed": true,
            "msg": format!(
                "Timed out waiting for {what} (timeout={})",
                self.options().reboot_timeout
            ),
            "rebooted": true,
        }))))
    }

    fn past_deadline(&self) -> bool {
        self.deadline.is_none_or(|d| Instant::now() >= d)
    }
}

impl Plugin for Reboot<'_> {
    fn next(&mut self, last: Option<TaskResult>) -> Step {
        match std::mem::replace(&mut self.state, State::Start) {
            State::Start => {
                // Before anything reaches the host, as in the reference: on the controller's own
                // connection there is only one machine to reboot, and it is this one.
                if self.local {
                    return Step::Done(TaskResult(object(json!({
                        "changed": false,
                        "elapsed": 0,
                        "failed": true,
                        "msg": "Running reboot with local connection would reboot the control node.",
                        "rebooted": false,
                    }))));
                }
                match read_options(self.args) {
                    Ok(options) => self.options = Some(options),
                    Err(msg) => return Step::Done(TaskResult::failed_with(msg)),
                }
                self.state = State::Setup;
                let mut args = Map::new();
                args.insert("gather_subset".into(), json!(["min"]));
                Sub::run("setup", args)
            }
            State::Setup => {
                let Some(setup) = last else {
                    return Step::Done(lost("setup"));
                };
                match distribution(&setup) {
                    Ok(found) => self.distribution = found,
                    Err(msg) => return Step::Done(TaskResult::failed_with(msg)),
                }
                self.state = State::BootId;
                Step::Run(self.boot_time_command())
            }
            State::BootId => {
                let Some(read) = last else {
                    return Step::Done(lost("raw"));
                };
                if rc(&read) != Some(0) {
                    // `reboot`, not `rebooted`: the reference's own key on this one failure.
                    return Step::Done(TaskResult(object(json!({
                        "failed": true,
                        "msg": format!(
                            "reboot: failed to get host boot time info, rc: {}, stdout: {}, stderr: {}",
                            rc(&read).map_or_else(|| "None".to_string(), |rc| rc.to_string()),
                            text(&read, "stdout"),
                            text(&read, "stderr"),
                        ),
                        "reboot": false,
                    }))));
                }
                self.previous = text(&read, "stdout").trim().to_string();
                let bin = self.shutdown_bin();
                if bin.starts_with('/') {
                    return self.shutdown(&bin);
                }
                let mut args = Map::new();
                args.insert("paths".into(), json!(self.options().search_paths));
                args.insert("patterns".into(), json!([bin]));
                args.insert("file_type".into(), json!("any"));
                self.state = State::Find { bin };
                Sub::run("find", args)
            }
            State::Find { bin } => {
                let Some(found) = last else {
                    return Step::Done(lost("find"));
                };
                let first = found
                    .0
                    .get("files")
                    .and_then(Value::as_array)
                    .and_then(|files| files.first())
                    .and_then(|f| f.get("path"))
                    .and_then(Value::as_str);
                let Some(path) = first else {
                    return Step::Done(TaskResult::failed_with(format!(
                        "Unable to find command \"{bin}\" in search paths: {}",
                        python_list(&self.options().search_paths)
                    )));
                };
                let path = path.to_string();
                self.shutdown(&path)
            }
            State::Shutdown => {
                let Some(ran) = last else {
                    return Step::Done(lost("raw"));
                };
                // The link going down under the command is the command working. A command that
                // answered and failed - no such program, no permission - fails the task now,
                // rather than waiting out the timeout for a reboot that will never happen.
                if !is_gone(&ran) && rc(&ran) != Some(0) {
                    return Step::Done(TaskResult(object(json!({
                        "elapsed": 0,
                        "failed": true,
                        "msg": format!(
                            "Reboot command failed. Error was: '{}, {}'",
                            text(&ran, "stdout").trim(),
                            text(&ran, "stderr").trim()
                        ),
                        "rebooted": false,
                    }))));
                }
                let now = Instant::now();
                let wait = Duration::from_secs(self.options().post_reboot_delay);
                let timeout = Duration::from_secs(self.options().reboot_timeout);
                self.started = Some(now);
                self.deadline = Some(now + wait + timeout);
                self.state = State::Back;
                self.reconnect(self.boot_time_command(), wait)
            }
            State::Back => {
                let Some(read) = last else {
                    return Step::Done(lost("raw"));
                };
                let id = text(&read, "stdout").trim();
                // An empty answer is not a new boot: measured by the reference on FreeBSD, which
                // answers nothing just before it goes down.
                if !is_gone(&read) && rc(&read) == Some(0) && !id.is_empty() && id != self.previous
                {
                    self.deadline =
                        Some(Instant::now() + Duration::from_secs(self.options().reboot_timeout));
                    self.state = State::Test;
                    return Step::RunDropping(raw(&self.options().test_command));
                }
                if self.past_deadline() {
                    return self.timed_out("last boot time check");
                }
                self.state = State::Back;
                self.reconnect(self.boot_time_command(), Duration::ZERO)
            }
            State::Test => {
                let Some(ran) = last else {
                    return Step::Done(lost("raw"));
                };
                if !is_gone(&ran) && rc(&ran) == Some(0) {
                    return Step::Done(TaskResult(object(json!({
                        "changed": true,
                        "elapsed": self.elapsed(),
                        "rebooted": true,
                    }))));
                }
                if self.past_deadline() {
                    return self.timed_out("post-reboot test command");
                }
                self.state = State::Test;
                self.reconnect(raw(&self.options().test_command), Duration::ZERO)
            }
        }
    }
}

fn raw(line: &str) -> Sub {
    let mut args = Map::new();
    args.insert("_raw_params".into(), Value::from(line));
    Sub {
        module: "raw",
        args,
        files: Vec::new(),
    }
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

fn rc(result: &TaskResult) -> Option<i64> {
    result.0.get("rc").and_then(Value::as_i64)
}

fn text<'r>(result: &'r TaskResult, key: &str) -> &'r str {
    result.0.get(key).and_then(Value::as_str).unwrap_or("")
}

/// The distribution's name and family, lowercased, off a `setup` of the `min` subset: the
/// reference's two failures when it cannot have them.
fn distribution(setup: &TaskResult) -> Result<(String, String), String> {
    if setup.failed() {
        return Err(format!(
            "Failed to determine system distribution. {}, {}",
            text(setup, "module_stdout").trim(),
            text(setup, "module_stderr").trim()
        ));
    }
    let facts = setup.0.get("ansible_facts");
    let fact = |name: &str| {
        facts
            .and_then(|f| f.get(name))
            .and_then(Value::as_str)
            .map(str::to_lowercase)
            .ok_or_else(|| {
                format!("Failed to get distribution information. Missing \"{name}\" in output.")
            })
    };
    let name = fact("ansible_distribution")?;
    fact("ansible_distribution_version")?;
    Ok((name, fact("ansible_os_family")?))
}

/// The task's arguments, read as the reference reads them: the delays are `int()`s clamped at
/// zero, the commands strings, `search_paths` a string or a list of them.
fn read_options(args: &Map<String, Value>) -> Result<Options, String> {
    let mut unknown: Vec<&str> = args
        .keys()
        .map(String::as_str)
        .filter(|k| !VALID_ARGS.contains(k))
        .collect();
    if !unknown.is_empty() {
        unknown.sort_unstable();
        return Err(format!("Invalid options for reboot: {}", unknown.join(",")));
    }
    let int = |name: &str, default: i64| -> Result<i64, String> {
        match args.get(name) {
            None => Ok(default),
            Some(value) => python_int(value),
        }
    };
    let delay = |name: &str| int(name, 0).map(|d| u64::try_from(d).unwrap_or(0));
    let string = |name: &str| -> Result<Option<String>, String> {
        match args.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(other) => Err(format!(
                "Invalid value given for '{name}': '{other}' is not a string and conversion is not allowed."
            )),
        }
    };
    let search_paths = match args.get("search_paths") {
        None => DEFAULT_SEARCH_PATHS
            .iter()
            .map(ToString::to_string)
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(list)) if list.iter().all(Value::is_string) => list
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(other) => {
            return Err(format!(
                "'search_paths' must be a string or flat list of strings, got {other}"
            ));
        }
    };
    let connect_timeout = match args.get("connect_timeout") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let seconds = match value {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.trim().parse::<f64>().ok(),
                _ => None,
            }
            .filter(|s| s.is_finite() && *s >= 0.0)
            .ok_or_else(|| format!("connect_timeout: {value} is not a number of seconds"))?;
            // `if connect_timeout:` in the reference: zero leaves the connection's own.
            (seconds > 0.0).then(|| Duration::from_secs_f64(seconds))
        }
    };
    let text_or_json = |name: &str, default: &str| match args.get(name) {
        None => default.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    Ok(Options {
        pre_reboot_delay: delay("pre_reboot_delay")?,
        post_reboot_delay: delay("post_reboot_delay")?,
        reboot_timeout: u64::try_from(int("reboot_timeout", 600)?).unwrap_or(0),
        connect_timeout,
        test_command: text_or_json("test_command", "whoami"),
        // `if self._task.args.get('boot_time_command'):` - an empty one is the default.
        boot_time_command: string("boot_time_command")?.filter(|c| !c.is_empty()),
        search_paths,
        reboot_command: string("reboot_command")?,
        msg: text_or_json("msg", "Reboot initiated by Ansible"),
    })
}

/// Python's `int()` of a task argument.
fn python_int(value: &Value) -> Result<i64, String> {
    match value {
        Value::Bool(b) => Ok(i64::from(*b)),
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f.trunc() as i64))
            .ok_or_else(|| format!("cannot convert {n} to an integer")),
        Value::String(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("invalid literal for int() with base 10: '{s}'")),
        other => Err(format!(
            "int() argument must be a string, a bytes-like object or a real number, not {other}"
        )),
    }
}

/// A list of strings as Python prints one, which is how the reference names the paths it searched.
fn python_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|i| format!("'{i}'")).collect();
    format!("[{}]", quoted.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(value: Value) -> Map<String, Value> {
        object(value)
    }

    /// The reference's defaults, for a `reboot: {}`: ten minutes, `whoami`, the five paths and
    /// the message the measured `shutdown` line carried.
    ///
    /// What would make this red: a default of this engine's own in place of one of `reboot.py`'s.
    #[test]
    fn the_defaults_are_the_reference_s() {
        let options = read_options(&Map::new()).expect("no arguments is fine");
        assert_eq!(options.reboot_timeout, 600);
        assert_eq!(options.pre_reboot_delay, 0);
        assert_eq!(options.post_reboot_delay, 0);
        assert_eq!(options.connect_timeout, None);
        assert_eq!(options.test_command, "whoami");
        assert_eq!(options.boot_time_command, None);
        assert_eq!(options.search_paths, DEFAULT_SEARCH_PATHS);
        assert_eq!(options.reboot_command, None);
        assert_eq!(options.msg, "Reboot initiated by Ansible");
    }

    /// An argument the reference does not accept fails the task naming it, and the delays read
    /// as `int()` reads them, a negative one as zero.
    #[test]
    fn the_arguments_are_read_as_the_reference_reads_them() {
        let Err(msg) = read_options(&map(json!({"reboot_wait": 5, "delay": 1}))) else {
            panic!("two unknown arguments");
        };
        assert_eq!(msg, "Invalid options for reboot: delay,reboot_wait");
        let options = read_options(&map(json!({
            "pre_reboot_delay": "120", "post_reboot_delay": -4, "reboot_timeout": 30.9,
            "connect_timeout": 5, "search_paths": "/opt/sbin", "msg": "kernel update"
        })))
        .expect("all valid");
        assert_eq!(options.pre_reboot_delay, 120);
        assert_eq!(options.post_reboot_delay, 0);
        assert_eq!(options.reboot_timeout, 30);
        assert_eq!(options.connect_timeout, Some(Duration::from_secs(5)));
        assert_eq!(options.search_paths, ["/opt/sbin"]);
        let Err(msg) = read_options(&map(json!({"pre_reboot_delay": "soon"}))) else {
            panic!("not an integer");
        };
        assert_eq!(msg, "invalid literal for int() with base 10: 'soon'");
    }

    /// The shutdown line, per distribution and per argument: minutes from `pre_reboot_delay`,
    /// `reboot` bare on Alpine, `+` minutes on Void, and a `reboot_command` split at its first
    /// space into the program and the rest.
    #[test]
    fn the_shutdown_line_follows_the_reference_s_tables() {
        let line = |args: Value, distribution: (&str, &str)| {
            let args = map(args);
            let plugin = Reboot {
                args: &args,
                local: false,
                state: State::Start,
                options: Some(read_options(&args).expect("valid")),
                distribution: (distribution.0.into(), distribution.1.into()),
                previous: String::new(),
                started: None,
                deadline: None,
            };
            (plugin.shutdown_bin(), plugin.shutdown_args())
        };
        assert_eq!(
            line(json!({"pre_reboot_delay": 150}), ("ubuntu", "debian")),
            (
                "shutdown".into(),
                "-r 2 \"Reboot initiated by Ansible\"".into()
            )
        );
        assert_eq!(
            line(json!({}), ("alpine", "alpine")),
            ("reboot".into(), String::new())
        );
        assert_eq!(
            line(json!({"msg": "bye"}), ("void", "void")),
            ("shutdown".into(), "-r +0 \"bye\"".into())
        );
        assert_eq!(
            line(
                json!({"reboot_command": "/usr/bin/systemctl reboot -i"}),
                ("ubuntu", "debian")
            ),
            ("/usr/bin/systemctl".into(), "reboot -i".into())
        );
    }
}
