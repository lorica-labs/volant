// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-host counters for the play recap and the process exit code, as ansible-playbook counts them.

use std::collections::BTreeMap;

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
}

impl Stats {
    /// `changed` is the result's own flag: an ignored failure still counts as changed when the
    /// module reported one, on top of counting as ignored. A hard failure never counts as
    /// changed, whatever the result says: the host stopped there.
    pub fn record(&mut self, host: &str, outcome: Outcome, changed: bool) {
        let h = self.hosts.entry(host.to_string()).or_default();
        match outcome {
            Outcome::Skipped => h.skipped += 1,
            Outcome::Failed => h.failed += 1,
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

    pub fn unreachable(&mut self, host: &str) {
        self.hosts.entry(host.to_string()).or_default().unreachable += 1;
    }

    #[cfg(test)]
    pub fn host(&self, name: &str) -> HostStats {
        self.hosts.get(name).copied().unwrap_or_default()
    }

    pub fn hosts(&self) -> impl Iterator<Item = (&str, &HostStats)> {
        self.hosts.iter().map(|(n, s)| (n.as_str(), s))
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
        if err.chain().any(|link| link.is::<Refusal>()) {
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
    if stats.hosts().any(|(_, h)| h.failed > 0) {
        code |= 2;
    }
    if stats.hosts().any(|(_, h)| h.unreachable > 0) {
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
    ///
    /// Measured on ansible-core 2.19.12: a task failing in a block with a rescue recaps
    /// `ok=1 changed=0 failed=0 rescued=1` - the `ok` being the rescue's own task - and the
    /// run exits 0. What would make this red: counting it as `failed`, which exits 2 on a
    /// playbook the reference exits 0 on; or as `ok`, which hides the failure entirely.
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

    #[test]
    fn hosts_are_listed_sorted() {
        let mut s = Stats::default();
        s.record("web", Outcome::Ok, false);
        s.record("db", Outcome::Ok, false);
        assert_eq!(s.hosts().map(|(n, _)| n).collect::<Vec<_>>(), ["db", "web"]);
    }
}
