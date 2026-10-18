// SPDX-License-Identifier: GPL-3.0-or-later
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
