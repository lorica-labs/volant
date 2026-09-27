// SPDX-License-Identifier: GPL-3.0-or-later
//! `systemd` (`systemd_service`), answered in the agent: ansible-core 2.19.12's
//! `systemd_service.py` step by step for `name`, `state`, `enabled` and `daemon_reload` on the
//! system scope. It runs the module's `systemctl` commands, in the module's order, with the
//! module's arguments, and reads their output the way the module reads it: `systemctl show`
//! before any action, then `is-enabled -l`, then the actions.
//!
//! Handed to the Python module before the first command that changes anything: `masked`,
//! `daemon_reexec`, `force`, `no_block`, a scope other than `system`, a value its validation
//! would convert or refuse, a unit name it would glob, split, quote or expand, and a host where
//! systemd runs offline or inside a chroot. A `systemctl show` that fails, a unit that is only an
//! init script and a chroot found on the way are handed back too while nothing has changed; after
//! a `daemon-reload`, which cannot be taken back, they are answered as the module answers them.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::modules::Context;
use crate::natives::Native;
use crate::natives::common::{
    ArgSpec, Clock, Stop, bool_param, check_names, clock, invocation, native_run, str_param,
};
use crate::natives::setup::{self, Root, py_space, py_strip};

pub const NATIVE: Native = Native {
    name: "systemd",
    aliases: &["systemd_service"],
    enabled: true,
    run: |args, context, cancelled| {
        let answer = match answer(args, context, clock(context, cancelled), "/etc") {
            Ok(result) | Err(Exit::Failed(result)) => Ok(result),
            Err(Exit::Stop(stop)) => Err(stop),
        };
        native_run(answer, context)
    },
};

/// How an answer ends before the module's last line: handed back, out of time, cancelled, or
/// with the module's own failure from deep inside a step.
#[derive(Debug)]
enum Exit {
    Stop(Stop),
    Failed(Map<String, Value>),
}

impl From<Stop> for Exit {
    fn from(stop: Stop) -> Exit {
        Exit::Stop(stop)
    }
}

impl From<String> for Exit {
    fn from(reason: String) -> Exit {
        Exit::Stop(Stop::HandBack(reason))
    }
}

impl From<&str> for Exit {
    fn from(reason: &str) -> Exit {
        Exit::Stop(Stop::HandBack(reason.to_string()))
    }
}

/// `systemd_service`'s `argument_spec` in ansible-core 2.19.12.
const SPEC: &[ArgSpec] = &[
    ArgSpec {
        name: "name",
        aliases: &["service", "unit"],
        default: || Value::Null,
    },
    ArgSpec {
        name: "state",
        aliases: &[],
        default: || Value::Null,
    },
    ArgSpec {
        name: "enabled",
        aliases: &[],
        default: || Value::Null,
    },
    ArgSpec {
        name: "force",
        aliases: &[],
        default: || Value::Null,
    },
    ArgSpec {
        name: "masked",
        aliases: &[],
        default: || Value::Null,
    },
    ArgSpec {
        name: "daemon_reload",
        aliases: &["daemon-reload"],
        default: || Value::Bool(false),
    },
    ArgSpec {
        name: "daemon_reexec",
        aliases: &["daemon-reexec"],
        default: || Value::Bool(false),
    },
    ArgSpec {
        name: "scope",
        aliases: &[],
        default: || Value::from("system"),
    },
    ArgSpec {
        name: "no_block",
        aliases: &[],
        default: || Value::Bool(false),
    },
];

const STATES: &[&str] = &["reloaded", "restarted", "started", "stopped"];

/// The `is-enabled` answers the module takes for a unit systemd knows, from `systemctl(1)` of
/// systemd 244.
const VALID_ENABLED_STATES: &[&str] = &[
    "enabled",
    "enabled-runtime",
    "linked",
    "linked-runtime",
    "masked",
    "masked-runtime",
    "static",
    "indirect",
    "disabled",
    "generated",
    "transient",
];

/// What the task asks, once the native knows it can answer it.
#[derive(Debug)]
struct Request<'a> {
    unit: Option<&'a str>,
    state: Option<&'a str>,
    enabled: Option<bool>,
    daemon_reload: bool,
}

/// The task's arguments, validated as the module validates them, or the reason to hand back.
///
/// `params` are the arguments with aliases resolved and defaults filled in, as `invocation`
/// prints them.
fn request<'a>(
    args: &Map<String, Value>,
    params: &'a Map<String, Value>,
) -> Result<Request<'a>, String> {
    check_names(SPEC, args)?;
    // The module converts `"yes"`, `1` and the like, and a number given as a string, and shows
    // the converted value in `invocation`; it also words the refusals. Only JSON's own types
    // are read here.
    let optional_bool = |name: &str| match &params[name] {
        Value::Null => Ok(None),
        _ => bool_param(params, name).map(Some),
    };
    let flag = |name: &str| bool_param(params, name);
    let unit = str_param(params, "name")?;
    let state = match &params["state"] {
        Value::Null => None,
        Value::String(state) if STATES.contains(&state.as_str()) => Some(state.as_str()),
        _ => return Err("state is not one of the module's choices".into()),
    };
    let enabled = optional_bool("enabled")?;
    if optional_bool("masked")?.is_some() {
        return Err("masked is left to the module".into());
    }
    if optional_bool("force")? == Some(true) {
        return Err("force is left to the module".into());
    }
    if flag("daemon_reexec")? {
        return Err("daemon_reexec is left to the module".into());
    }
    if flag("no_block")? {
        return Err("no_block is left to the module".into());
    }
    if params["scope"] != "system" {
        return Err("a scope other than system is left to the module".into());
    }
    // `required_one_of` never fires: the defaults of `daemon_reload` and `daemon_reexec` are set
    // before it counts the keys given, so a task with only a `name` reads the unit's status.
    let daemon_reload = flag("daemon_reload")?;
    if (state.is_some() || enabled.is_some()) && unit.is_none() {
        return Err("the module refuses state or enabled without a name".into());
    }
    if let Some(unit) = unit {
        // The module fails on a glob; it runs `systemctl <verb> '<unit>'` through `shlex.split`,
        // then `expanduser` and `expandvars` on each word; a path is also an init script's.
        if unit.is_empty()
            || unit.starts_with('~')
            || unit.contains(['*', '?', '[', '\'', '$', '/', '\0'])
        {
            return Err("the module reads this unit name differently".into());
        }
    }
    Ok(Request {
        unit,
        state,
        enabled,
        daemon_reload,
    })
}

