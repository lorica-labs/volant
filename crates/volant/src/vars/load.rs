// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading variable files from disk: `group_vars`, `host_vars`, `include_vars` and `-e`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::{Map, Value};

use super::Tagged;
use crate::tags::{TagTree, top_level};
use crate::yaml;

/// `hash_behaviour = merge`, set once from the configuration before anything is read.
static HASH_MERGE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Sets `hash_behaviour`: `true` for `merge`. Measured on ansible-core 2.19.12, `merge` lays a
/// mapping over a mapping of the same name key by key, between every two variable sources, and
/// prints no deprecation warning.
pub fn set_hash_merge(on: bool) {
    HASH_MERGE.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn hash_merge() -> bool {
    HASH_MERGE.load(std::sync::atomic::Ordering::Relaxed)
}

/// The variables of `existing` with `new` laid over them, mappings merged key by key all the way
/// down and anything else replaced: `merge_hash` of ansible-core.
pub(crate) fn merge_hash(
    existing: &Map<String, Value>,
    new: &Map<String, Value>,
) -> Map<String, Value> {
    let mut out = existing.clone();
    for (key, value) in new {
        let merged = match (out.get(key), value) {
            (Some(Value::Object(old)), Value::Object(fresh)) => {
                Value::Object(merge_hash(old, fresh))
            }
            _ => value.clone(),
        };
        out.insert(key.clone(), merged);
    }
    out
}

/// Lays one value over the variable of that name: `combine_vars` of ansible-core, which replaces
/// it, or merges two mappings under `hash_behaviour = merge`.
fn combine_one(target: &mut Map<String, Value>, key: &str, value: &Value) {
    let merged = match (hash_merge(), target.get(key), value) {
        (true, Some(Value::Object(old)), Value::Object(fresh)) => {
            Value::Object(merge_hash(old, fresh))
        }
        _ => value.clone(),
    };
    target.insert(key.to_string(), merged);
}

pub(crate) fn extend(target: &mut Map<String, Value>, source: &Map<String, Value>) {
    for (k, v) in source {
        combine_one(target, k, v);
    }
}

/// `extend`, leaving out the names in `skip`.
pub(super) fn extend_except(
    target: &mut Map<String, Value>,
    source: &Map<String, Value>,
    skip: &BTreeSet<String>,
) {
    for (k, v) in source {
        if !skip.contains(k) {
            combine_one(target, k, v);
        }
    }
}

/// `group_vars/` or `host_vars/`: for each entry, a file `name`, `name.yml`, `name.yaml`,
/// `name.json`, or a directory `name/` whose files are merged in name order. Hidden files,
/// editor backups and `.retry` files are ignored, like Ansible does.
pub(crate) fn load_vars_dir(dir: &Path) -> anyhow::Result<BTreeMap<String, Tagged>> {
    let mut out: BTreeMap<String, Tagged> = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(out);
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if ignored(file_name) {
            continue;
        }
        let name = strip_vars_extension(file_name).to_string();
        let target = out.entry(name).or_default();
        if path.is_dir() {
            let mut inner: Vec<PathBuf> = std::fs::read_dir(&path)?
                .filter_map(Result::ok)
                .map(|e| e.path())
                .collect();
            inner.sort();
            for file in inner.into_iter().filter(|p| p.is_file()) {
                if file
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(ignored)
                {
                    continue;
                }
                target.extend(&load_group_file(&file)?);
            }
        } else {
            target.extend(&load_group_file(&path)?);
        }
    }
    Ok(out)
}

/// A `group_vars` or `host_vars` file. The reference stops a vaulted one it cannot read with
/// exit 4 and the bare reason.
fn load_group_file(path: &Path) -> anyhow::Result<Tagged> {
    load_vars_file(path).map_err(|err| match err.downcast_ref::<VaultFile>() {
        Some(vault) => crate::stats::Refusal::at(4, &vault.reason),
        None => err,
    })
}

fn ignored(file_name: &str) -> bool {
    file_name.starts_with('.') || file_name.ends_with('~') || file_name.ends_with(".retry")
}

fn strip_vars_extension(file_name: &str) -> &str {
    for ext in [".yml", ".yaml", ".json"] {
        if let Some(stem) = file_name.strip_suffix(ext) {
            return stem;
        }
    }
    file_name
}

/// A vaulted file that could not be read, with the reference's reason.
#[derive(Debug)]
pub struct VaultFile {
    pub path: PathBuf,
    pub reason: String,
}

impl std::fmt::Display for VaultFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Invalid vars_files file '{}': {}",
            self.path.display(),
            self.reason
        )
    }
}

impl std::error::Error for VaultFile {}

/// A YAML or JSON file read, before anyone checks what its document is.
pub struct VarsDocument {
    /// The first document of the file, or null when there is none.
    pub value: Value,
    /// Whether the file was vaulted: `include_vars` hides the result of a task that read one.
    pub encrypted: bool,
    /// The tags of `value`: where each part was written, every string trusted.
    pub tags: Option<TagTree>,
}

