// SPDX-License-Identifier: GPL-3.0-or-later
//! The parity matrix: every name ansible-core 2.19.12 and the collection corpus know, against
//! what this release does with it. `parity/reference.json` is the reference's side, written by
//! `just parity-reference`; everything else is asked of the engine.

use serde_json::Value;
use volant::keywords;
use volant_protocol::modules;

fn reference() -> Value {
    serde_json::from_str(include_str!("parity/reference.json")).expect("reference.json")
}

fn names(v: &Value, path: &[&str]) -> Vec<String> {
    let mut at = v;
    for p in path {
        at = &at[*p];
    }
    at.as_array()
        .unwrap_or_else(|| panic!("{path:?} is not a list in reference.json"))
        .iter()
        .map(|n| n.as_str().unwrap().to_string())
        .collect()
}

/// A place keywords are written, its table, and whether a name has a row there.
type Place = (&'static str, &'static [keywords::Keyword], fn(&str) -> bool);

/// Every keyword the reference knows has a row in `keywords.rs`, at the same place, and no row
/// there names a keyword the reference does not know.
///
/// What would make this red: a keyword the reference added (or one of ours misspelled), since the
/// row would otherwise be read as a module name, or refused as unknown, without anyone deciding.
#[test]
fn the_keyword_tables_are_the_references() {
    let r = reference();
    let places: [Place; 4] = [
        ("play", keywords::PLAY_KEYWORDS, |n| {
            keywords::play_keyword(n).is_some()
        }),
        ("block", keywords::BLOCK_KEYWORDS, |n| {
            keywords::block_keyword(n).is_some()
        }),
        ("task", keywords::TASK_KEYWORDS, |n| {
            keywords::task_keyword(n).is_some()
        }),
        ("loop_control", keywords::LOOP_CONTROL_KEYWORDS, |n| {
            keywords::loop_control_keyword(n).is_some()
        }),
    ];
    for (place, table, known) in places {
        let theirs = names(&r, &["keywords", place]);
        for n in &theirs {
            assert!(
                known(n),
                "{place} keyword '{n}' is missing from keywords.rs"
            );
        }
        for k in table {
            assert!(
                theirs.contains(&k.name.to_string()),
                "{place} keyword '{}' is not the reference's",
                k.name
            );
        }
    }
    for n in names(&r, &["keywords", "handler"]) {
        assert!(
            keywords::handler_keyword(&n).is_some() || keywords::task_keyword(&n).is_some(),
            "handler keyword '{n}' is missing from keywords.rs"
        );
    }
}

/// Every `meta` action the reference accepts is either run or refused by name.
///
/// What would make this red: an action the reference added (2.19 may have `end_batch` and
/// `end_role`) answered with "invalid meta action", the message for a typo.
#[test]
fn every_meta_action_is_known() {
    for action in names(&reference(), &["meta"]) {
        let text = format!("- hosts: all\n  gather_facts: false\n  tasks:\n    - meta: {action}\n");
        let pb = volant::playbook::parse(&text, "parity.yml").expect("parses");
        if let Err(e) = volant::preflight::check(&pb) {
            let msg = format!("{e:#}");
            assert!(
                msg.contains("is not supported yet"),
                "meta '{action}': {msg}"
            );
        }
    }
}

/// Every builtin module the reference ships is a builtin here, and has a route.
///
/// What would make this red: a module missing from `BUILTIN_MODULES`, which a playbook would
/// then be told is a typo.
#[test]
fn the_builtin_modules_are_the_references() {
    let r = reference();
    let theirs = names(&r, &["builtin", "module"]);
    for m in &theirs {
        assert!(
            modules::builtin_route(m).is_some(),
            "builtin module '{m}' has no route"
        );
    }
    for m in modules::BUILTIN_MODULES {
        assert!(
            theirs.contains(&m.to_string()),
            "'{m}' is not a builtin module of the reference"
        );
    }
    let actions = names(&r, &["builtin", "action"]);
    for m in modules::BUILTIN_ACTION_PLUGINS {
        assert!(
            actions.contains(&m.to_string()),
            "'{m}' has no action plugin in the reference"
        );
    }
}