/// The module's `systemctl`, and what bounds each run of it.
struct Host<'a> {
    systemctl: PathBuf,
    /// Set over the agent's environment for every command: the task's `environment`, and the
    /// `XDG_RUNTIME_DIR` the module adds when it has none.
    env: BTreeMap<String, String>,
    /// The module's `PATH`, for `get_bin_path`.
    path: BTreeMap<String, String>,
    clock: Clock<'a>,
}

impl Host<'_> {
    /// `module.run_command(...)` on `systemctl args...`: exit code, output, error output.
    fn systemctl(&self, reply: &Reply, args: &[&str]) -> Result<(i32, String, String), Exit> {
        self.run(reply, &self.systemctl, args)
    }

    /// A program that cannot be started is handed back while nothing has changed; after that it
    /// fails the way `run_command` fails when `Popen` raises.
    fn run(
        &self,
        reply: &Reply,
        program: &Path,
        args: &[&str],
    ) -> Result<(i32, String, String), Exit> {
        match setup::run_output(&self.env, self.clock, program, args)? {
            Ok(ran) => Ok(ran),
            Err(_) if !reply.touched => {
                Err(format!("{} could not be started", program.display()).into())
            }
            Err(errno) => {
                let program = program.to_string_lossy();
                let words: Vec<&str> = std::iter::once(program.as_ref())
                    .chain(args.iter().copied())
                    .collect();
                let mut failure = reply.fail("Error executing command.".into());
                failure.insert("rc".into(), Value::from(errno));
                failure.insert("stdout".into(), Value::from(""));
                failure.insert("stderr".into(), Value::from(""));
                failure.insert("cmd".into(), Value::from(clean_args(&words)));
                Err(Exit::Failed(failure))
            }
        }
    }

    /// `is_chroot(module)`: `/` against pid 1's root, or, when that cannot be read (not root),
    /// the inode of `/` against the file system's own root inode.
    fn is_chroot(&self, reply: &Reply) -> Result<bool, Exit> {
        let root = std::fs::metadata("/").map_err(|err| format!("/ cannot be read: {err}"))?;
        if let Ok(init) = std::fs::metadata("/proc/1/root/.") {
            return Ok(root.ino() != init.ino() || root.dev() != init.dev());
        }
        let mut fs_root_ino = 2;
        if let Some(stat) = setup::bin_path(&Root::real(), &self.path, "stat") {
            let (_, out, _) = self.run(reply, &stat, &["-f", "--format=%T", "/"])?;
            if out.contains("btrfs") {
                fs_root_ino = 256;
            } else if out.contains("xfs") {
                fs_root_ino = 128;
            }
        }
        Ok(root.ino() != fs_root_ino)
    }
}

/// What every result carries besides the module's own keys.
struct Reply {
    invocation: Value,
    warnings: Vec<String>,
    /// Whether a command that changes the host has run; from then on nothing is handed back.
    touched: bool,
}

impl Reply {
    fn finish(&self, mut result: Map<String, Value>) -> Map<String, Value> {
        if !self.warnings.is_empty() {
            result.insert("warnings".into(), Value::from(self.warnings.clone()));
        }
        result.insert("invocation".into(), self.invocation.clone());
        result
    }

    /// `module.fail_json(msg=...)`.
    fn fail(&self, msg: String) -> Map<String, Value> {
        let mut result = Map::new();
        result.insert("failed".into(), Value::Bool(true));
        result.insert("msg".into(), Value::String(msg));
        self.finish(result)
    }

    /// Hands the task back while nothing has changed; once something has, the native goes on
    /// the way the module does.
    fn outside(&self, reason: &str) -> Result<(), Stop> {
        if self.touched {
            Ok(())
        } else {
            Err(reason.into())
        }
    }
}

