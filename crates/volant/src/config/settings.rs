// SPDX-License-Identifier: GPL-3.0-or-later
//! One setting at a time: integers, switches and the few values with a closed set of names.

/// An integer setting, or the reference's own refusal for one that is not an integer.
///
/// Measured against ansible-core 2.19.12 on the development machine: `forks = many` in
/// `ansible.cfg` prints `ERROR: Config 'DEFAULT_FORKS' from '<path>' has an invalid value:
/// Invalid value provided for 'integer': 'many'` and exits **5**, before any play header;
/// `timeout = abc` prints the same with `DEFAULT_TIMEOUT`, and `ANSIBLE_TIMEOUT=abc` prints it
/// with `env: ANSIBLE_TIMEOUT` as the origin. Silently keeping the default instead - which is
/// what `timeout` used to do from both its sources - runs the playbook with a value the
/// operator never wrote and the reference never accepted.
///
/// The sign is kept, because the two settings that read integers do different things with it;
/// see [`forks`] and [`timeout_secs`].
pub(super) fn integer(name: &str, origin: &str, value: &str) -> anyhow::Result<i64> {
    let value = value.trim();
    value.parse::<i64>().map_err(|_| {
        crate::stats::Refusal::at(
            5,
            format!(
                "Config '{name}' from '{origin}' has an invalid value: Invalid value provided for 'integer': '{value}'"
            ),
        )
    })
}

/// The fact cache plugin a setting names. Only the two the engine has are accepted: any other is
/// refused by name rather than run with no cache.
pub(super) fn cache_plugin(value: &str, origin: &str) -> anyhow::Result<String> {
    match value.trim() {
        "" => Ok("memory".to_string()),
        name @ ("memory" | "jsonfile" | "ansible.builtin.memory" | "ansible.builtin.jsonfile") => {
            Ok(name.trim_start_matches("ansible.builtin.").to_string())
        }
        other => Err(crate::stats::Refusal::at(
            4,
            format!(
                "fact cache plugin '{other}' (from '{origin}') is not supported yet; use 'memory' or 'jsonfile'"
            ),
        )),
    }
}

/// The `gathering` policy: one of the three words the reference knows, refused otherwise.
pub(super) fn gathering(value: &str, origin: &str) -> anyhow::Result<String> {
    match value.trim() {
        "" => Ok("implicit".to_string()),
        word @ ("implicit" | "explicit" | "smart") => Ok(word.to_string()),
        other => Err(crate::stats::Refusal::at(
            5,
            format!(
                "Config 'DEFAULT_GATHERING' from '{origin}' has an invalid value: Invalid value provided for 'gathering': '{other}'; expected one of implicit, explicit, smart"
            ),
        )),
    }
}

/// `[volant] runtime`: one of its three words, refused otherwise.
pub(super) fn runtime_mode(value: &str, origin: &str) -> anyhow::Result<crate::runtime::Mode> {
    crate::runtime::Mode::parse(value.trim()).ok_or_else(|| {
        crate::stats::Refusal::at(
            5,
            format!(
                "Config 'VOLANT_RUNTIME' from '{origin}' has an invalid value: '{}'; expected one of auto, managed, system",
                value.trim()
            ),
        )
    })
}

/// A negative `forks` is clamped to zero, where the single startup check refuses it with the
/// reference's own words and the same exit 2 a literal zero gets: measured, `forks = -1` exits
/// 2 with `The number of processes (--forks) must be >= 1`.
pub(super) fn forks(n: i64) -> usize {
    n.max(0) as usize
}

/// A switch as a configuration line or an environment variable spells one: every word PyYAML
/// reads as a boolean, and the two digits with them.
///
/// `bool_from_str` alone would drop `VOLANT_BATCHING=1`, since YAML 1.1 reads `1` as an integer
/// and not as a boolean. A configuration file is not YAML, and neither is an environment
/// variable, so the digit is read here.
pub(super) fn switch(value: &str) -> Option<bool> {
    match value.trim() {
        "1" => Some(true),
        "0" => Some(false),
        text => crate::yaml::bool_from_str(text),
    }
}

/// A switch that has to be one, or the refusal the integer settings get, worded for a boolean.
///
/// The reference reads every word it does not know as `false`; `native_modules = maybe` read that
/// way would turn natives off without a word, and a comparison run between the two paths would
/// compare nothing.
pub(super) fn boolean(name: &str, origin: &str, value: &str) -> anyhow::Result<bool> {
    switch(value).ok_or_else(|| {
        crate::stats::Refusal::at(
            5,
            format!(
                "Config '{name}' from '{origin}' has an invalid value: Invalid value provided for 'boolean': '{}'",
                value.trim()
            ),
        )
    })
}

/// A negative connection timeout is refused where it is read, rather than clamped.
///
/// Measured on ansible-core 2.19.12: `timeout = -1` is passed straight to `ssh`, which answers
/// `command-line line 0: invalid time value`, and every host of the run comes back UNREACHABLE
/// at exit 4. So the reference does not run with it either. Clamping it to zero here would
/// instead mean `ConnectTimeout=0`, which is no limit at all - a connection that hangs for as
/// long as the kernel allows, from a line the operator wrote to make it hang less. It is
/// refused before the first connection instead, with the exit 2 the other unusable startup
/// number gets.
pub(super) fn timeout_secs(n: i64, origin: &str) -> anyhow::Result<u64> {
    u64::try_from(n).map_err(|_| {
        crate::stats::Refusal::at(
            2,
            format!("The connection timeout from '{origin}' must be >= 0, got {n}"),
        )
    })
}
