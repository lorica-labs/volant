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

/// The deepest JSON a cache entry may hold. `serde_json` stops at 128 levels unless told not to,
/// and the writer wraps each level of a `set_fact` value in a second object, so a value that the
/// YAML loader accepts is about twice as deep in the file. A file this deep is read with the
/// limit off, which is safe because the depth is counted first, without recursion.
const MAX_JSON_DEPTH: usize = 2 * crate::yaml::MAX_DEPTH + 8;

/// How deep `serde_json` reads before it stops, less a margin for the object around a payload.
const PLAIN_JSON_DEPTH: usize = 120;

/// The deepest nesting of brackets in the JSON `text`, counted without recursion.
fn json_depth(text: &str) -> usize {
    let (mut depth, mut deepest, mut in_string, mut escaped) = (0usize, 0usize, false, false);
    for byte in text.bytes() {
        match (in_string, byte) {
            (true, _) if escaped => escaped = false,
            (true, b'\\') => escaped = true,
            (true, b'"') => in_string = false,
            (false, b'"') => in_string = true,
            (false, b'[' | b'{') => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            (false, b']' | b'}') => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    deepest
}

/// Whether a `set_fact` value can be written to the file and read back.
pub fn fits(value: &Value) -> bool {
    let (mut deepest, mut stack) = (0, vec![(value, 1)]);
    while let Some((value, depth)) = stack.pop() {
        deepest = deepest.max(depth);
        match value {
            Value::Array(items) => stack.extend(items.iter().map(|v| (v, depth + 2))),
            Value::Object(map) => stack.extend(map.values().map(|v| (v, depth + 2))),
            _ => {}
        }
    }
    deepest < MAX_JSON_DEPTH
}

/// `text` as JSON, past the 128 levels `serde_json` allows by default.
fn parse_nested(text: &str) -> serde_json::Result<Value> {
    let mut de = serde_json::Deserializer::from_str(text);
    de.disable_recursion_limit();
    let mut values = de.into_iter::<Value>();
    match (values.next(), values.next()) {
        (Some(Ok(value)), None) => Ok(value),
        (Some(Err(e)), _) => Err(e),
        // Nothing, or more than one value: the plain parser words the error.
        _ => serde_json::from_str(text),
    }
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
        Value::Number(n) if !n.is_f64() => ("_AnsibleTaggedInt", value.clone()),
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
        let depth = json_depth(&payload);
        if depth > MAX_JSON_DEPTH {
            eprintln!(
                "[WARNING]: The fact cache entry of {host} nests deeper than {MAX_JSON_DEPTH} levels and is ignored."
            );
            return Ok(None);
        }
        let parsed = if depth > PLAIN_JSON_DEPTH {
            parse_nested(&payload)
        } else {
            serde_json::from_str::<Value>(&payload)
        };
        match parsed {
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
        if json_depth(&payload) > MAX_JSON_DEPTH {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("the facts nest deeper than {MAX_JSON_DEPTH} levels"),
            ));
        }
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
mod tests;
