// SPDX-License-Identifier: GPL-3.0-or-later
//! The decision: what the task asks, against what the host already has.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use super::super::runtime::Root;
use super::request::{State, request};
use super::status::{
    bin_path, cache_mtime, from_timestamp, is_installed, read, stanzas, system_python_has_apt,
    utc_offset,
};
use super::{EXTENDED_STATES, STATUS, UPDATES};

/// Decides, reading only. `Err` is the reason the task goes to the Python module.
#[expect(clippy::too_many_lines)]
pub(super) fn plan(
    args: &Map<String, Value>,
    root: &Root,
    task_env: &BTreeMap<String, String>,
    now: SystemTime,
) -> Result<Map<String, Value>, String> {
    if task_env.contains_key("TZ") {
        return Err("the task sets TZ, which moves the module's local time".into());
    }
    let request = request(args)?;
    let status = read(root, STATUS)?.ok_or("the dpkg status file is missing")?;
    let status = stanzas(&status);
    if !is_installed(&status, "python3-apt", None) || !system_python_has_apt(root) {
        return Err(
            "python3-apt is not importable by the system python, and the module would install it"
                .into(),
        );
    }
    if std::fs::read_dir(root.path(UPDATES)).is_ok_and(|mut dir| dir.next().is_some()) {
        return Err("dpkg has pending updates in its journal".into());
    }

    let (seconds, micros) = match cache_mtime(root)? {
        Some((sec, nsec)) => from_timestamp(sec, nsec),
        None => (0, 0),
    };
    let now = now
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "the clock is before 1970")?;
    let now_seconds = i64::try_from(now.as_secs()).map_err(|_| "the clock is out of range")?;
    // The module makes `cache_update_time` with `mktime`, and compares the cache's age in naive
    // local times: each equals its UTC counterpart only while the zone keeps one offset around
    // the stamp, and, for the age, from it to now.
    let check = request.update_cache || request.cache_valid_time != 0;
    let mut instants = vec![seconds - 3600, seconds, seconds + 3600];
    if check {
        instants.push(now_seconds);
    }
    let offsets: Option<Vec<libc::c_long>> = instants.into_iter().map(utc_offset).collect();
    if !offsets.is_some_and(|o| o.windows(2).all(|w| w[0] == w[1])) {
        return Err("the local time changes offset around the cache stamp".into());
    }
    if check {
        let stamp = i128::from(seconds) * 1_000_000 + i128::from(micros);
        let valid = i128::from(request.cache_valid_time) * 1_000_000;
        // `datetime.now()` floors to the microsecond.
        let now_micros = i128::from(now_seconds) * 1_000_000 + i128::from(now.subsec_micros());
        if stamp + valid < now_micros {
            return Err(if request.cache_valid_time == 0 {
                "update_cache without cache_valid_time".into()
            } else {
                "the package cache is older than cache_valid_time".into()
            });
        }
    }

    let mut result = Map::new();
    result.insert(
        "invocation".into(),
        Value::Object(Map::from_iter([(
            "module_args".to_string(),
            Value::Object(request.module_args),
        )])),
    );
    let cache = |result: &mut Map<String, Value>| {
        result.insert("changed".into(), false.into());
        result.insert("cache_updated".into(), false.into());
        result.insert("cache_update_time".into(), seconds.into());
    };
    // No package: the module exits after its cache check, or, without one, installs nothing
    // (the cache keys) or removes nothing (`changed` alone).
    if request.packages.is_empty() {
        if check || request.state == State::Present {
            cache(&mut result);
        } else {
            result.insert("changed".into(), false.into());
        }
        return Ok(result);
    }
    match request.state {
        State::Absent => {
            if request.packages.iter().any(|name| {
                status.get(name.as_str()).is_some_and(|entries| {
                    entries
                        .iter()
                        .any(|e| !matches!(e.current(), "not-installed" | "config-files"))
                })
            }) {
                return Err("a package is installed, or partly".into());
            }
            result.insert("changed".into(), false.into());
            Ok(result)
        }
        State::Present => {
            // `dpkg` is always installed for the native architecture.
            let native_arch = match status.get("dpkg").map(Vec::as_slice) {
                Some([dpkg]) => dpkg.architecture,
                _ => return Err("the native architecture is not dpkg's alone".into()),
            };
            if request
                .packages
                .iter()
                .any(|name| !is_installed(&status, name, Some(native_arch)))
            {
                return Err("a package is not installed".into());
            }
            // The module's `apt-mark manual` would change what apt believes: that task is its own.
            let extended = read(root, EXTENDED_STATES)?.unwrap_or_default();
            let extended = stanzas(&extended);
            if request.packages.iter().any(|name| {
                extended
                    .get(name.as_str())
                    .is_some_and(|entries| entries.iter().any(|e| e.auto_installed))
            }) {
                return Err(
                    "a package is marked automatically installed, and apt-mark would change that"
                        .into(),
                );
            }
            // Without `apt-mark` the module warns; with it, every name manual, the call it makes
            // is a no-op the native skips.
            if bin_path(task_env, "apt-mark").is_none() {
                return Err("apt-mark is not on PATH, and the module would warn".into());
            }
            cache(&mut result);
            Ok(result)
        }
    }
}
