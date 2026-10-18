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
    ArgSpec, bool_param, check_names, clean_args, clock, invocation, native_run, popen_error,
    shlex_quote, str_param,
};
use crate::natives::runtime::{self, Clock, Root, Stop, py_space, py_strip};

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
        match runtime::run_output(&self.env, self.clock, program, args)? {
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
                failure.insert("exception".into(), popen_error(errno, &program));
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
        if let Some(stat) = runtime::bin_path(&Root::real(), &self.path, "stat") {
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
#[expect(clippy::too_many_lines)]
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
    let systemctl = runtime::bin_path(&Root::real(), &path, "systemctl")
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
    if let Some(dir) = xdg_default(var, unsafe { libc::geteuid() }) {
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
mod tests;
