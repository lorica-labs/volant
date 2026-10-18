// SPDX-License-Identifier: GPL-3.0-or-later
use std::path::PathBuf;

use rstest::rstest;
use serde_json::{Map, Value};
use tokio::sync::watch;

use super::Abort;
use super::play_checks::max_fail_percentage;

use crate::template::Templar;

/// A number, a number written as text and a template that renders to one all read; anything
/// else is refused by the keyword's name.
#[rstest]
#[case::integer("25", Ok(Some(25.0)))]
#[case::float("12.5", Ok(Some(12.5)))]
#[case::text("\"50\"", Ok(Some(50.0)))]
#[case::template("\"{{ pct }}\"", Ok(Some(30.0)))]
#[case::none("null", Ok(None))]
#[case::words("most", Err("is not a number"))]
#[case::list("[1]", Err("is not a number"))]
#[case::undefined("\"{{ nope }}\"", Err("max_fail_percentage"))]
fn max_fail_percentage_reads_a_number(
    #[case] written: &str,
    #[case] want: Result<Option<f64>, &'static str>,
) {
    let play = crate::playbook::parse(
        &format!(
            "- hosts: all
  max_fail_percentage: {written}
  tasks: []
"
        ),
        "x.yml",
    )
    .expect("the play loads")
    .plays
    .remove(0);
    let templar = Templar::new(PathBuf::from("."));
    let vars = Map::from_iter([("pct".to_string(), Value::from(30))]);
    let got = max_fail_percentage(&play, &templar, &vars);
    match want {
        Ok(limit) => assert_eq!(got.expect("the keyword reads"), limit),
        Err(needle) => {
            let err = got.expect_err("the keyword is refused");
            assert!(format!("{err:#}").contains(needle), "{err:#}");
        }
    }
}

/// An interruption that arrives first keeps the run an interrupted one: a reason raised after
/// it is dropped, so the run exits 99 and not 1.
#[test]
fn an_interruption_that_came_first_is_not_turned_into_a_reason() {
    let (tx, rx) = watch::channel(false);
    let abort = Abort::new(tx);
    abort.interrupt();
    abort.raise("late".into());
    assert_eq!(abort.reason(), None);
    assert!(*rx.borrow());

    let (tx, rx) = watch::channel(false);
    let abort = Abort::new(tx);
    abort.raise("first".into());
    abort.raise("second".into());
    assert_eq!(abort.reason().as_deref(), Some("first"));
    assert!(*rx.borrow());
}
