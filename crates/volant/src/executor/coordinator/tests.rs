// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::watch;

use super::{Coordinator, Event, Progress};
use crate::compile::{Step, StepKind};
use crate::playbook::PlayTask;
use crate::render::{Dump, Renderer};
use crate::stats::{Outcome, Stats, exit_code};
use volant_protocol::TaskResult;

/// A coordinator over one list of hosts, which the batch and the play share.
///
/// `run_once` and `elected` start empty and then persist across every `publish` the test
/// makes, so a test over a real step list reads the elections its earlier calls decided.
fn coordinator<'a>(tx: &'a watch::Sender<Progress>, hosts: &'a [String]) -> Coordinator<'a> {
    Coordinator {
        tx,
        batch_hosts: hosts,
        all_hosts: hosts,
        frontier: HashMap::new(),
        spliced_through: None,
        run_once: BTreeMap::new(),
        elected: BTreeMap::new(),
        fatal: BTreeSet::new(),
        aborted: false,
        aborted_at: None,
        failed_here: HashSet::new(),
        last_failure: None,
        limit: None,
        batch_size: hosts.len(),
        removed: HashSet::new(),
        ending: BTreeSet::new(),
        dismissed: BTreeSet::new(),
        rendering: None,
    }
}

/// The barrier opens on the slowest live host, and a host that has left the play stops
/// holding it: this is what keeps a wait from outliving the host it waits for.
#[test]
fn progress_follows_the_slowest_live_host_and_forgets_the_others() {
    let hosts: Vec<String> = vec!["alpha".into(), "beta".into()];
    let (tx, rx) = watch::channel(Progress::default());
    let mut co = coordinator(&tx, &hosts);
    let mut lost = HashSet::new();

    co.publish(&lost, &[]);
    assert_eq!(rx.borrow().completed_through, None, "nobody has reported");
    assert_eq!(rx.borrow().live_hosts, hosts);

    co.frontier.insert("beta".to_string(), 3);
    co.publish(&lost, &[]);
    assert_eq!(
        rx.borrow().completed_through,
        None,
        "alpha has reported nothing, so the barrier stays shut"
    );

    co.frontier.insert("alpha".to_string(), 1);
    co.publish(&lost, &[]);
    assert_eq!(rx.borrow().completed_through, Some(1));

    lost.insert("alpha".to_string());
    co.publish(&lost, &[]);
    assert_eq!(
        rx.borrow().completed_through,
        Some(3),
        "a host out of the play no longer holds the barrier"
    );
    assert_eq!(rx.borrow().live_hosts, vec!["beta".to_string()]);

    lost.insert("beta".to_string());
    co.publish(&lost, &[]);
    assert!(rx.borrow().live_hosts.is_empty());
    assert_eq!(rx.borrow().completed_through, None);
}

/// Who runs a `run_once` step is decided once, and a host that leaves afterwards does not move
/// it. The mask is part of that decision, not a filter applied after it.
#[test]
fn run_once_is_decided_once_and_survives_the_runner_leaving() {
    let hosts: Vec<String> = vec!["h1".into(), "h2".into()];
    let step = |mask: Option<Vec<String>>| Step {
        kind: StepKind::Task,
        task: PlayTask {
            name: "once".into(),
            module: "command".into(),
            run_once: Some(true),
            ..PlayTask::empty()
        },
        block: None,
        section: crate::compile::Section::Body,
        role: None,
        origin: Arc::default(),
        include_params: None,
        hosts: mask.map(Arc::from),
    };
    let steps = vec![step(None), step(Some(vec!["h2".into()]))];

    let (tx, rx) = watch::channel(Progress::default());
    let mut co = coordinator(&tx, &hosts);
    let mut lost: HashSet<String> = HashSet::new();

    // The publish `run_batch` makes before it spawns a single driver. Index 0 has no barrier
    // in front of it, so this is the only moment its runner can be fixed.
    co.publish(&lost, &steps);
    assert_eq!(
        rx.borrow().elected.get(&0).map(String::as_str),
        Some("h1"),
        "the first live host of the batch"
    );

    // h1 dies on the way, which is the publish that wakes h2.
    lost.insert("h1".to_string());
    co.publish(&lost, &steps);
    let p = rx.borrow().clone();
    assert_eq!(
        p.live_hosts,
        vec!["h2".to_string()],
        "the list a driver electing for itself would have read"
    );
    assert_eq!(
        p.elected.get(&0).map(String::as_str),
        Some("h1"),
        "and the election did not move with it"
    );

    // The step behind it is masked to h2, and both hosts are live again for it.
    let (tx, rx) = watch::channel(Progress::default());
    let mut co = coordinator(&tx, &hosts);
    for host in &hosts {
        co.frontier.insert(host.clone(), 0);
    }
    co.publish(&HashSet::new(), &steps);
    assert_eq!(
        rx.borrow().elected.get(&1).map(String::as_str),
        Some("h2"),
        "elected inside the step's mask, not filtered after it"
    );
}

