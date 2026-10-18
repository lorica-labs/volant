// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use rstest::rstest;
use serde_json::json;

fn as_written(_: &str, value: &Value) -> Rendered {
    Ok(value.as_object().cloned().unwrap_or_default())
}

fn groups() -> BTreeMap<String, Vec<String>> {
    BTreeMap::from([(
        "ansible.builtin.testgroup".to_string(),
        vec!["ansible.builtin.ping".to_string()],
    )])
}

#[rstest]
#[case::the_innermost_layer_replaces_whole(
    json!([{"debug": {"msg": "outer", "verbosity": 1}}, {"debug": {"var": "x"}}]),
    "ansible.builtin.debug",
    json!({"var": "x"})
)]
#[case::the_last_spelling_listed_wins(
    json!([{"debug": {"msg": "short"}, "ansible.builtin.debug": {"msg": "fq"}}]),
    "debug",
    json!({"msg": "fq"})
)]
#[case::a_legacy_call_reads_a_short_key(
    json!([{"debug": {"msg": "short"}}]),
    "ansible.legacy.debug",
    json!({"msg": "short"})
)]
#[case::another_module_is_left_alone(json!([{"debug": {"msg": "x"}}]), "ping", json!({}))]
#[case::a_group_applies_under_the_module_s_own_key(
    json!([{"group/testgroup": {"data": "g", "x": 1}}, {"ping": {"data": "own"}}]),
    "ping",
    json!({"data": "own", "x": 1})
)]
#[case::a_module_outside_the_group_gets_nothing(
    json!([{"group/testgroup": {"data": "g"}}]),
    "debug",
    json!({})
)]
fn module_defaults_follow_the_reference(
    #[case] layers: Value,
    #[case] module: &str,
    #[case] want: Value,
) {
    let layers = layers.as_array().expect("layers");
    assert_eq!(
        Value::Object(defaults_for(module, layers, &groups(), &mut as_written).expect("defaults")),
        want
    );
}

/// `my.col` has `hello`, `my.col2` has `hello` and `only2`.
fn installed() -> (tempfile::TempDir, RoleSearch) {
    let dir = tempfile::tempdir().expect("a scratch directory");
    for (collection, module) in [("col", "hello"), ("col2", "hello"), ("col2", "only2")] {
        let modules = dir
            .path()
            .join("ansible_collections/my")
            .join(collection)
            .join("plugins/modules");
        std::fs::create_dir_all(&modules).expect("a modules directory");
        std::fs::write(modules.join(format!("{module}.py")), "").expect("a module");
    }
    let search = RoleSearch {
        paths: Vec::new(),
        collections: vec![dir.path().to_path_buf()],
    };
    (dir, search)
}

#[rstest]
#[case::first_listed_wins(&["my.col", "my.col2"], "hello", "my.col.hello", 0)]
#[case::order_decides(&["my.col2", "my.col"], "hello", "my.col2.hello", 0)]
#[case::a_later_collection_is_searched(&["my.col", "my.col2"], "only2", "my.col2.only2", 0)]
#[case::a_builtin_name_stays(&["my.col"], "debug", "debug", 0)]
#[case::a_full_name_stays(&["my.col2"], "my.col.hello", "my.col.hello", 0)]
#[case::a_missing_collection_warns_and_is_skipped(&["no.such", "my.col"], "hello", "my.col.hello", 1)]
#[case::a_name_nobody_has_warns_for_each_missing(&["no.such", "other.thing"], "debug", "debug", 2)]
#[case::a_name_that_is_a_path_is_never_looked_up(&["my.col"], "../hello", "../hello", 0)]
fn a_short_name_goes_to_the_first_listed_collection_that_has_it(
    #[case] listed: &[&str],
    #[case] module: &str,
    #[case] want: &str,
    #[case] warnings: usize,
) {
    let (_dir, search) = installed();
    let mut task = PlayTask::empty();
    task.module = module.to_string();
    task.collections = Some(listed.iter().map(ToString::to_string).collect());
    let mut said = Vec::new();
    resolve(&mut task, &search, &mut said);
    assert_eq!(
        (task.module.as_str(), said.len()),
        (want, warnings),
        "{said:?}"
    );
}

#[test]
fn a_group_nobody_defined_names_itself() {
    let layers = [json!({"group/nosuch": {"a": 1}})];
    assert_eq!(
        defaults_for("ping", &layers, &groups(), &mut as_written),
        Err(unresolved_group("ansible.builtin.nosuch"))
    );
}
