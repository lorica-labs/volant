// SPDX-License-Identifier: GPL-3.0-or-later

//! The options `systemd` reads, and the request read from them.

use serde_json::{Map, Value};

use crate::natives::common::{ArgSpec, bool_param, check_names, str_param};

/// `systemd_service`'s `argument_spec` in ansible-core 2.19.12.
pub(super) const SPEC: &[ArgSpec] = &[
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

pub(super) const STATES: &[&str] = &["reloaded", "restarted", "started", "stopped"];

/// The `is-enabled` answers the module takes for a unit systemd knows, from `systemctl(1)` of
/// systemd 244.
pub(super) const VALID_ENABLED_STATES: &[&str] = &[
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
pub(super) struct Request<'a> {
    pub(super) unit: Option<&'a str>,
    pub(super) state: Option<&'a str>,
    pub(super) enabled: Option<bool>,
    pub(super) daemon_reload: bool,
}

/// The task's arguments, validated as the module validates them, or the reason to hand back.
///
/// `params` are the arguments with aliases resolved and defaults filled in, as `invocation`
/// prints them.
pub(super) fn request<'a>(
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
