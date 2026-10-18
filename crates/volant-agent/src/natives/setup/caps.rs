// SPDX-License-Identifier: GPL-3.0-or-later
//! The `caps` collector: `capsh --print`, its `Current:` line read the way the reference reads it,
//! and `N/A` for both facts when `capsh` is missing or fails.

use serde_json::{Map, Value};

use super::Host;
use crate::natives::runtime::{Stop, splitlines};

pub fn collect(host: &Host) -> Result<Map<String, Value>, Stop> {
    let mut enforced = Value::from("N/A");
    let mut capabilities = Value::from("N/A");
    if let Some(capsh) = host.bin_path("capsh") {
        // A capsh that cannot be started is a warning in the module's result.
        let (code, out) = host
            .run(&capsh, &["--print"])?
            .ok_or("capsh was found and could not be started")?;
        if code == 0 {
            let (value, caps) = parse(&out);
            enforced = Value::from(value);
            capabilities = Value::from(caps);
        }
    }
    let mut facts = Map::new();
    facts.insert("system_capabilities_enforced".into(), enforced);
    facts.insert("system_capabilities".into(), capabilities);
    Ok(facts)
}

/// `line.split(':')[1].strip() == '=ep'`, else the names between the first and second `=`, split
/// on commas.
fn parse(out: &str) -> (&'static str, Vec<String>) {
    let mut enforced = "NA";
    let mut caps = Vec::new();
    for line in splitlines(out) {
        if !line.starts_with("Current:") {
            continue;
        }
        if crate::natives::runtime::py_strip(line.split(':').nth(1).unwrap_or_default()) == "=ep" {
            enforced = "False";
        } else {
            enforced = "True";
            caps = line
                .split('=')
                .nth(1)
                .unwrap_or_default()
                .split(',')
                .map(|cap| crate::natives::runtime::py_strip(cap).to_string())
                .collect();
        }
    }
    (enforced, caps)
}

#[cfg(test)]
mod tests;