/// A host is never counted past what it has finished, whatever order its reports arrive in.
#[test]
fn a_gap_in_a_hosts_reports_holds_the_barrier_where_it_is() {
    let hosts: Vec<String> = vec!["alpha".into()];
    let (tx, rx) = watch::channel(Progress::default());
    let mut co = coordinator(&tx, &hosts);
    let mut done = HashSet::new();
    let lost = HashSet::new();

    for index in [0, 1] {
        co.finished(&mut done, "alpha", index);
    }
    // Steps 4 and 5 are stepped over: the driver says so while the batch holding 2 and 3 is
    // still running.
    for index in [4, 5] {
        co.finished(&mut done, "alpha", index);
    }
    co.publish(&lost, &[]);
    assert_eq!(
        rx.borrow().completed_through,
        Some(1),
        "the steps behind the gap are not finished yet"
    );

    co.finished(&mut done, "alpha", 2);
    co.publish(&lost, &[]);
    assert_eq!(rx.borrow().completed_through, Some(2), "the gap is smaller");

    co.finished(&mut done, "alpha", 3);
    co.publish(&lost, &[]);
    assert_eq!(
        rx.borrow().completed_through,
        Some(5),
        "the batch reported, so everything behind it counts"
    );
}

/// A failed host forgiven by `clear_host_errors` before its `Finished` stays forgiven; a
/// failure no result announced is still filed by `Finished`.
#[test]
fn a_finished_host_is_not_filed_again_after_its_failure_was() {
    let mut round = super::types::Round::default();
    let mut failed = HashSet::new();
    round.failed(&mut failed, "h2");
    assert!(failed.contains("h2"));
    // `clear_host_errors` forgives it before its `Finished` is read.
    failed.remove("h2");
    assert!(!round.files_failure("h2", true));
    assert!(round.files_failure("h2", true));
    assert!(round.files_failure("h3", true));
    assert!(!round.files_failure("h4", false));
}

/// The line of a failed result is shown when its host's turn comes, which can be after a
/// `clear_host_errors` that the coordinator read first. Showing it adds to the recap's `failed`
/// column and must not put the forgiven host back among the failing ones.
#[test]
fn showing_a_failed_line_after_a_clear_does_not_fail_the_host_again() {
    let mut stats = Stats::default();
    let mut out = Renderer::with_writer(Box::new(std::io::sink()), false, 79, 0);
    stats.clear_errors("h2", false);
    super::display::report_result(failed("h2"), &mut stats, &mut out);
    assert_eq!(stats.host("h2").failed, 1, "the recap still counts it");
    assert_eq!(exit_code(&stats), 0, "the clear forgave it");
}

fn failed(host: &str) -> Event {
    Event::Result {
        host: host.to_string(),
        index: 0,
        label: None,
        outcome: Outcome::Failed,
        result: TaskResult::default(),
        dump: Dump::No,
        show: true,
        counts: true,
        censored: false,
        delegate: None,
        error: None,
        task_origin: None,
    }
}

/// A failure forgiven by a clear that was read after it stays forgiven, and one read after the
/// clear counts again: the exit code follows the order the events arrive in.
#[test]
fn the_exit_code_follows_the_order_the_failures_arrive_in() {
    let mut stats = Stats::default();
    let mut out = Renderer::with_writer(Box::new(std::io::sink()), false, 79, 0);
    let early = super::display::arrived(failed("h2"), &mut stats);
    stats.clear_errors("h2", false);
    let late = super::display::arrived(failed("h3"), &mut stats);
    for event in [early, late] {
        super::display::report_result(event, &mut stats, &mut out);
    }
    assert_eq!(stats.host("h2").failed + stats.host("h3").failed, 2);
    assert_eq!(exit_code(&stats), 2, "h3 failed after the clear");
    stats.clear_errors("h3", false);
    assert_eq!(exit_code(&stats), 0, "and h2 stayed forgiven");
}