/// The module's result, or the reason to hand the task back. `etc` is `/etc` on a host, where
/// the module looks for init scripts.
fn answer(
    args: &Map<String, Value>,
    context: &Context,
    clock: Clock,
    etc: &str,
) -> Result<Map<String, Value>, Exit> {
    let invocation = invocation(SPEC, args);
    let params = invocation["module_args"].as_object().unwrap().clone();
    let request = request(args, &params)?;
    // The module's environment: the agent's, which the Python module starts with, and the
    // task's over it.
    let var =
        |name: &str| {
            context.environment.get(name).cloned().or_else(|| {
                std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
            })
        };
    if var("SYSTEMD_OFFLINE").as_deref() == Some("1")
        || var("debian_chroot").is_some_and(|value| !value.is_empty())
    {
        return Err("systemd runs offline here, or the host is a chroot".into());
    }
    let path = BTreeMap::from([("PATH".to_string(), var("PATH").unwrap_or_default())]);
    let systemctl = setup::bin_path(&Root::real(), &path, "systemctl")
        .ok_or("the module finds no systemctl")?;
    // The module splits `"<systemctl> <verb> '<unit>'"` with `shlex`: a path `shlex.quote` leaves
    // alone reads back as itself.
    if !systemctl
        .to_str()
        .is_some_and(|text| shlex_quote(text) == text)
    {
        return Err("the module reads this systemctl path differently".into());
    }
    let mut env = context.environment.clone();
    if let Some(dir) = xdg_default(&var, unsafe { libc::geteuid() }) {
        env.insert("XDG_RUNTIME_DIR".into(), dir);
    }
    let host = Host {
        systemctl,
        env,
        path,
        clock,
    };
    let mut reply = Reply {
        invocation,
        warnings: Vec::new(),
        touched: false,
    };

    let mut result = Map::new();
    result.insert("name".into(), request.unit.map_or(Value::Null, Value::from));
    result.insert("changed".into(), Value::Bool(false));
    let mut status = Map::new();

    if request.daemon_reload {
        let (rc, _, err) = host.systemctl(&reply, &["daemon-reload"])?;
        if rc != 0 {
            // A reload that failed changed nothing: handing back is still safe here.
            if host.is_chroot(&reply)? {
                return Err("daemon-reload failed in a chroot".into());
            }
            return Ok(reply.fail(format!("failure {rc} during daemon-reload: {err}")));
        }
        reply.touched = true;
    }

    if let Some(unit) = request.unit {
        let is_initd = Path::new(&format!("{etc}/init.d/{unit}")).exists();
        let mut is_systemd = false;
        // Read before any action: the module reports the unit as it found it.
        let (rc, out, err) = host.systemctl(&reply, &["show", unit])?;
        if rc == 0 && !(request_was_ignored(&out) || request_was_ignored(&err)) {
            if !out.is_empty() {
                status = parse_systemctl_show(&out);
                let load_state = status.get("LoadState").and_then(Value::as_str);
                is_systemd = load_state.is_some_and(|state| state != "not-found");
                let is_masked = load_state == Some("masked");
                if is_systemd
                    && !is_masked
                    && let Some(error) = status.get("LoadError").and_then(Value::as_str)
                {
                    return Ok(reply.fail(format!("Error loading unit file '{unit}': {error}")));
                }
            }
        } else if !err.is_empty() && rc == 1 && err.contains("Failed to parse bus message") {
            reply.outside("systemctl show could not parse a bus message")?;
            status = parse_systemctl_show(&out);
            let search = match unit.split_once('@') {
                Some((base, _)) => format!("{base}@"),
                None => unit.to_string(),
            };
            let (_, out, _) =
                host.systemctl(&reply, &["list-unit-files", &format!("{search}*")])?;
            is_systemd = out.contains(&search);
            let (_, out, _) = host.systemctl(&reply, &["is-active", unit])?;
            status.insert(
                "ActiveState".into(),
                Value::from(out.trim_end_matches('\n')),
            );
        } else {
            reply.outside("systemctl show failed")?;
            let (_, out, _) = host.systemctl(&reply, &["is-enabled", unit])?;
            if VALID_ENABLED_STATES.contains(&py_strip(&out)) {
                is_systemd = true;
            } else {
                let (rc, _, _) = host.systemctl(&reply, &["list-unit-files", unit])?;
                if rc == 0 {
                    is_systemd = true;
                } else {
                    // `module.run_command(systemctl, check_rc=True)`.
                    let (rc, out, err) = host.systemctl(&reply, &[])?;
                    if rc != 0 {
                        // `stderr` is still bytes there: `rstrip()` strips ASCII whitespace.
                        let msg = err.trim_end_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
                        let mut failure = reply.fail(msg.to_string());
                        let systemctl = host.systemctl.to_string_lossy();
                        failure.insert("cmd".into(), Value::from(clean_args(&[&systemctl])));
                        failure.insert("rc".into(), Value::from(rc));
                        failure.insert("stdout".into(), Value::from(out));
                        failure.insert("stderr".into(), Value::from(err));
                        return Ok(failure);
                    }
                }
            }
        }

        let found = is_systemd || is_initd;
        if is_initd && !is_systemd {
            reply.outside("the unit is only an init script")?;
            reply.warnings.push(format!(
                "The service ({unit}) is actually an init script but the system is managed by \
                 systemd"
            ));
        }

        if let Some(wanted) = request.enabled {
            let action = if wanted { "enable" } else { "disable" };
            if !found {
                return Ok(reply.fail(reply_missing(unit)));
            }
            let (rc, out, _) = host.systemctl(&reply, &["is-enabled", unit, "-l"])?;
            let mut enabled = false;
            if rc == 0 {
                // Enabled for the module unless systemd says it is so only for now, through
                // another unit, or as an alias: those are enabled again.
                enabled = !matches!(
                    out.trim_end_matches(py_space),
                    "enabled-runtime" | "indirect" | "alias"
                );
            } else if rc == 1
                && is_initd
                && !py_strip(&out).ends_with("disabled")
                && sysv_is_enabled(etc, unit)
            {
                enabled = true;
            }
            result.insert("enabled".into(), Value::Bool(enabled));
            if enabled != wanted {
                result.insert("changed".into(), Value::Bool(true));
                reply.touched = true;
                let (rc, out, err) = host.systemctl(&reply, &[action, unit])?;
                if rc != 0 {
                    return Ok(reply.fail(format!("Unable to {action} service {unit}: {out}{err}")));
                }
                result.insert("enabled".into(), Value::Bool(!enabled));
            }
        }

        if let Some(state) = request.state {
            if !found {
                return Ok(reply.fail(reply_missing(unit)));
            }
            result.insert("state".into(), Value::from(state));
            if let Some(active) = status.get("ActiveState") {
                let running = *active == "active" || *active == "activating";
                let action = match state {
                    "started" => (!running).then_some("start"),
                    "stopped" => (running || *active == "deactivating").then_some("stop"),
                    _ => {
                        result.insert("state".into(), Value::from("started"));
                        Some(match (running, state) {
                            (false, _) => "start",
                            (true, "restarted") => "restart",
                            (true, _) => "reload",
                        })
                    }
                };
                if let Some(action) = action {
                    result.insert("changed".into(), Value::Bool(true));
                    reply.touched = true;
                    let (rc, _, err) = host.systemctl(&reply, &[action, unit])?;
                    if rc != 0 {
                        return Ok(reply.fail(format!("Unable to {action} service {unit}: {err}")));
                    }
                }
            } else if host.is_chroot(&reply)? {
                reply.outside("the host is a chroot")?;
                reply.warnings.push(
                    "Target is a chroot or systemd is offline. This can lead to false positives \
                     or prevent the init system tools from working."
                        .into(),
                );
            } else {
                let mut failure = reply.fail("Service is in unknown state".into());
                failure.insert("status".into(), Value::Object(status));
                return Ok(failure);
            }
        }
    }

    result.insert("status".into(), Value::Object(status));
    Ok(reply.finish(result))
}

