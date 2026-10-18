// SPDX-License-Identifier: GPL-3.0-or-later
//! The `date_time` collector: the probe's, which the module's interpreter computed with the
//! reference's own code, after setting the locale the way the module sets it, under the task's
//! environment. The agent's C library is not asked: under musl it leaves `tzname[1]` empty for a
//! zone without daylight time, and never reloads `/etc/localtime` once it has read it.

use serde_json::{Map, Value};

use super::Host;

pub fn collect(host: &Host) -> Result<Map<String, Value>, String> {
    if host.probe.lc_time.is_none() {
        // The module would then replace `LANG`, `LC_ALL` and `LC_MESSAGES` before collecting.
        return Err("the module's locale cannot be set, and it would replace it".into());
    }
    let mut facts = Map::new();
    facts.insert("date_time".into(), host.probe.date_time.clone());
    Ok(facts)
}

#[cfg(test)]
mod tests;
