// SPDX-License-Identifier: GPL-3.0-or-later

//! Reading what `systemctl` and the init scripts say.

use std::path::Path;

use serde_json::{Map, Value};

use crate::natives::runtime::{py_space, py_strip};

/// The `XDG_RUNTIME_DIR` the module sets when `os.getenv` finds none, even an empty one.
pub(super) fn xdg_default(var: impl Fn(&str) -> Option<String>, euid: u32) -> Option<String> {
    var("XDG_RUNTIME_DIR")
        .is_none()
        .then(|| format!("/run/user/{euid}"))
}

/// `fail_if_missing(module, found, unit, msg='host')`'s message.
pub(super) fn reply_missing(unit: &str) -> String {
    format!("Could not find the requested service {unit}: host")
}

/// `request_was_ignored(out)`.
pub(super) fn request_was_ignored(out: &str) -> bool {
    !out.contains('=') && (out.contains("ignoring request") || out.contains("ignoring command"))
}

/// `parse_systemctl_show(out.split('\n'))`: one value per `Key=value` line, stripped; a value
/// spans lines only for an `Exec*` key whose value opens a `{` it does not close on that line,
/// up to the line that ends with `}`. A value left open at the end of the output is dropped.
pub(super) fn parse_systemctl_show(out: &str) -> Map<String, Value> {
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
pub(super) fn sysv_is_enabled(etc: &str, name: &str) -> bool {
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