/// The `XDG_RUNTIME_DIR` the module sets when `os.getenv` finds none, even an empty one.
fn xdg_default(var: impl Fn(&str) -> Option<String>, euid: u32) -> Option<String> {
    var("XDG_RUNTIME_DIR")
        .is_none()
        .then(|| format!("/run/user/{euid}"))
}

/// `AnsibleModule._clean_args(args)`, the `cmd` of a failed `run_command`: the word after one
/// that starts `pass` (after up to two `-`) masked, or the part after its `=`, then each word
/// `shlex.quote`d. `heuristic_log_sanitize` is not applied: it only rewrites `user:pass@` URLs.
fn clean_args(words: &[&str]) -> String {
    let mut clean = Vec::new();
    let mut masked = false;
    for word in words {
        if masked {
            masked = false;
            clean.push("********".to_string());
            continue;
        }
        let bare = word
            .strip_prefix("--")
            .or_else(|| word.strip_prefix('-'))
            .unwrap_or(word);
        if bare.starts_with("pass") {
            if let Some(at) = word.find('=') {
                clean.push(format!("{}=********", &word[..at]));
                continue;
            }
            masked = true;
        }
        clean.push((*word).to_string());
    }
    clean
        .iter()
        .map(|word| shlex_quote(word))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `shlex.quote`.
fn shlex_quote(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
    {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\"'\"'"))
    }
}

/// `fail_if_missing(module, found, unit, msg='host')`'s message.
fn reply_missing(unit: &str) -> String {
    format!("Could not find the requested service {unit}: host")
}

/// `request_was_ignored(out)`.
fn request_was_ignored(out: &str) -> bool {
    !out.contains('=') && (out.contains("ignoring request") || out.contains("ignoring command"))
}

/// `parse_systemctl_show(out.split('\n'))`: one value per `Key=value` line, stripped; a value
/// spans lines only for an `Exec*` key whose value opens a `{` it does not close on that line,
/// up to the line that ends with `}`. A value left open at the end of the output is dropped.
fn parse_systemctl_show(out: &str) -> Map<String, Value> {
    let mut parsed = Map::new();
    let mut multival: Vec<&str> = Vec::new();
    let mut key: Option<&str> = None;
    for line in out.split('\n') {
        match key {
            None => {
                if let Some((k, v)) = line.split_once('=') {
                    if k.starts_with("Exec")
                        && v.trim_start_matches(py_space).starts_with('{')
                        && !v.trim_end_matches(py_space).ends_with('}')
                    {
                        multival.push(v);
                        key = Some(k);
                        continue;
                    }
                    parsed.insert(k.to_string(), Value::from(py_strip(v)));
                }
            }
            Some(k) => {
                multival.push(line);
                if line.trim_end_matches(py_space).ends_with('}') {
                    parsed.insert(k.to_string(), Value::from(py_strip(&multival.join("\n"))));
                    multival.clear();
                    key = None;
                }
            }
        }
    }
    parsed
}