/// The first document of a YAML or JSON file. A vaulted file is decrypted with the run's secrets
/// first, and a failure of the vault is a [`VaultFile`] inside the error.
pub fn read_vars_document(path: &Path) -> anyhow::Result<VarsDocument> {
    let mut bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let encrypted = crate::vault::is_encrypted(&bytes);
    if encrypted {
        bytes = crate::vault::decrypt(&bytes, &crate::vault::run_secrets()).map_err(|e| {
            anyhow::Error::new(VaultFile {
                path: path.to_path_buf(),
                reason: crate::vault::sentence(&e.0),
            })
        })?;
    }
    let text = String::from_utf8(bytes).with_context(|| format!("reading {}", path.display()))?;
    let source = path.display().to_string();
    let (docs, origins) = yaml::load_with_origins(&text, &source)?;
    let (value, tags) = match docs.first() {
        None => (Value::Null, None),
        Some(doc) => {
            let value = yaml::to_json(doc).with_context(|| format!("in {}", path.display()))?;
            let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
            let absolute = absolute.display().to_string();
            (value, yaml::tags_of(doc, &origins, Some(&absolute), true))
        }
    };
    yaml::note_value_origins(&docs, &origins, &source, &text);
    Ok(VarsDocument {
        value,
        encrypted,
        tags,
    })
}

/// A YAML or JSON file whose document is a mapping. An empty file is an empty mapping. A vaulted
/// file is decrypted with the run's secrets first.
pub fn load_vars_file(path: &Path) -> anyhow::Result<Tagged> {
    let document = read_vars_document(path)?;
    match document.value {
        Value::Object(map) => Ok(Tagged::new(map, top_level(document.tags))),
        Value::Null => Ok(Tagged::default()),
        _ => bail!(
            "{}: a variables file must contain a mapping",
            path.display()
        ),
    }
}

/// A play's `vars_files` file: a mapping, or a list of mappings merged in order, a later one
/// winning, as ansible-core 2.19.12's `preprocess_vars` reads one.
pub fn load_play_vars_file(path: &Path) -> anyhow::Result<Tagged> {
    let document = read_vars_document(path)?;
    match document.value {
        Value::Array(items) if items.iter().all(Value::is_object) => {
            let mut merged = Tagged::default();
            for (i, item) in items.into_iter().enumerate() {
                if let Value::Object(map) = item {
                    let tags = top_level(document.tags.as_ref().and_then(|t| t.index(i)).cloned());
                    for (name, value) in map {
                        let tree = tags.get(&name).cloned();
                        merged.insert(name, value, tree);
                    }
                }
            }
            Ok(merged)
        }
        Value::Array(_) => bail!(
            "{}: a variables file must contain a mapping or a list of mappings",
            path.display()
        ),
        _ => load_vars_file(path),
    }
}

/// Every path an `include_vars` naming `name` looks in, in the order it looks.
///
/// Measured on ansible-core 2.19.12 by reading the list a missing file prints. From a task inside
/// a role whose `tasks/` wrote the statement, the six are `<role>/vars/`, `<role>/`,
/// `<role>/tasks/vars/`, `<role>/tasks/`, `<playbook>/vars/` and `<playbook>/`: three bases, each
/// with its `vars/` first. From a playbook task the role base is absent and the other two are the
/// same directory, which is why the reference's own message lists the playbook's two **twice** -
/// reproduced here rather than deduplicated, because the message is what the operator reads and
/// the list is the message.
///
/// An absolute name short-circuits the walk: it is the one path, and the only one worth naming.
pub fn include_vars_paths(
    file_dir: &Path,
    role_dir: Option<&Path>,
    playbook_dir: &Path,
    name: &str,
) -> Vec<PathBuf> {
    let named = Path::new(name);
    if named.is_absolute() {
        return vec![named.to_path_buf()];
    }
    let mut out = Vec::new();
    for base in role_dir.into_iter().chain([file_dir, playbook_dir]) {
        out.push(base.join("vars").join(name));
        out.push(base.join(name));
    }
    out
}

/// `-e` values: `key=value` pairs (strings), inline JSON or YAML mappings, and `@file`.
/// Later items win over earlier ones.
///
/// A value given inline carries the origin `<CLI option 'extra_vars'>` and is trusted, as the
/// reference tags the option's text; a file's values carry their place in it.
pub fn parse_extra_vars(items: &[String], base: &Path) -> anyhow::Result<Tagged> {
    let mut out = Tagged::default();
    let origin = crate::tags::origin_described("<CLI option 'extra_vars'>");
    let inline = |out: &mut Tagged, map: &Map<String, Value>| {
        for (name, value) in map {
            out.combine_one(
                name,
                value,
                crate::tags::uniform(value, origin, true).as_ref(),
            );
        }
    };
    for item in items {
        let item = item.trim();
        if let Some(file) = item.strip_prefix('@') {
            out.extend(&load_vars_file(&base.join(file))?);
        } else if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(item) {
            // JSON is read as ansible-core's own decoder reads it, `__ansible_unsafe` and all:
            // measured, `-e '{"x": {"__ansible_unsafe": "{{ 1 }}"}}'` keeps `{{ 1 }}` as text.
            if let Value::Object(map) = yaml::unsafe_markers(&Value::Object(map)) {
                inline(&mut out, &map);
            }
        } else if item.starts_with('{') || item.starts_with('[') {
            let docs = yaml::load(item, "extra vars")?;
            match docs.first().map(yaml::to_json).transpose()? {
                Some(Value::Object(map)) => inline(&mut out, &map),
                _ => bail!("extra vars must be a mapping: {item}"),
            }
        } else {
            let (options, raw) = crate::splitter::parse_kv(item)?;
            if let Some(word) = raw {
                bail!("extra var '{word}' is not key=value");
            }
            let map = options
                .into_iter()
                .map(|(k, v)| (k, Value::String(yaml::literal_escapes(&v))))
                .collect();
            inline(&mut out, &map);
        }
    }
    Ok(out)
}
