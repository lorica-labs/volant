// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading variable files from disk: `group_vars`, `host_vars`, `include_vars` and `-e`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::{Map, Value};

use crate::yaml;
pub(super) fn extend(target: &mut Map<String, Value>, source: &Map<String, Value>) {
    for (k, v) in source {
        target.insert(k.clone(), v.clone());
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
            target.insert(k.clone(), v.clone());
        }
    }
}

/// `group_vars/` or `host_vars/`: for each entry, a file `name`, `name.yml`, `name.yaml`,
/// `name.json`, or a directory `name/` whose files are merged in name order. Hidden files,
/// editor backups and `.retry` files are ignored, like Ansible does.
pub(super) fn load_vars_dir(dir: &Path) -> anyhow::Result<BTreeMap<String, Map<String, Value>>> {
    let mut out: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
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
                extend(target, &load_group_file(&file)?);
            }
        } else {
            extend(target, &load_group_file(&path)?);
        }
    }
    Ok(out)
}

/// A `group_vars` or `host_vars` file. The reference stops a vaulted one it cannot read with
/// exit 4 and the bare reason.
fn load_group_file(path: &Path) -> anyhow::Result<Map<String, Value>> {
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

/// A YAML or JSON file whose document is a mapping. An empty file is an empty mapping. A vaulted
/// file is decrypted with the run's secrets first.
pub fn load_vars_file(path: &Path) -> anyhow::Result<Map<String, Value>> {
    let mut bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if crate::vault::is_encrypted(&bytes) {
        bytes = crate::vault::decrypt(&bytes, &crate::vault::run_secrets()).map_err(|e| {
            anyhow::Error::new(VaultFile {
                path: path.to_path_buf(),
                reason: crate::vault::sentence(&e.0),
            })
        })?;
    }
    let text = String::from_utf8(bytes).with_context(|| format!("reading {}", path.display()))?;
    let docs = yaml::load(&text, &path.display().to_string())?;
    match docs.first() {
        None => Ok(Map::new()),
        Some(doc) => match yaml::to_json(doc).with_context(|| format!("in {}", path.display()))? {
            Value::Object(map) => Ok(map),
            Value::Null => Ok(Map::new()),
            _ => bail!(
                "{}: a variables file must contain a mapping",
                path.display()
            ),
        },
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
pub fn parse_extra_vars(items: &[String], base: &Path) -> anyhow::Result<Map<String, Value>> {
    let mut out = Map::new();
    for item in items {
        let item = item.trim();
        if let Some(file) = item.strip_prefix('@') {
            extend(&mut out, &load_vars_file(&base.join(file))?);
        } else if item.starts_with('{') || item.starts_with('[') {
            let docs = yaml::load(item, "extra vars")?;
            match docs.first().map(yaml::to_json).transpose()? {
                Some(Value::Object(map)) => extend(&mut out, &map),
                _ => bail!("extra vars must be a mapping: {item}"),
            }
        } else {
            let (options, raw) = crate::splitter::parse_kv(item)?;
            if let Some(word) = raw {
                bail!("extra var '{word}' is not key=value");
            }
            for (k, v) in options {
                out.insert(k, Value::String(v));
            }
        }
    }
    Ok(out)
}