/// `sysv_is_enabled(name)`: whether any runlevel directory holds a start link `S??<name>`,
/// under `<etc>/rc?.d/`, or `<etc>/init.d/rc?.d/` where `<etc>/rc0.d/` is no directory.
fn sysv_is_enabled(etc: &str, name: &str) -> bool {
    let base = if Path::new(&format!("{etc}/rc0.d/")).is_dir() {
        etc.to_string()
    } else {
        format!("{etc}/init.d")
    };
    let one_char = |text: &str| text.chars().count() == 1;
    let Ok(dirs) = std::fs::read_dir(base) else {
        return false;
    };
    dirs.flatten().any(|dir| {
        let runlevel = dir.file_name();
        let is_rc = runlevel.to_str().is_some_and(|runlevel| {
            runlevel
                .strip_prefix("rc")
                .and_then(|rest| rest.strip_suffix(".d"))
                .is_some_and(one_char)
        });
        is_rc
            && std::fs::read_dir(dir.path()).is_ok_and(|links| {
                links.flatten().any(|link| {
                    link.file_name().to_str().is_some_and(|link| {
                        link.strip_prefix('S')
                            .and_then(|rest| rest.strip_suffix(name))
                            .is_some_and(|middle| middle.chars().count() == 2)
                    })
                })
            })
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use serde_json::json;
    use volant_protocol::TaskResult;

    use super::*;
    use crate::natives::NativeRun;
    use crate::natives::common::golden::within;
    use crate::natives::setup::unbounded;

    /// `systemctl show cron` on an Ubuntu 24.04 host (systemd 255), with its `InvocationID`
    /// replaced. 271 lines, none of them multi-line.
    const SHOW_CRON: &str = include_str!("systemd_show_cron.txt");

    /// A directory holding a fake `systemctl` first on the module's `PATH`, which appends each
    /// call's arguments to `calls` and runs `body` (`sh`), exiting 0 unless `body` exits itself,
    /// and an `etc` without init scripts.
    struct Fake(String);

    impl Fake {
        fn new(name: &str, body: &str) -> Fake {
            let dir = format!("/tmp/volant-systemd-{name}-{}", std::process::id());
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(format!("{dir}/bin")).unwrap();
            fs::create_dir_all(format!("{dir}/etc/init.d")).unwrap();
            fs::write(format!("{dir}/show"), SHOW_CRON).unwrap();
            let script = format!("{dir}/bin/systemctl");
            fs::write(
                &script,
                format!("#!/bin/sh\necho \"$*\" >> {dir}/calls\ncd {dir}\n{body}\ntrue\n"),
            )
            .unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            Fake(dir)
        }

        fn context(&self) -> Context {
            Context {
                environment: BTreeMap::from([(
                    "PATH".to_string(),
                    format!("{}/bin:/usr/bin:/bin", self.0),
                )]),
                ..Context::default()
            }
        }

        fn etc(&self) -> String {
            format!("{}/etc", self.0)
        }

        /// The native's answer, a failure from inside a step included, as `NATIVE.run` gives it.
        fn answer(&self, args: Value) -> Result<Map<String, Value>, Exit> {
            match answer(
                args.as_object().unwrap(),
                &self.context(),
                unbounded(),
                &self.etc(),
            ) {
                Err(Exit::Failed(result)) => Ok(result),
                other => other,
            }
        }

        fn calls(&self) -> Vec<String> {
            fs::read_to_string(format!("{}/calls", self.0))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A `systemctl` that answers `show` with cron's output, `is-enabled` with `enabled`, and
    /// succeeds at everything else.
    const CRON: &str = r#"case "$1" in
  show) cat show ;;
  is-enabled) echo enabled ;;
esac"#;

    /// Compares an answer with the golden recording of the same case, every key but `status`,
    /// which is the host's own and is checked by each test.
    fn assert_like_recording(recording: &str, answer: &Map<String, Value>) {
        let mut want: Map<String, Value> = serde_json::from_str(recording).unwrap();
        for key in ["action", "_ansible_no_log", "exception", "_after", "status"] {
            want.remove(key);
        }
        let mut ours = crate::modules::module_result(answer.clone()).0;
        ours.remove("status");
        assert_eq!(Value::Object(ours), Value::Object(want));
    }

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    /// The module's split of a real `systemctl show`, and the two shapes around it: a value that
    /// opens `{` and closes it lines later under an `Exec*` key, and a `{` that stays open under
    /// another key, which is a single line.
    ///
    /// What would make this red: values kept unstripped; every `{` read as multi-line, which
    /// swallows the rest of the output after such a `Description`; the lines of a multi-line
    /// value joined with something else than `\n`; a line without `=` kept as a key.
    #[test]
    fn systemctl_show_splits_like_the_module() {
        let status = parse_systemctl_show(SHOW_CRON);
        assert_eq!(status.len(), 271);
        assert_eq!(status["ActiveState"], "active");
        assert_eq!(status["LoadState"], "loaded");
        assert_eq!(status["UnitFileState"], "enabled");
        assert_eq!(status["Id"], "cron.service");
        assert_eq!(
            status["ExecStart"],
            "{ path=/usr/sbin/cron ; argv[]=/usr/sbin/cron -f -P $EXTRA_OPTS ; \
             ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; \
             status=0/0 }"
        );
        assert_eq!(
            Value::Object(parse_systemctl_show(
                "Description={ open brace  \nExecStart={ path=/a ;\n  argv[]=/a }  \n\
                 no equals sign\nExecStop={ one line }\nExecReload={ never closed\nId=x\n"
            )),
            json!({
                "Description": "{ open brace",
                "ExecStart": "{ path=/a ;\n  argv[]=/a }",
                "ExecStop": "{ one line }",
            })
        );
    }

    /// `systemd-started-same`: cron running and enabled, asked `started` and `enabled`. Two
    /// reads, no action, `changed: false`, and the status as `show` gave it.
    ///
    /// What would make this red: a `start` or an `enable` run on a unit already so; a missing
    /// `-l`; `invocation` without a default the reference prints.
    #[test]
    fn a_running_enabled_unit_is_left_alone() {
        let fake = Fake::new("same", CRON);
        let answer = fake
            .answer(json!({"name": "cron", "state": "started", "enabled": true}))
            .unwrap();
        assert_like_recording(
            include_str!("../../../volant/tests/golden/native/systemd-started-same.json"),
            &answer,
        );
        assert_eq!(
            Value::Object(answer["status"].as_object().unwrap().clone()),
            Value::Object(parse_systemctl_show(SHOW_CRON))
        );
        assert_eq!(fake.calls(), ["show cron", "is-enabled cron -l"]);
    }

    /// `systemd-enabled-only`: `enabled` alone reads the unit and changes nothing.
    #[test]
    fn enabled_alone_reads_and_does_not_start() {
        let fake = Fake::new("enabled-only", CRON);
        let answer = fake
            .answer(json!({"name": "cron.service", "enabled": true}))
            .unwrap();
        assert_like_recording(
            include_str!("../../../volant/tests/golden/native/systemd-enabled-only.json"),
            &answer,
        );
        assert_eq!(
            fake.calls(),
            ["show cron.service", "is-enabled cron.service -l"]
        );
    }

    /// `systemd-daemon-reload`: no unit, one reload, and the module's empty answer.
    #[test]
    fn a_daemon_reload_alone_answers_nothing_else() {
        let fake = Fake::new("reload", "");
        let answer = fake.answer(json!({"daemon_reload": true})).unwrap();
        let mut want: Map<String, Value> = serde_json::from_str(include_str!(
            "../../../volant/tests/golden/native/systemd-daemon-reload.json"
        ))
        .unwrap();
        want.remove("action");
        want.remove("_ansible_no_log");
        assert_eq!(Value::Object(answer), Value::Object(want));
        assert_eq!(fake.calls(), ["daemon-reload"]);
    }

    /// `systemd-missing-unit`: `show` succeeds with `LoadState=not-found`, no init script, and
    /// `state` fails with the module's message before any action.
    #[test]
    fn a_missing_unit_fails_like_the_module() {
        let fake = Fake::new(
            "missing",
            r#"[ "$1" = show ] && printf 'LoadState=not-found\nActiveState=inactive\n'"#,
        );
        let answer = fake
            .answer(json!({"name": "volant-no-such-unit", "state": "started"}))
            .unwrap();
        assert_like_recording(
            include_str!("../../../volant/tests/golden/native/systemd-missing-unit.json"),
            &answer,
        );
        assert_eq!(fake.calls(), ["show volant-no-such-unit"]);
    }

    /// A unit whose `MainPID` changes on `restart`: the status is the one `show` gave before the
    /// action, `changed` is true and `state` is `started`, as for `reloaded`.
    ///
    /// What would make this red: `show` run after the action (the new `MainPID`); `changed`
    /// false for a restart; `state` left `restarted`.
    #[test]
    fn restarted_and_reloaded_report_the_unit_as_found() {
        let body = r#"case "$1" in
  show) printf 'LoadState=loaded\nActiveState=active\nMainPID=%s\n' "$(cat pid 2>/dev/null || echo 100)" ;;
  restart) echo 200 > pid ;;
esac"#;
        for (state, action) in [("restarted", "restart"), ("reloaded", "reload")] {
            let fake = Fake::new(state, body);
            let answer = fake
                .answer(json!({"name": "volant-golden", "state": state}))
                .unwrap();
            assert_eq!(answer["changed"], true, "{state}");
            assert_eq!(answer["state"], "started", "{state}");
            assert_eq!(answer["status"]["MainPID"], "100", "{state}");
            assert_eq!(
                fake.calls(),
                [
                    "show volant-golden".to_string(),
                    format!("{action} volant-golden")
                ]
            );
        }
    }

    /// Each state against a running and a stopped unit: the action the module takes, or none.
    #[test]
    fn each_state_acts_only_when_the_unit_differs() {
        for (active, state, action) in [
            ("inactive", "started", Some("start")),
            ("active", "stopped", Some("stop")),
            ("deactivating", "stopped", Some("stop")),
            ("inactive", "stopped", None),
            ("activating", "started", None),
            ("inactive", "restarted", Some("start")),
            ("failed", "reloaded", Some("start")),
        ] {
            let fake = Fake::new(
                "states",
                &format!(r#"[ "$1" = show ] && printf 'LoadState=loaded\nActiveState={active}\n'"#),
            );
            let answer = fake.answer(json!({"name": "u", "state": state})).unwrap();
            assert_eq!(answer["changed"], action.is_some(), "{active} {state}");
            let mut calls = vec!["show u".to_string()];
            calls.extend(action.map(|action| format!("{action} u")));
            assert_eq!(fake.calls(), calls, "{active} {state}");
        }
    }

    /// `enabled` against what `is-enabled -l` says, and the failure of the action.
    #[test]
    fn enabled_follows_is_enabled_like_the_module() {
        for (said, rc, wanted, action) in [
            ("enabled", 0, false, Some("disable")),
            ("static", 0, true, None),
            ("enabled-runtime", 0, true, Some("enable")),
            ("indirect", 0, true, Some("enable")),
            ("alias", 0, true, Some("enable")),
            ("disabled", 1, true, Some("enable")),
            ("disabled", 1, false, None),
        ] {
            let fake = Fake::new(
                "enabled",
                &format!(
                    r#"case "$1" in show) cat show ;; is-enabled) echo {said}; exit {rc} ;; esac"#
                ),
            );
            let answer = fake
                .answer(json!({"name": "u", "enabled": wanted}))
                .unwrap();
            assert_eq!(answer["changed"], action.is_some(), "{said} {wanted}");
            assert_eq!(answer["enabled"], wanted, "{said} {wanted}");
            let mut calls = vec!["show u".to_string(), "is-enabled u -l".to_string()];
            calls.extend(action.map(|action| format!("{action} u")));
            assert_eq!(fake.calls(), calls, "{said} {wanted}");
        }
        let fake = Fake::new(
            "enable-fails",
            r#"case "$1" in
  show) cat show ;;
  is-enabled) echo disabled; exit 1 ;;
  enable) echo out; echo err >&2; exit 1 ;;
esac"#,
        );
        let answer = fake.answer(json!({"name": "u", "enabled": true})).unwrap();
        assert_eq!(answer["failed"], true);
        assert_eq!(answer["msg"], "Unable to enable service u: out\nerr\n");
        assert!(answer.get("changed").is_none(), "fail_json has no changed");
    }

    /// An init script with start links counts as enabled where `is-enabled` exits 1 without
    /// saying `disabled`, as `sysv_is_enabled` reads the runlevel directories.
    #[test]
    fn an_init_script_with_start_links_is_enabled() {
        let fake = Fake::new(
            "sysv",
            r#"case "$1" in show) cat show ;; is-enabled) echo masked-ish; exit 1 ;; esac"#,
        );
        let etc = fake.etc();
        fs::write(format!("{etc}/init.d/u"), "").unwrap();
        let answer = fake.answer(json!({"name": "u", "enabled": true})).unwrap();
        assert_eq!(answer["changed"], true, "no start link: enabled false");
        fs::create_dir_all(format!("{etc}/rc0.d")).unwrap();
        fs::create_dir_all(format!("{etc}/rc2.d")).unwrap();
        fs::write(format!("{etc}/rc2.d/S01u"), "").unwrap();
        let answer = fake.answer(json!({"name": "u", "enabled": true})).unwrap();
        assert_eq!(answer["changed"], false, "a start link: enabled");
        assert!(!sysv_is_enabled(&etc, "v"));
        fs::write(format!("{etc}/rc2.d/S1v"), "").unwrap();
        assert!(!sysv_is_enabled(&etc, "v"), "S? is not S??");
    }

    /// A unit that fails to load, and is not masked, fails with the module's message.
    #[test]
    fn a_load_error_fails_like_the_module() {
        let fake = Fake::new(
            "load-error",
            r#"printf 'LoadState=bad-setting\nLoadError=org.freedesktop.systemd1.BadUnitSetting "Unit u.service has a bad unit file setting."\n'"#,
        );
        let answer = fake
            .answer(json!({"name": "u", "state": "started"}))
            .unwrap();
        assert_eq!(
            answer["msg"],
            "Error loading unit file 'u': org.freedesktop.systemd1.BadUnitSetting \"Unit \
             u.service has a bad unit file setting.\""
        );
        assert_eq!(fake.calls(), ["show u"]);
    }

    /// Every case the native leaves to the module is handed back before `systemctl` runs at
    /// all, and those found by `show` before anything changes.
    ///
    /// What would make this red: `masked` answered (the module masks; the native would start a
    /// masked unit); a scope, a flag or a spelling the module reads differently taken as given.
    #[test]
    fn what_the_native_leaves_to_the_module_is_handed_back_first() {
        for refused in [
            json!({"name": "u", "masked": true}),
            json!({"name": "u", "masked": false, "state": "started"}),
            json!({"name": "u", "state": "started", "scope": "user"}),
            json!({"name": "u", "state": "started", "scope": "nope"}),
            json!({"daemon_reexec": true}),
            json!({"daemon-reexec": true, "daemon_reload": true}),
            json!({"name": "u", "state": "started", "force": true}),
            json!({"name": "u", "state": "started", "no_block": true}),
            json!({"name": "u*", "state": "started"}),
            json!({"name": "u?", "state": "started"}),
            json!({"name": "u[1]", "state": "started"}),
            json!({"name": "u'x", "state": "started"}),
            json!({"name": "$HOME", "state": "started"}),
            json!({"name": "~u", "state": "started"}),
            json!({"name": "/etc/init.d/u", "state": "started"}),
            json!({"name": "", "state": "started"}),
            json!({"name": "u", "state": "running"}),
            json!({"name": "u", "enabled": "yes"}),
            json!({"name": "u", "daemon_reload": "yes"}),
            json!({"name": 1, "state": "started"}),
            json!({"name": "u", "service": "u", "state": "started"}),
            json!({"name": "u", "state": "started", "other": 1}),
            json!({"state": "started"}),
        ] {
            let fake = Fake::new("refused", CRON);
            assert!(
                matches!(
                    fake.answer(refused.clone()),
                    Err(Exit::Stop(Stop::HandBack(_)))
                ),
                "{refused}"
            );
            assert!(fake.calls().is_empty(), "{refused}");
        }
        for (body, init_script) in [
            ("exit 1", false),
            ("echo 'Failed to parse bus message' >&2; exit 1", false),
            (
                r#"[ "$1" = show ] && printf 'LoadState=not-found\nActiveState=inactive\n'"#,
                true,
            ),
        ] {
            let fake = Fake::new("show-outside", body);
            if init_script {
                fs::write(format!("{}/init.d/u", fake.etc()), "").unwrap();
            }
            assert!(
                matches!(
                    fake.answer(json!({"name": "u", "state": "started"})),
                    Err(Exit::Stop(Stop::HandBack(_)))
                ),
                "{body}"
            );
            assert_eq!(fake.calls(), ["show u"], "{body}");
        }
    }

    /// The same cases after a `daemon-reload`, which cannot be handed back: answered the way the
    /// module answers them.
    #[test]
    fn after_a_reload_the_module_s_other_branches_are_followed() {
        let fake = Fake::new(
            "bus",
            r#"case "$1" in
  show) echo 'Id=u@1.service'; echo 'Failed to parse bus message' >&2; exit 1 ;;
  list-unit-files) echo 'u@.service enabled' ;;
  is-active) echo inactive ;;
