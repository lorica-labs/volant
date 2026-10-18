// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use rstest::rstest;

fn cache(dir: &Path, timeout: i64) -> FactCache {
    FactCache {
        dir: dir.to_path_buf(),
        prefix: "pfx_".into(),
        timeout,
    }
}

#[test]
fn a_file_is_what_the_reference_writes() {
    let dir = tempfile::tempdir().unwrap();
    let facts = json!({"ansible_x": "é\u{1F600}\u{7f}", "n": [1, {"a": null}]});
    cache(dir.path(), 0)
        .write("h1", facts.as_object().unwrap())
        .unwrap();
    let text = std::fs::read_to_string(dir.path().join("pfx_s1_h1")).unwrap();
    assert_eq!(
        text,
        r#"{"__payload__": "{\"ansible_x\": \"\\u00e9\\ud83d\\ude00\\u007f\", \"n\": [1, {\"a\": null}]}"}"#
    );
    let back = cache(dir.path(), 0).read("h1").unwrap().unwrap();
    assert_eq!(Value::Object(back), facts);
}

#[test]
fn a_set_fact_value_comes_back_untagged() {
    let facts = json!({"s": tag(&json!("v")), "d": tag(&json!({"k": [1, true]}))});
    let dir = tempfile::tempdir().unwrap();
    cache(dir.path(), 0)
        .write("h", facts.as_object().unwrap())
        .unwrap();
    let back = cache(dir.path(), 0).read("h").unwrap().unwrap();
    assert_eq!(
        Value::Object(back),
        json!({"s": "v", "d": {"k": [1, true]}})
    );
}

#[rstest]
#[case::empty("", "Expecting value: line 1 column 1 (char 0)")]
#[case::text("bad", "Expecting value: line 1 column 1 (char 0)")]
fn a_corrupt_file_is_erased_and_reported(#[case] body: &str, #[case] why: &str) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("pfx_s1_h");
    std::fs::write(&file, body).unwrap();
    let failure = cache(dir.path(), 0).read("h").unwrap_err();
    let shown = file.display();
    assert_eq!(
        failure.warning.unwrap(),
        format!(
            "error in 'jsonfile' cache plugin while trying to read {shown} : b'{why}'. Most likely a corrupt file, so erasing and failing."
        )
    );
    assert_eq!(
        failure.error,
        format!(
            "The cache file {shown} was corrupt, or did not otherwise contain valid data. It has been removed, so you can re-run your command now."
        )
    );
    assert!(!file.exists());
}

#[test]
fn a_bad_payload_is_the_references_unguarded_crash() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pfx_s1_h"), r#"{"__payload__": "zzz"}"#).unwrap();
    let failure = cache(dir.path(), 0).read("h").unwrap_err();
    assert!(failure.warning.is_none());
    assert_eq!(
        failure.error,
        "Unexpected Exception, this is probably a bug: Expecting value: line 1 column 1 (char 0)"
    );
}

#[rstest]
#[case::never(0, false)]
#[case::fresh(3600, false)]
#[case::always(-1, true)]
fn the_timeout_is_the_age_of_the_file(#[case] timeout: i64, #[case] expired: bool) {
    let dir = tempfile::tempdir().unwrap();
    let c = cache(dir.path(), timeout);
    c.write("h", &Map::new()).unwrap();
    assert_eq!(c.read("h").unwrap().is_none(), expired);
}

#[test]
fn a_flush_removes_only_the_hosts_it_is_given() {
    let dir = tempfile::tempdir().unwrap();
    let c = cache(dir.path(), 0);
    c.write("a", &Map::new()).unwrap();
    std::fs::write(dir.path().join("pfx_s1_other"), "x").unwrap();
    c.flush(["a", "missing"]).unwrap();
    assert!(!dir.path().join("pfx_s1_a").exists());
    assert!(dir.path().join("pfx_s1_other").exists());
}

#[test]
fn the_settings_that_cannot_make_a_cache_say_so() {
    let mut config = Config {
        fact_caching: "jsonfile".into(),
        ..Config::default()
    };
    let (none, warnings) = FactCache::open(&config);
    assert!(none.is_none());
    assert_eq!(
        warnings,
        ["Required config '_uri' for 'jsonfile' cache plugin not provided."]
    );
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("f");
    std::fs::write(&blocker, "").unwrap();
    config.fact_caching_connection = Some(blocker.join("fc"));
    let (none, warnings) = FactCache::open(&config);
    assert!(none.is_none());
    assert!(
        warnings[0]
            .starts_with("Error in 'jsonfile' cache plugin while trying to create cache dir '"),
        "{warnings:?}"
    );
}
/// The reference's own file, with the origin tags emptied, comes out of a read and a write
/// byte for byte: this writer keeps no origin tags.
#[test]
fn the_reference_s_file_is_written_back_byte_for_byte() {
    fn strip(value: &mut Value) {
        match value {
            Value::Object(map) => {
                if let Some(tags) = map.get_mut("tags") {
                    *tags = json!([]);
                }
                map.values_mut().for_each(strip);
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let text = include_str!("../../tests/golden/fact_cache/pfx_s1_hostA");
    let dir = tempfile::tempdir().unwrap();
    let c = cache(dir.path(), 0);
    std::fs::write(dir.path().join("pfx_s1_hostA"), text).unwrap();
    let mut payload = serde_json::from_str::<Value>(
        serde_json::from_str::<Value>(text).unwrap()["__payload__"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        dumps(&json!({ "__payload__": dumps(&payload) })),
        text,
        "the writer spells the reference's file as the reference does"
    );
    strip(&mut payload);
    let expected = dumps(&json!({ "__payload__": dumps(&payload) }));
    // The file is named `s1_<host>`: read it under that name, write it back, compare.
    std::fs::rename(dir.path().join("pfx_s1_hostA"), dir.path().join("pfx_s1_h")).unwrap();
    let facts = c.read("h").unwrap().unwrap();
    let tagged: Map<String, Value> = facts.iter().map(|(k, v)| (k.clone(), tag(v))).collect();
    c.write("h", &tagged).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("pfx_s1_h")).unwrap(),
        expected
    );
}
