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
use std::path::Path;

use serde_json::{Map, Value};

use crate::modules::Context;
use crate::natives::Native;
use crate::natives::common::{clean_args, clock, invocation, native_run, shlex_quote};
use crate::natives::runtime::{self, Clock, Root, Stop, py_space, py_strip};

mod host;
mod parse;
mod request;

use host::{Host, Reply};
use parse::{
    parse_systemctl_show, reply_missing, request_was_ignored, sysv_is_enabled, xdg_default,
};
use request::{SPEC, VALID_ENABLED_STATES, request};

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

#[cfg(all(test, target_os = "linux"))]
mod tests;
