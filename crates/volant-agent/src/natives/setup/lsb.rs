// SPDX-License-Identifier: GPL-3.0-or-later
//! The `lsb` collector: `lsb_release -a`, or `/etc/lsb-release` when that gives nothing, each line
//! matched by substring in the reference's order.

use serde_json::{Map, Value};

use super::{Host, LsbRelease, py_strip, splitlines};

const STRIP_QUOTES: &[char] = &['\'', '"', '\\'];

pub fn collect(host: &Host, lsb_release: &LsbRelease) -> Result<Map<String, Value>, String> {
    let mut lsb: Vec<(&str, String)> = Vec::new();
    // A dictionary's assignment: a key keeps its place and takes the new value.
    fn set(lsb: &mut Vec<(&'static str, String)>, key: &'static str, value: &str) {
        match lsb.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => *v = value.to_string(),
            None => lsb.push((key, value.to_string())),
        }
    }
    if let Some((0, out)) = &lsb_release.collector {
        for line in splitlines(out) {
            let Some((_, value)) = line.split_once(':') else {
                continue;
            };
            let value = py_strip(value);
            if line.contains("LSB Version:") {
                set(&mut lsb, "release", value);
            } else if line.contains("Distributor ID:") {
                set(&mut lsb, "id", value);
            } else if line.contains("Description:") {
                set(&mut lsb, "description", value);
            } else if line.contains("Release:") {
                set(&mut lsb, "release", value);
            } else if line.contains("Codename:") {
                set(&mut lsb, "codename", value);
            }
        }
    }
    if lsb.is_empty() && host.root.exists("/etc/lsb-release") {
        for line in host.root.lines("/etc/lsb-release") {
            // The reference indexes the part after `=`, and a line without one fails the whole
            // collector.
            let (_, value) = line
                .split_once('=')
                .ok_or("/etc/lsb-release has a line without '='")?;
            let value = py_strip(value);
            if line.contains("DISTRIB_ID") {
                set(&mut lsb, "id", value);
            } else if line.contains("DISTRIB_RELEASE") {
                set(&mut lsb, "release", value);
            } else if line.contains("DISTRIB_DESCRIPTION") {
                set(&mut lsb, "description", value);
            } else if line.contains("DISTRIB_CODENAME") {
                set(&mut lsb, "codename", value);
            }
        }
    }
    if let Some(release) = lsb
        .iter()
        .find(|(key, _)| *key == "release")
        .map(|(_, v)| v.clone())
    {
        let major = release.split('.').next().unwrap_or_default().to_string();
        lsb.push(("major_release", major));
    }
    let lsb = lsb
        .into_iter()
        .map(|(key, value)| {
            (
                key.to_string(),
                Value::from(value.trim_matches(STRIP_QUOTES)),
            )
        })
        .collect();
    let mut facts = Map::new();
    facts.insert("lsb".into(), Value::Object(lsb));
    Ok(facts)
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
