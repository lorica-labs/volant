// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-host counters for the play recap and the process exit code, as ansible-playbook counts them.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// How one task went on one host, from the renderer's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Changed,
    Skipped,
    Failed,
    /// Failed, and a `rescue` around it takes the failure. It shows exactly like `Failed` -
    /// measured on ansible-core 2.19.12, the rescued task still prints `fatal: [h]: FAILED!` -
    /// and counts in the recap's own `rescued` column instead of `failed`. It is not the host
    /// leaving the play: the rescue runs, and the play goes on for it.
    Rescued,
    /// Failed, but the task had `ignore_errors`.
    Ignored,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HostStats {
    pub ok: u32,
    pub changed: u32,
    pub unreachable: u32,
    pub failed: u32,
    pub skipped: u32,
    pub rescued: u32,
    pub ignored: u32,
}

#[derive(Debug, Default)]
pub struct Stats {
    hosts: BTreeMap<String, HostStats>,
    /// Hosts the run failed without their ever reporting a task. Measured on ansible-core
    /// 2.19.12: a `run_once` task that fails takes every other host of the play out of it, and
    /// those hosts have **no recap line at all** - they ran nothing, so there is nothing to
    /// count - while the run still exits 2. A counter would have printed a line the reference
    /// does not, so the exit code reads this set and the recap does not.
    unreported: std::collections::BTreeSet<String>,
    /// The hosts whose failures and unreachability still count for the exit code. The recap keeps
    /// every count; `meta: clear_host_errors` takes a host out of these two sets and leaves its
    /// recap line as it was. Measured on ansible-core 2.19.12: `failed=1` in the recap and exit 0.
    failing: std::collections::BTreeSet<String>,
    dark: std::collections::BTreeSet<String>,
}

impl Stats {
    /// `changed` is the result's own flag: an ignored failure still counts as changed when the
    /// module reported one, on top of counting as ignored. A hard failure never counts as
    /// changed, whatever the result says: the host stopped there.
    pub fn record(&mut self, host: &str, outcome: Outcome, changed: bool) {
        let h = self.hosts.entry(host.to_string()).or_default();
        match outcome {
            Outcome::Skipped => h.skipped += 1,
            Outcome::Failed => {
                h.failed += 1;
                self.failing.insert(host.to_string());
            }
            // Counted in its own column and nowhere else. Measured on ansible-core 2.19.12: a
            // failure a rescue takes reads `failed=0 rescued=1`, and it does not count as `ok`
            // either, however the play ends for that host.
            Outcome::Rescued => h.rescued += 1,
            Outcome::Ok | Outcome::Changed | Outcome::Ignored => {
                h.ok += 1;
                if changed {
                    h.changed += 1;
                }
                if outcome == Outcome::Ignored {
                    h.ignored += 1;
                }
            }
        }
    }

    /// A host that failed with nothing to show for it. See [`Stats::unreported`].
    pub fn failed_unreported(&mut self, host: &str) {
        self.unreported.insert(host.to_string());
    }

    pub fn unreachable(&mut self, host: &str) {
        self.hosts.entry(host.to_string()).or_default().unreachable += 1;
        self.dark.insert(host.to_string());
    }

    /// `meta: clear_host_errors` for `host`: its failures stop counting for the exit code, and so
    /// does its unreachability unless `keep_dark` says the reference still holds the host
    /// unreachable - one that went unreachable in an earlier play stays so for every play after
    /// it, measured, and the exit code says 4 whatever a later play cleared.
    pub fn clear_errors(&mut self, host: &str, keep_dark: bool) {
        self.failing.remove(host);
        self.unreported.remove(host);
        if !keep_dark {
            self.dark.remove(host);
        }
    }

    #[cfg(test)]
    pub fn host(&self, name: &str) -> HostStats {
        self.hosts.get(name).copied().unwrap_or_default()
    }

    pub fn hosts(&self) -> impl Iterator<Item = (&str, &HostStats)> {
        self.hosts.iter().map(|(n, s)| (n.as_str(), s))
    }
}

/// What `set_stats` collected: one map per host that asked for `per_host`, and one for the run.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CustomStats {
    hosts: BTreeMap<String, Map<String, Value>>,
    run: Map<String, Value>,
}

