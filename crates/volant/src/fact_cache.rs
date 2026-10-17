// SPDX-License-Identifier: GPL-3.0-or-later
//! The `jsonfile` fact cache, in the bytes ansible-core 2.19.12 writes: one file per host named
//! `<dir>/<prefix>s1_<host>`, holding `{"__payload__": "<json>"}` as Python's `json.dumps` spells
//! it (`", "` and `": "` separators, ASCII only), written to a temporary file and renamed.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{Map, Value, json};

use crate::config::Config;

/// The cache of one run: where the files live and how long they stay fresh.
#[derive(Debug, Clone)]
pub struct FactCache {
    dir: PathBuf,
    prefix: String,
    timeout: i64,
}

/// What a read that found a broken file reports: a warning to print first, and the error that
/// stops the run.
#[derive(Debug)]
pub struct CacheFailure {
    pub warning: Option<String>,
    pub error: String,
}

/// `json.dumps` with its defaults: `", "` between items, `": "` after a key, and every character
/// outside printable ASCII written as `\uXXXX`.
fn dumps(value: &Value) -> String {
    fn quote(text: &str, out: &mut String) {
        out.push('"');
        for c in text.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{8}' => out.push_str("\\b"),
                '\u{c}' => out.push_str("\\f"),
                ' '..='~' => out.push(c),
                _ => {
                    let mut units = [0u16; 2];
                    for unit in c.encode_utf16(&mut units) {
                        let _ = write!(out, "\\u{unit:04x}");
                    }
                }
            }
        }
        out.push('"');
    }
    fn walk(value: &Value, out: &mut String) {
        match value {
            Value::String(text) => quote(text, out),
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    walk(item, out);
                }
                out.push(']');
            }
            Value::Object(map) => {
                out.push('{');
                for (i, (key, item)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    quote(key, out);
                    out.push_str(": ");
                    walk(item, out);
                }
                out.push('}');
            }
            scalar => out.push_str(&scalar.to_string()),
        }
    }
    let mut out = String::new();
    walk(value, &mut out);
    out
}

/// A value as the reference tags it when `set_fact` caches it: the value, no tags, and the type
/// the tagging wrapper reads it back by. Booleans, `null` and floats are not taggable there.
pub fn tag(value: &Value) -> Value {
    let (kind, inner) = match value {
        Value::String(_) => ("_AnsibleTaggedStr", value.clone()),
        Value::Number(n) if n.is_i64() || n.is_u64() => ("_AnsibleTaggedInt", value.clone()),
        Value::Array(items) => (
            "_AnsibleTaggedList",
            Value::Array(items.iter().map(tag).collect()),
        ),
        Value::Object(map) => (
            "_AnsibleTaggedDict",
            Value::Object(map.iter().map(|(k, v)| (k.clone(), tag(v))).collect()),
        ),
        other => return other.clone(),
    };
    json!({"value": inner, "tags": [], "__ansible_type": kind})
}

