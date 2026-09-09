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

/// `same` above is deliberately order-insensitive. This one is not: it walks two already-`same`
/// values and additionally requires every mapping to iterate its keys in the same order, so
/// `our_yaml_loading_of_vars_matches_the_reference` can actually gate key order through our own
/// `yaml::load`/`to_json` instead of only through `Map`'s order-blind `PartialEq`.
fn same_key_order(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same_key_order(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.keys().eq(y.keys())
                && x.values()
                    .zip(y.values())
                    .all(|(a, b)| same_key_order(a, b))
        }
        _ => true,
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
        } else if !same_key_order(&ours, &want) {
            failures.push(format!(
                "{case:?}\n  key order differs\n  reference: {want}\n  ours: {ours}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} case(s)' vars differ from ansible-core's own YAML reading:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn inventory_matches_the_reference() {
    let expected: Value =
        serde_json::from_str(include_str!("golden/expected_inventory.json")).unwrap();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/inventory.ini");
    let inv = volant::inventory::Inventory::load(&path).unwrap();
    let mut failures = Vec::new();

    for (host, want) in expected["hostvars"].as_object().unwrap() {
        let got = inv.host_with_vars(host).vars;
        for (k, v) in want.as_object().unwrap() {
            if k.starts_with("ansible_") && k != "ansible_host" && k != "ansible_connection" {
                continue;
            }
            match got.get(k) {
                Some(ours) if same(ours, v) => {}
                other => failures.push(format!("{host}.{k}: reference {v}, ours {other:?}")),
            }
        }
    }
    for (group, want) in expected["groups"].as_object().unwrap() {
        let mut got = inv.groups().get(group).cloned().unwrap_or_default();
        got.sort();
        if serde_json::to_value(&got).unwrap() != *want {
            failures.push(format!("group {group}: reference {want}, ours {got:?}"));
        }
    }
    for (pattern, want) in expected["patterns"].as_object().unwrap() {
        let res = inv.resolve(pattern);
        let got: Vec<&str> = res.hosts.iter().map(|h| h.name.as_str()).collect();
        if serde_json::to_value(&got).unwrap() != want["hosts"] {
            failures.push(format!(
                "pattern {pattern}: reference {}, ours {got:?}",
                want["hosts"]
            ));
        }
        // inventory.ini deliberately carries no host/group homonym (see homonym_inventory.ini
        // below), so every one of these patterns must come back warning-free; checking both
        // directions means a spurious warning fails this just as loudly as a missing one, unlike
        // the one-directional check this replaced, which passed regardless of whether `resolve`
        // warned correctly because this fixture used to warn on every single pattern.
        match (want["warning"].as_bool(), res.warnings.is_empty()) {
            (Some(true), true) => {
                failures.push(format!("pattern {pattern}: the reference warns, we do not"))
            }
            (Some(false), false) => failures.push(format!(
                "pattern {pattern}: we warn, the reference does not: {:?}",
                res.warnings
            )),
            _ => {}
        }
    }

    // The one case in this golden that must warn: a group and one of its own hosts share a name.
    // Kept in its own tiny fixture (see homonym_inventory.ini's own comment) precisely so it does
    // not drown out the "must not warn" signal above.
    let homonym_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/homonym_inventory.ini");
    let homonym_inv = volant::inventory::Inventory::load(&homonym_path).unwrap();
    let homonym_res = homonym_inv.resolve("same");
    let homonym_got: Vec<&str> = homonym_res.hosts.iter().map(|h| h.name.as_str()).collect();
    if serde_json::to_value(&homonym_got).unwrap() != expected["homonym"]["hosts"] {
        failures.push(format!(
            "homonym pattern same: reference {}, ours {homonym_got:?}",
            expected["homonym"]["hosts"]
        ));
    }
    if expected["homonym"]["warning"].as_bool() == Some(true) && homonym_res.warnings.is_empty() {
        failures.push("homonym pattern same: the reference warns, we do not".to_string());
    }

    assert!(
        failures.is_empty(),
        "{} difference(s) with ansible-inventory:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
