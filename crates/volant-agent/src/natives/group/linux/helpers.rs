// SPDX-License-Identifier: GPL-3.0-or-later

//! What `group` and `user` share: the host gate, the commands, the account files and the result
//! keys.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::modules::Context;
use crate::natives::common::{Account, Group, files_first, group_account, lookup_group};
use crate::natives::runtime::{self, Clock, Root, Stop};

/// Hands back on the first argument outside `subset`, before anything is read or run.
pub fn outside_subset(args: &Map<String, Value>, subset: &[&str]) -> Result<(), String> {
    match args.keys().find(|key| !subset.contains(&key.as_str())) {
        Some(key) => Err(format!("{key} is left to the module")),
        None => Ok(()),
    }
}

/// `state`, one of the module's two choices.
pub fn present(params: &Map<String, Value>) -> Result<bool, String> {
    match params["state"].as_str() {
        Some("present") => Ok(true),
        Some("absent") => Ok(false),
        _ => Err("state is not one of the module's choices".into()),
    }
}

/// A `type='int'` id taken as a JSON integer in range; the module converts strings and
/// floats, and passes a negative or huge one on to a command that refuses it.
pub fn id_param(params: &Map<String, Value>, name: &str) -> Result<Option<u32>, String> {
    match &params[name] {
        Value::Null => Ok(None),
        value => value
            .as_u64()
            .and_then(|id| u32::try_from(id).ok())
            .map(Some)
            .ok_or_else(|| format!("{name} is not an id the native takes")),
    }
}

/// A name the account files and the commands read as itself. All digits would be looked up
/// as an id by the shared lookups.
pub fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains([':', ',', '\n', '\0'])
        && !name.bytes().all(|b| b.is_ascii_digit())
}

/// The host checks made before anything else: a Debian or Ubuntu host, where the module
/// takes its generic `User` and its `Linux` group class, with SELinux off, whose `nsswitch`
/// (the file `nsswitch`) asks `files` first for accounts and groups: another source first
/// answers `pwd` and `grp` with entries the commands, which edit the files, never see.
pub fn host_gate(nsswitch: &str) -> Result<(), String> {
    let os_release = std::fs::read_to_string("/etc/os-release")
        .map_err(|err| format!("reading /etc/os-release: {err}"))?;
    let id = os_release
        .lines()
        .filter_map(|line| line.strip_prefix("ID="))
        .next_back()
        .map(|id| id.trim_matches(['"', '\'']).to_lowercase());
    if !matches!(id.as_deref(), Some("debian" | "ubuntu")) {
        return Err("the distribution is outside Debian and Ubuntu".into());
    }
    if crate::natives::common::selinux_enabled() {
        return Err("SELinux is on".into());
    }
    for db in ["passwd", "group"] {
        if !files_first(nsswitch, db) {
            return Err(format!(
                "nsswitch.conf asks another source before files for {db}"
            ));
        }
    }
    Ok(())
}

/// `module.get_bin_path(name, required=True)` along the module's `PATH`. A missing command
/// fails the module; the native hands back instead.
pub fn bin(context: &Context, name: &str) -> Result<PathBuf, String> {
    let path = context
        .environment
        .get("PATH")
        .cloned()
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default();
    let path = BTreeMap::from([("PATH".to_string(), path)]);
    runtime::bin_path(&Root::real(), &path, name)
        .ok_or_else(|| format!("the module finds no {name}"))
}

/// `module.run_command(argv)` under the task's clock: exit code, output, error output. A
/// command that cannot be started changed nothing, so the task is handed back.
pub fn run_command(
    context: &Context,
    clock: Clock,
    program: &Path,
    args: &[&str],
) -> Result<(i32, String, String), Stop> {
    runtime::run_output(&context.environment, clock, program, args)?
        .map_err(|_| Stop::HandBack(format!("{} could not be started", program.display())))
}

/// The first entry of the colon-separated file `file` named `name`, as the C library's
/// `files` source finds it: a line with fewer than `fields` fields or a non-numeric id is
/// skipped. `None` when no entry has that name.
pub fn entry(file: &str, fields: usize, name: &str) -> Result<Option<Vec<String>>, String> {
    let text = std::fs::read_to_string(file).map_err(|err| format!("reading {file}: {err}"))?;
    for line in text.lines() {
        if line.starts_with(['+', '-']) {
            return Err(format!("{file} has NIS compat entries"));
        }
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= fields && parts[0] == name && parts[2].parse::<u32>().is_ok() {
            return Ok(Some(parts.into_iter().map(str::to_string).collect()));
        }
    }
    Ok(None)
}

/// `group_exists`/`group_info` of `user.py` for a `group` or `groups` argument: by gid when
/// Python's `int()` reads it, by name otherwise. `None` when there is no such group, which
/// the module fails on; anything Python would read differently hands back.
pub fn resolve_group(given: &str, clock: Clock) -> Result<Option<Group>, Stop> {
    match group_account(given, clock)? {
        Account::Unknown(_) => Ok(None),
        Account::Id(_) if given.bytes().all(|b| b.is_ascii_digit()) => {
            lookup_group(given, clock)?.map(Some).ok_or_else(|| {
                Stop::HandBack("no group has this id; the module then looks it up as a name".into())
            })
        }
        Account::Id(gid) => Ok(Some(Group {
            name: given.to_string(),
            gid,
        })),
    }
}

/// `rc`, `out` and `err` as the module's `main` ends with them: `changed`, and the outputs
/// only when not empty.
pub fn outcome(result: &mut Map<String, Value>, ran: Option<&(i32, String, String)>) {
    result.insert("changed".into(), Value::Bool(ran.is_some()));
    if let Some((_, out, err)) = ran {
        for (key, text) in [("stdout", out), ("stderr", err)] {
            if !text.is_empty() {
                result.insert(key.into(), Value::from(text.as_str()));
            }
        }
    }
}

/// `module.fail_json(**keys)`.
pub fn failure(invocation: Value, keys: &[(&str, Value)]) -> Map<String, Value> {
    let mut result: Map<String, Value> = keys
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect();
    result.insert("failed".into(), Value::Bool(true));
    result.insert("invocation".into(), invocation);
    result
}