/// The plain value under whatever tagging the writer put on it.
fn untag(value: Value) -> Value {
    match value {
        Value::Object(mut map) => {
            let tagged = map
                .get("__ansible_type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.starts_with("_AnsibleTagged"));
            match (tagged, map.remove("value")) {
                (true, Some(inner)) => untag(inner),
                (_, Some(inner)) => {
                    map.insert("value".into(), untag(inner));
                    Value::Object(map.into_iter().map(|(k, v)| (k, untag(v))).collect())
                }
                (_, None) => Value::Object(map.into_iter().map(|(k, v)| (k, untag(v))).collect()),
            }
        }
        Value::Array(items) => Value::Array(items.into_iter().map(untag).collect()),
        other => other,
    }
}

/// Python's wording of a JSON syntax error, for the cases it has words for.
fn python_json_error(err: &serde_json::Error) -> String {
    let at = err.column().saturating_sub(1);
    let text = err.to_string();
    if text.starts_with("expected value") || text.starts_with("EOF while parsing a value") {
        format!(
            "Expecting value: line {} column {} (char {at})",
            err.line().max(1),
            at + 1
        )
    } else {
        text
    }
}

impl FactCache {
    /// The cache the settings ask for, with the warnings the reference prints while it loads the
    /// plugin. `None` for `memory`, and for a `jsonfile` that cannot be set up: the run goes on
    /// without it, as the reference's does.
    pub fn open(config: &Config) -> (Option<Self>, Vec<String>) {
        if config.fact_caching != "jsonfile" {
            return (None, Vec::new());
        }
        let Some(dir) = config
            .fact_caching_connection
            .as_ref()
            .map(|d| std::path::absolute(d).unwrap_or_else(|_| d.clone()))
        else {
            return (
                None,
                vec![
                    "Required config '_uri' for 'jsonfile' cache plugin not provided.".to_string(),
                ],
            );
        };
        if let Err(err) = std::fs::create_dir_all(&dir) {
            // Python names the first directory it could not make: the topmost missing one.
            let failed = dir
                .ancestors()
                .take_while(|p| !p.as_os_str().is_empty() && !p.exists())
                .last()
                .unwrap_or(&dir);
            let why = err.to_string();
            let why = why.split(" (os error").next().unwrap_or(&why);
            let code = err.raw_os_error().unwrap_or(0);
            return (
                None,
                vec![format!(
                    "Error in 'jsonfile' cache plugin while trying to create cache dir '{}': [Errno {code}] {why}: '{}'",
                    dir.display(),
                    failed.display()
                )],
            );
        }
        let cache = Self {
            dir,
            prefix: config.fact_caching_prefix.clone(),
            timeout: config.fact_caching_timeout,
        };
        (Some(cache), Vec::new())
    }

    fn path(&self, host: &str) -> PathBuf {
        self.dir.join(format!("{}s1_{host}", self.prefix))
    }

    /// Whether `file` was last written longer ago than the timeout. `0` never expires; a negative
    /// timeout always has, as in the reference.
    fn expired(&self, file: &Path) -> bool {
        if self.timeout == 0 {
            return false;
        }
        let Ok(modified) = std::fs::metadata(file).and_then(|m| m.modified()) else {
            return true;
        };
        let age = SystemTime::now()
            .duration_since(modified)
            .map_or(0.0, |d| d.as_secs_f64());
        #[expect(clippy::cast_precision_loss, reason = "a timeout in seconds")]
        let timeout = self.timeout as f64;
        age > timeout
    }

    /// The facts cached for `host`, plain, or `None` when there are none or they are too old.
    /// A file that is not what the writer produces is erased and reported.
    pub fn read(&self, host: &str) -> Result<Option<Map<String, Value>>, CacheFailure> {
        let file = self.path(host);
        let Ok(bytes) = std::fs::read(&file) else {
            return Ok(None);
        };
        if self.expired(&file) {
            return Ok(None);
        }
        let outer = serde_json::from_slice::<Value>(&bytes)
            .map_err(|e| python_json_error(&e))
            .and_then(|v| match v {
                Value::Object(mut m) => match m.remove("__payload__") {
                    Some(Value::String(payload)) => Ok(payload),
                    _ => Err("'__payload__'".to_string()),
                },
                _ => Err("'__payload__'".to_string()),
            });
        let payload = match outer {
            Ok(payload) => payload,
            Err(why) => {
                let shown = file.display();
                let _ = std::fs::remove_file(&file);
                return Err(CacheFailure {
                    warning: Some(format!(
                        "error in 'jsonfile' cache plugin while trying to read {shown} : b'{why}'. Most likely a corrupt file, so erasing and failing."
                    )),
                    error: format!(
                        "The cache file {shown} was corrupt, or did not otherwise contain valid data. It has been removed, so you can re-run your command now."
                    ),
                });
            }
        };
        // The reference does not guard this step: a bad payload is its own crash.
        match serde_json::from_str::<Value>(&payload) {
            Ok(Value::Object(facts)) => Ok(Some(
                facts
                    .into_iter()
                    .map(|(k, v)| (k, untag(v)))
                    .collect::<Map<_, _>>(),
            )),
            Ok(_) => Err(CacheFailure {
                warning: None,
                error: "Unexpected Exception, this is probably a bug: the cached facts are not a dictionary".to_string(),
            }),
            Err(e) => Err(CacheFailure {
                warning: None,
                error: format!(
                    "Unexpected Exception, this is probably a bug: {}",
                    python_json_error(&e)
                ),
            }),
        }
    }

    /// Writes `host`'s file whole: a temporary file in the directory, mode 0644, then a rename.
    /// `facts` is in the form the file holds, see [`tag`].
    pub fn write(&self, host: &str, facts: &Map<String, Value>) -> std::io::Result<()> {
        let payload = dumps(&Value::Object(facts.clone()));
        let body = dumps(&json!({ "__payload__": payload }));
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        tmp.write_all(body.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            tmp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o644))?;
        }
        tmp.persist(self.path(host)).map_err(|e| e.error)?;
        Ok(())
    }

    /// Removes the entries of `hosts` and nothing else in the directory.
    pub fn flush<'a>(&self, hosts: impl IntoIterator<Item = &'a str>) -> std::io::Result<()> {
        for host in hosts {
            match std::fs::remove_file(self.path(host)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
        let text = include_str!("../tests/golden/fact_cache/pfx_s1_hostA");
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
}
