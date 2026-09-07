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