impl CustomStats {
    /// One `set_stats` result's `ansible_stats` for `host`: `data` under the host's own name when
    /// `per_host`, under the run's otherwise, summed with what is there when `aggregate` and
    /// replacing it when not. A malformed `ansible_stats` adds nothing.
    pub fn record(&mut self, host: &str, stats: &Value) {
        let Some(data) = stats.get("data").and_then(Value::as_object) else {
            return;
        };
        let flag = |name: &str| stats.get(name).and_then(Value::as_bool).unwrap_or(false);
        let (per_host, aggregate) = (flag("per_host"), flag("aggregate"));
        let into = if per_host {
            self.hosts.entry(host.to_string()).or_default()
        } else {
            &mut self.run
        };
        for (key, value) in data {
            match into.get_mut(key) {
                Some(held) if aggregate => {
                    if let Some(sum) = aggregated(held, value) {
                        *held = sum;
                    }
                }
                _ => {
                    into.insert(key.clone(), value.clone());
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty() && self.run.is_empty()
    }

    /// The hosts that asked for `per_host`, sorted.
    pub fn hosts(&self) -> &BTreeMap<String, Map<String, Value>> {
        &self.hosts
    }

    /// The data kept for the run as a whole.
    pub fn run(&self) -> &Map<String, Value> {
        &self.run
    }
}

/// `held + new` as `AggregateStats.update_custom_stats` adds them: a value of another type than
/// the one held is ignored (`None`), mappings merge deeply, lists and strings join, numbers add.
/// A boolean counts as the integer it is in Python, so `true` four times reads 4.
fn aggregated(held: &Value, new: &Value) -> Option<Value> {
    match (held, new) {
        (Value::Object(a), Value::Object(b)) => {
            let mut merged = a.clone();
            for (key, value) in b {
                let next = match merged.get(key) {
                    Some(Value::Object(_)) if value.is_object() => aggregated(&merged[key], value),
                    _ => Some(value.clone()),
                };
                merged.insert(key.clone(), next?);
            }
            Some(Value::Object(merged))
        }
        (Value::Array(a), Value::Array(b)) => Some(Value::Array([a.clone(), b.clone()].concat())),
        (Value::String(a), Value::String(b)) => Some(Value::String(format!("{a}{b}"))),
        (Value::Bool(a), Value::Bool(b)) => Some(Value::from(i64::from(*a) + i64::from(*b))),
        // `isinstance(what, type(held))`: an integer or a boolean may join an integer.
        (Value::Number(a), Value::Number(b)) if a.is_f64() && b.is_f64() => {
            Some(Value::from(a.as_f64()? + b.as_f64()?))
        }
        (Value::Number(a), Value::Number(b)) if !a.is_f64() && !b.is_f64() => {
            Some(Value::from(a.as_i64()?.checked_add(b.as_i64()?)?))
        }
        (Value::Number(a), Value::Bool(b)) if !a.is_f64() => {
            Some(Value::from(a.as_i64()?.checked_add(i64::from(*b))?))
        }
        _ => None,
    }
}

/// A refusal that stops a run before any recap, carrying the exit status the reference gives
/// it. Everything the reference refuses at that level does *not* exit 1: a playbook it cannot
/// read or make sense of exits 4, and a setting it will not accept exits 2. The code travels
/// with the error rather than being decided where it is printed, because the same `?` in the
/// run carries all three.
#[derive(Debug)]
pub struct Refusal {
    pub code: i32,
    pub message: String,
}

impl Refusal {
    /// Wraps an error so the run exits with `code` instead of 1. The message is flattened here
    /// because it is only ever printed: `{:#}` on an `anyhow::Error` keeps its whole context
    /// chain, which is the text the operator already reads today.
    pub fn at(code: i32, err: impl std::fmt::Display) -> anyhow::Error {
        anyhow::Error::new(Refusal {
            code,
            message: format!("{err}"),
        })
    }

    /// Gives `err` this code unless it already carries one of its own, so a blanket code for a
    /// whole family of refusals cannot bury the one a member of it measured for itself.
    pub fn or(code: i32, err: anyhow::Error) -> anyhow::Error {
        if err
            .chain()
            .any(<dyn std::error::Error + 'static>::is::<Refusal>)
        {
            err
        } else {
            Refusal::at(code, format!("{err:#}"))
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Refusal {}

/// The exit status an error out of a run gives the process: whatever refusal it carries, or 1.
pub fn error_code(err: &anyhow::Error) -> i32 {
    err.chain()
        .find_map(|e| e.downcast_ref::<Refusal>())
        .map_or(1, |refusal| refusal.code)
}

/// ansible-playbook's exit status: 2 for failed hosts, 4 for unreachable hosts, combined.
pub fn exit_code(stats: &Stats) -> i32 {
    let mut code = 0;
    if !stats.failing.is_empty() || !stats.unreported.is_empty() {
        code |= 2;
    }
    if !stats.dark.is_empty() {
        code |= 4;
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_follow_ansible_rules() {
        let mut s = Stats::default();
        s.record("h", Outcome::Ok, false);
        s.record("h", Outcome::Changed, true);
        s.record("h", Outcome::Ignored, false);
        s.record("h", Outcome::Skipped, false);
        s.record("h", Outcome::Failed, true);
        s.unreachable("h");
        let h = s.host("h");
        assert_eq!(
            (
                h.ok,
                h.changed,
                h.ignored,
                h.skipped,
                h.failed,
                h.unreachable
            ),
            (3, 1, 1, 1, 1, 1)
        );
    }

    /// A rescued failure counts once, in its own column, and moves nothing else.
    #[test]
    fn a_rescued_failure_counts_only_as_rescued() {
        let mut s = Stats::default();
        s.record("h", Outcome::Rescued, true);
        let h = s.host("h");
        assert_eq!(
            (h.rescued, h.failed, h.ok, h.changed, h.ignored),
            (1, 0, 0, 0, 0)
        );
        assert_eq!(exit_code(&s), 0, "a rescued host is not a failed host");
    }

    #[test]
    fn an_ignored_failure_still_counts_as_changed_when_the_result_says_so() {
        let mut s = Stats::default();
        s.record("h", Outcome::Ignored, true);
        let h = s.host("h");
        assert_eq!((h.ok, h.changed, h.ignored), (1, 1, 1));
    }

    #[test]
    fn a_hard_failure_never_counts_as_changed() {
        let mut s = Stats::default();
        s.record("h", Outcome::Failed, true);
        let h = s.host("h");
        assert_eq!((h.ok, h.changed, h.failed), (0, 0, 1));
    }

    #[test]
    fn exit_codes_are_a_bitmask() {
        let mut s = Stats::default();
        assert_eq!(exit_code(&s), 0);
        s.record("a", Outcome::Failed, false);
        assert_eq!(exit_code(&s), 2);
        s.unreachable("b");
        assert_eq!(exit_code(&s), 6);
    }

    /// A cleared host keeps its recap counts and stops counting for the exit code, but for the
    /// unreachability the reference keeps.
    #[test]
    fn cleared_errors_leave_the_recap_and_the_exit_code() {
        let mut s = Stats::default();
        s.record("a", Outcome::Failed, false);
        s.unreachable("b");
        s.unreachable("c");
        s.failed_unreported("d");
        assert_eq!(exit_code(&s), 6);
        for host in ["a", "b", "d"] {
            s.clear_errors(host, false);
        }
        s.clear_errors("c", true);
        assert_eq!(exit_code(&s), 4, "c is still unreachable for the reference");
        s.clear_errors("c", false);
        assert_eq!(exit_code(&s), 0);
        assert_eq!((s.host("a").failed, s.host("b").unreachable), (1, 1));
        s.unreachable("b");
        assert_eq!((exit_code(&s), s.host("b").unreachable), (4, 2));
    }

    #[test]
    fn hosts_are_listed_sorted() {
        let mut s = Stats::default();
        s.record("web", Outcome::Ok, false);
        s.record("db", Outcome::Ok, false);
        assert_eq!(s.hosts().map(|(n, _)| n).collect::<Vec<_>>(), ["db", "web"]);
    }
}
