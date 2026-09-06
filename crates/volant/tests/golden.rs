// SPDX-License-Identifier: GPL-3.0-or-later
//! Every case in tests/golden/cases.yml, compared with what the reference ansible-core did.

use serde_json::{Map, Value};
use volant::template::Templar;
use volant::yaml;

fn expected() -> Vec<Value> {
    serde_json::from_str(include_str!("golden/expected.json")).expect("expected.json parses")
}

/// Python's `2 == 2.0` is true; serde_json's is not. Integral floats compare equal to integers.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => (x - y).abs() < f64::EPSILON,
            _ => x == y,
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

#[test]
fn every_golden_case_matches_the_reference() {
    let base = std::env::temp_dir().join(format!("volant-golden-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("golden_lookup.txt"), "file contents").unwrap();
    // Safety: nextest runs each test in its own process; nothing else reads the environment here.
    unsafe { std::env::set_var("VOLANT_GOLDEN_ENV", "golden-env-value") };
    let templar = Templar::new(base.clone());
    let mut failures = Vec::new();
    for entry in expected() {
        let case = &entry["case"];
        let vars: Map<String, Value> = case
            .get("vars")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let ours: Result<Option<Value>, String> = match case.get("when").and_then(Value::as_str) {
            Some(when) => templar
                .condition(when, &vars)
                .map(|ok| ok.then(|| Value::String("ran".into())))
                .map_err(|e| e.to_string()),
            None => {
                let text = case["template"].as_str().expect("template is a string");
                templar
                    .render(text, &vars)
                    .map(Some)
                    .map_err(|e| e.to_string())
            }
        };
        let verdict = match (
            &ours,
            entry.get("error"),
            entry.get("skipped"),
            entry.get("result"),
        ) {
            (Err(_), Some(_), _, _) => Ok(()),
            (Ok(None), _, Some(_), _) => Ok(()),
            (Ok(Some(v)), None, None, Some(want)) if same(v, want) => Ok(()),
            _ => Err(format!(
                "{case}\n  reference: {}\n  ours: {ours:?}",
                serde_json::to_string(&entry).unwrap()
            )),
        };
        if let Err(text) = verdict {
            failures.push(text);
        }
    }
    std::fs::remove_dir_all(&base).ok();
    assert!(
        failures.is_empty(),
        "{} case(s) differ from ansible-core:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// `every_golden_case_matches_the_reference` above takes each case's `vars` straight from
/// `expected.json`, i.e. already typed by ansible-core's own YAML reading of `cases.yml` — it
/// never runs `vars` through our own `yaml::to_json`. This test does: it re-parses
/// `cases.yml`'s literal source, converts each case's `vars` mapping through the same
/// `yaml::load`/`yaml::to_json` path a real playbook's `vars_files` or inline `vars` uses, and
/// compares the result to the same reference recording, so a YAML 1.1/1.2 scalar-resolution
/// gap between saphyr and PyYAML shows up here even though the test above can't see it.
#[test]
fn our_yaml_loading_of_vars_matches_the_reference() {
    let cases_text = include_str!("golden/cases.yml");
    let docs = yaml::load(cases_text, "cases.yml").expect("cases.yml parses");
    let cases = docs[0].as_vec().expect("cases.yml is a list of cases");
    let mut failures = Vec::new();
    for (case, entry) in cases.iter().zip(expected()) {
        let Some(vars) = yaml::field(case, "vars") else {
            continue;
        };
        let ours = yaml::to_json(vars).expect("vars convert to JSON");
        let want = entry["case"]["vars"].clone();
        if !same(&ours, &want) {
            failures.push(format!("{case:?}\n  reference: {want}\n  ours: {ours}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} case(s)' vars differ from ansible-core's own YAML reading:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
