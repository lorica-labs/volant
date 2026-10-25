// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use crate::shadow::ledger::{self, Line};

/// A replay of `site.yml` by `volant`, `verdict`, that compared `tasks` of the three it knows.
fn line(verdict: &str, volant: &str, tasks: &[&str]) -> Line {
    Line {
        date: String::new(),
        time: 1000,
        playbook: "site.yml".into(),
        playbook_sha256: "aa".into(),
        volant: volant.into(),
        ansible_core: "2.19.12".into(),
        recording_sha256: "bb".into(),
        lock_sha256: None,
        limit: None,
        tags: Vec::new(),
        hosts: 1,
        compared: tasks.len(),
        not_compared: 0,
        tasks: tasks.iter().map(|t| (*t).to_owned()).collect(),
        known: [
            ("site.yml:3", "a"),
            ("site.yml:6", "b"),
            ("site.yml:9", "c"),
        ]
        .into_iter()
        .map(|(p, n)| (p.to_owned(), n.to_owned()))
        .collect(),
        divergences: if verdict == "dirty" {
            [("args".to_owned(), 1)].into_iter().collect()
        } else {
            BTreeMap::new()
        },
        verdict: verdict.into(),
    }
}

const ALL: &[&str] = &["site.yml:3", "site.yml:6", "site.yml:9"];

fn clean(n: usize) -> Vec<Line> {
    (0..n).map(|_| line("clean", "0.1.0", ALL)).collect()
}

/// The streak and the verdict a ledger gives, against 5 replays and 90% of the tasks.
#[rstest::rstest]
#[case::five_clean(clean(5), 5, "ready")]
#[case::too_few(clean(3), 3, "not-ready")]
#[case::reset_by_a_dirty_line(
    [clean(3), vec![line("dirty", "0.1.0", ALL)], clean(5)].concat(), 5, "ready"
)]
#[case::reset_by_a_dirty_line_short(
    [clean(4), vec![line("dirty", "0.1.0", ALL)], clean(2)].concat(), 2, "not-ready"
)]
#[case::diverged([clean(6), vec![line("dirty", "0.1.0", ALL)]].concat(), 0, "diverged")]
#[case::reset_by_another_volant(
    [clean(4), (0..2).map(|_| line("clean", "0.2.0", ALL)).collect()].concat(), 2, "not-ready"
)]
#[case::out_of_profile_ignored(
    [clean(3), vec![line("out-of-profile", "0.1.0", &[])], clean(2)].concat(), 5, "ready"
)]
fn the_streak_follows_the_rules(
    #[case] lines: Vec<Line>,
    #[case] streak: usize,
    #[case] verdict: &str,
) {
    let rows = rows(&lines, 5, 900);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].streak, rows[0].verdict),
        (streak, verdict),
        "{:?}",
        rows[0].reasons
    );
}

/// A playbook edited between replays keeps its streak: both engines read the same new file.
#[test]
fn a_changed_playbook_keeps_the_streak() {
    let mut lines = clean(5);
    lines[2].playbook_sha256 = "cc".into();
    assert_eq!(rows(&lines, 5, 900)[0].verdict, "ready");
}

/// Too few tasks compared during the streak: not ready, naming the ones never compared.
#[test]
fn low_coverage_names_what_was_never_compared() {
    let lines: Vec<Line> = (0..5)
        .map(|_| line("clean", "0.1.0", &["site.yml:3", "site.yml:6"]))
        .collect();
    let row = &rows(&lines, 5, 900)[0];
    assert_eq!(row.verdict, "not-ready");
    assert_eq!(row.coverage, 666);
    assert_eq!(
        row.reasons,
        ["66.6% of the tasks compared, 90% wanted; never compared: c (site.yml:9)"]
    );
    let shown = human(&rows(&lines, 5, 900), 5, 1000);
    assert!(
        shown.starts_with(
            "site.yml  not-ready  streak 5/5, coverage 66.6% of 3 tasks (1 never compared)"
        ),
        "{shown}"
    );
}

/// The tasks compared over the streak add up: two replays that compared different tasks cover
/// them all.
#[test]
fn coverage_adds_up_over_the_streak() {
    let lines = vec![
        line("clean", "0.1.0", &["site.yml:3"]),
        line("clean", "0.1.0", &["site.yml:6", "site.yml:9"]),
    ];
    assert_eq!(rows(&lines, 2, 900)[0].verdict, "ready");
}

/// Both formats carry the verdict, the reasons and the last divergence.
#[test]
fn both_formats_say_the_same() {
    let lines = [clean(1), vec![line("dirty", "0.1.0", ALL)]].concat();
    let rows = rows(&lines, 5, 900);
    let human = human(&rows, 5, 1000 + 7200);
    assert!(human.contains("site.yml  diverged"), "{human}");
    assert!(
        human.contains("last divergence 2 hours ago (args)"),
        "{human}"
    );
    let json: Value = serde_json::from_str(json(&rows, 1000 + 7200).trim()).unwrap();
    assert_eq!(json["verdict"], "diverged");
    assert_eq!(
        json["last_divergence"]["kinds"],
        serde_json::json!(["args"])
    );
    assert_eq!(
        json["reasons"],
        serde_json::json!(["the last replay diverged"])
    );
}

#[test]
fn utc_text_is_the_calendar_date() {
    assert_eq!(ledger::utc(0), "1970-01-01T00:00:00Z");
    assert_eq!(ledger::utc(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(ledger::utc(1_761_388_582), "2025-10-25T10:36:22Z");
}

/// A line that cannot be read is left out, and the others are read.
#[test]
fn an_unreadable_line_is_left_out() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("deep/shadow.jsonl");
    ledger::append(&path, &line("clean", "0.1.0", ALL)).unwrap();
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("{not json\n");
    std::fs::write(&path, text).unwrap();
    ledger::append(&path, &line("dirty", "0.1.0", ALL)).unwrap();
    let lines = ledger::read(&path).unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1].verdict, "dirty");
    assert!(
        ledger::read(&dir.path().join("none.jsonl"))
            .unwrap()
            .is_empty()
    );
}