esac"#,
        );
        let answer = fake
            .answer(json!({"name": "u@1", "state": "started", "daemon_reload": true}))
            .unwrap();
        assert_eq!(
            answer["status"],
            json!({"Id": "u@1.service", "ActiveState": "inactive"})
        );
        assert_eq!(answer["changed"], true);
        assert_eq!(
            fake.calls(),
            [
                "daemon-reload",
                "show u@1",
                "list-unit-files u@*",
                "is-active u@1",
                "start u@1"
            ]
        );

        let fake = Fake::new(
            "show-fails",
            r#"case "$1" in
  show) exit 1 ;;
  is-enabled) echo bogus ;;
  list-unit-files) exit 1 ;;
  '') echo listed; printf 'broken\034 \n' >&2; exit 3 ;;
esac"#,
        );
        let answer = fake
            .answer(json!({"name": "u", "state": "started", "daemon_reload": true}))
            .unwrap();
        let systemctl = format!("{}/bin/systemctl", fake.0);
        assert_eq!(
            Value::Object(answer),
            json!({
                // `bytes.rstrip()` keeps the `\x1c` `str.strip()` would take.
                "failed": true, "msg": "broken\u{1c}", "cmd": systemctl, "rc": 3,
                "stdout": "listed\n", "stderr": "broken\u{1c} \n",
                "invocation": invocation(SPEC, &args(
                    json!({"name": "u", "state": "started", "daemon_reload": true})
                )),
            })
        );

        let fake = Fake::new(
            "sysv-only",
            r#"[ "$1" = show ] && printf 'LoadState=not-found\nActiveState=inactive\n'"#,
        );
        fs::write(format!("{}/init.d/u", fake.etc()), "").unwrap();
        let answer = fake
            .answer(json!({"name": "u", "state": "started", "daemon_reload": true}))
            .unwrap();
        assert_eq!(
            answer["warnings"],
            json!([
                "The service (u) is actually an init script but the system is managed by systemd"
            ])
        );
        assert_eq!(fake.calls(), ["daemon-reload", "show u", "start u"]);

        let fake = Fake::new(
            "no-active-state",
            r#"[ "$1" = show ] && echo LoadState=loaded"#,
        );
        let answer = fake
            .answer(json!({"name": "u", "state": "started", "daemon_reload": true}))
            .unwrap();
        assert_eq!(answer["msg"], "Service is in unknown state");
        assert_eq!(answer["status"], json!({"LoadState": "loaded"}));

        let fake = Fake::new("reload-fails", "echo nope >&2; exit 4");
        let answer = fake.answer(json!({"daemon_reload": true})).unwrap();
        assert_eq!(answer["msg"], "failure 4 during daemon-reload: nope\n");
    }

    /// The module's environment decides: offline or chroot hands back before anything runs,
    /// and `XDG_RUNTIME_DIR` reaches `systemctl` when the module has to set it.
    #[test]
    fn the_module_s_environment_is_the_native_s() {
        let fake = Fake::new("env", r#"echo "xdg=$XDG_RUNTIME_DIR" >> calls; cat show"#);
        for (key, value) in [("SYSTEMD_OFFLINE", "1"), ("debian_chroot", "x")] {
            let mut context = fake.context();
            context.environment.insert(key.into(), value.into());
            assert!(matches!(
                answer(
                    &args(json!({"name": "u", "state": "started"})),
                    &context,
                    unbounded(),
                    &fake.etc()
                ),
                Err(Exit::Stop(Stop::HandBack(_)))
            ));
        }
        assert!(fake.calls().is_empty());
        let mut context = fake.context();
        context
            .environment
            .insert("XDG_RUNTIME_DIR".into(), "/run/user/4242".into());
        answer(
            &args(json!({"name": "u", "state": "started"})),
            &context,
            unbounded(),
            &fake.etc(),
        )
        .unwrap();
        assert_eq!(fake.calls(), ["show u", "xdg=/run/user/4242"]);
    }

    /// A task with only a `name` reads the unit and answers its status, and an empty one answers
    /// nothing: the module's `required_one_of` counts the defaults it has just set.
    ///
    /// What would make this red: the old "nothing asked" hand-back; any command but `show`.
    #[test]
    fn a_status_only_task_reads_the_unit() {
        let fake = Fake::new("status-only", CRON);
        let answer = fake.answer(json!({"name": "cron"})).unwrap();
        assert_eq!(answer["changed"], false);
        assert_eq!(answer["name"], "cron");
        assert!(answer.get("state").is_none() && answer.get("enabled").is_none());
        assert_eq!(
            answer["status"],
            Value::Object(parse_systemctl_show(SHOW_CRON))
        );
        assert_eq!(fake.calls(), ["show cron"]);

        let fake = Fake::new("empty", CRON);
        let answer = fake.answer(json!({})).unwrap();
        assert_eq!(
            Value::Object(answer),
            json!({"changed": false, "name": null, "status": {},
                   "invocation": invocation(SPEC, &Map::new())})
        );
        assert!(fake.calls().is_empty());
    }

    /// The aliases answer like the canonical names, and `invocation` carries both spellings.
    ///
    /// What would make this red: an alias read as unknown (a hand-back), or dropped from
    /// `module_args`, or the canonical key left at its default.
    #[test]
    fn the_aliases_answer_and_are_kept_in_the_invocation() {
        for alias in ["unit", "service"] {
            let fake = Fake::new(alias, CRON);
            let task = format!(r#"{{"{alias}": "cron", "enabled": true}}"#);
            let answer = fake.answer(serde_json::from_str(&task).unwrap()).unwrap();
            let module_args = &answer["invocation"]["module_args"];
            assert_eq!(module_args[alias], "cron");
            assert_eq!(module_args["name"], "cron");
            assert_eq!(answer["name"], "cron");
            assert_eq!(fake.calls(), ["show cron", "is-enabled cron -l"]);
        }
        let fake = Fake::new("reload-alias", "");
        let answer = fake.answer(json!({"daemon-reload": true})).unwrap();
        let module_args = &answer["invocation"]["module_args"];
        assert_eq!(module_args["daemon-reload"], true);
        assert_eq!(module_args["daemon_reload"], true);
        assert_eq!(fake.calls(), ["daemon-reload"]);
    }

    /// `XDG_RUNTIME_DIR` is set only when `os.getenv` finds none; an empty one stays.
    #[test]
    fn xdg_runtime_dir_is_added_only_when_missing() {
        assert_eq!(xdg_default(|_| None, 42), Some("/run/user/42".into()));
        assert_eq!(xdg_default(|_| Some(String::new()), 42), None);
        assert_eq!(xdg_default(|_| Some("/run/user/7".into()), 42), None);
    }

    /// A `systemctl` that cannot be started after the reload changed the host fails the task as
    /// `run_command` does when `Popen` raises, instead of handing a half-done task back.
    ///
    /// What would make this red: the hand-back kept after an effect, which runs the reload
    /// again under Python; a wrong `rc`, `cmd` or missing `stdout`/`stderr`.
    #[test]
    fn a_systemctl_that_cannot_start_after_a_change_fails_like_the_module() {
        let fake = Fake::new("vanishes", r#"[ "$1" = daemon-reload ] && rm -f "$0""#);
        let task = json!({"name": "u", "state": "started", "daemon_reload": true});
        let answer = fake.answer(task.clone()).unwrap();
        let systemctl = format!("{}/bin/systemctl", fake.0);
        assert_eq!(
            Value::Object(answer),
            json!({
                "failed": true, "msg": "Error executing command.", "rc": 2,
                "cmd": format!("{systemctl} show u"), "stdout": "", "stderr": "",
                "invocation": invocation(SPEC, &args(task)),
            })
        );
        assert_eq!(fake.calls(), ["daemon-reload"]);
    }

    /// `_clean_args`: `shlex.quote` on each word, and the word after a `pass...` one masked.
    #[test]
    fn a_failed_command_is_named_like_the_module_names_it() {
        assert_eq!(
            clean_args(&["/usr/bin/systemctl", "list-unit-files", "u@*"]),
            "/usr/bin/systemctl list-unit-files 'u@*'"
        );
        assert_eq!(
            clean_args(&["systemctl", "is-enabled", "passwd.service", "-l"]),
            "systemctl is-enabled passwd.service '********'"
        );
        assert_eq!(
            clean_args(&["--password=x", "a b", "it's", ""]),
            "'--password=********' 'a b' 'it'\"'\"'s' ''"
        );
        assert_eq!(
            clean_args(&["/opt/\u{e9}/systemctl"]),
            "'/opt/\u{e9}/systemctl'"
        );
    }

    /// A `systemctl` that hangs (a D-Bus that does not answer) ends at the task's `timeout`, and
    /// at the controller's cancel, and is killed either way.
    #[test]
    fn a_hung_systemctl_ends_with_the_timeout_and_the_cancel() {
        let fake = Fake::new("hung", "exec sleep 600");
        let mut context = fake.context();
        context.timeout = Some(Duration::from_secs(1));
        let task = args(json!({"name": "u", "state": "started"}));
        let (task1, context1) = (task.clone(), context.clone());
        let run = within(30, move || (NATIVE.run)(&task1, &context1, &|| false));
        let NativeRun::Done(result) = run else {
            panic!("a timeout is an answer")
        };
        assert_eq!(result, TaskResult::timed_out(1));
        context.timeout = None;
        let run = within(30, move || (NATIVE.run)(&task, &context, &|| true));
        assert!(matches!(run, NativeRun::Cancelled));
    }
}
