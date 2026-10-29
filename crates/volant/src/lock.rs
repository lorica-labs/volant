// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant.lock`: the Volant version, the managed runtime and a hash of every installed
//! collection and role, so two machines with the same checkout learn they differ before a run.
//! The file sits next to the `ansible.cfg` in effect, else in the working directory.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::config::Config;

/// The name of the file.
pub(crate) const FILE: &str = "volant.lock";

/// The only profile there is until profiles exist.
const CORE_VERSION: &str = "2.19";

/// What a lock records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lock {
    pub(crate) volant: String,
    pub(crate) core_version: String,
    pub(crate) runtime: Option<Runtime>,
    pub(crate) collections: Vec<Entry>,
    pub(crate) roles: Vec<Entry>,
}

/// The managed runtime: its versions and the SHA-256 of the manifest it was installed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Runtime {
    pub(crate) python: String,
    pub(crate) ansible_core: String,
    pub(crate) sha256: String,
}

/// An installed collection or role and the hash of its tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) sha256: String,
}

/// Where the lock is read and written.
#[must_use]
pub(crate) fn path() -> PathBuf {
    crate::config::locate()
        .and_then(|cfg| std::path::absolute(cfg).ok())
        .and_then(|cfg| cfg.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(FILE)
}

/// What is installed now.
#[must_use]
pub(crate) fn current(config: &Config) -> Lock {
    Lock {
        volant: env!("CARGO_PKG_VERSION").to_owned(),
        core_version: CORE_VERSION.to_owned(),
        runtime: runtime(),
        collections: collections(&config.collections_path),
        roles: roles(&config.roles_path),
    }
}

fn runtime() -> Option<Runtime> {
    let manifest = crate::runtime::manifest::Manifest::pinned().ok()?;
    let runtimes = crate::runtime::runtimes_dir().ok()?;
    crate::runtime::install::installed(&runtimes, &manifest.id())?;
    Some(Runtime {
        python: manifest.python.clone(),
        ansible_core: manifest.ansible_core.clone(),
        sha256: manifest.digest().to_owned(),
    })
}

/// The sorted entries of a directory, directories only.
fn dirs(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    found.sort();
    found
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// The collections the search paths hold, the first path that has a name winning, as the
/// collection loader picks it.
fn collections(paths: &[PathBuf]) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for path in paths {
        for namespace in dirs(&crate::galaxy::collections_root(path)) {
            for collection in dirs(&namespace) {
                let name = format!("{}.{}", file_name(&namespace), file_name(&collection));
                if out.iter().any(|e| e.name == name) {
                    continue;
                }
                out.push(Entry {
                    version: collection_version(&collection),
                    sha256: tree_hash(&collection, &[]),
                    name,
                });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// `MANIFEST.json`'s version, else `galaxy.yml`'s, else `*`.
fn collection_version(dir: &Path) -> String {
    let manifest = std::fs::read(dir.join("MANIFEST.json"))
        .ok()
        .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok());
    if let Some(version) = manifest
        .as_ref()
        .and_then(|m| m["collection_info"]["version"].as_str())
    {
        return version.to_owned();
    }
    yaml_value(&dir.join("galaxy.yml"), "version").unwrap_or_else(|| "*".to_owned())
}

/// The roles the roles paths hold, the first path that has a name winning.
fn roles(paths: &[PathBuf]) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for path in paths {
        for role in dirs(path) {
            let name = file_name(&role);
            let is_role = ["meta/main.yml", "meta/main.yaml"]
                .iter()
                .any(|m| role.join(m).is_file());
            if !is_role || out.iter().any(|e| e.name == name) {
                continue;
            }
            out.push(Entry {
                version: yaml_value(&role.join("meta/.galaxy_install_info"), "version")
                    .unwrap_or_default(),
                sha256: tree_hash(&role, &["meta/.galaxy_install_info"]),
                name,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// A top-level text value of a YAML file.
fn yaml_value(file: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    let docs = crate::yaml::load(&text, &file.display().to_string()).ok()?;
    let value = crate::yaml::to_json(docs.first()?).ok()?;
    match &value[key] {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// The SHA-256 of a tree: each file's relative path, `/`-separated, and the SHA-256 of its
/// content, in sorted path order. `__pycache__` directories, `*.pyc` files and the paths of
/// `skip` are left out, so the hash follows the content and not the machine.
#[must_use]
pub(crate) fn tree_hash(dir: &Path, skip: &[&str]) -> String {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if path.is_dir() {
                if name != "__pycache__" {
                    walk(&path, &rel, out);
                }
            } else if !name.ends_with(".pyc") {
                out.push((rel, path));
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, "", &mut files);
    files.retain(|(rel, _)| !skip.contains(&rel.as_str()));
    files.sort();
    let mut hasher = Sha256::new();
    for (rel, path) in files {
        let content = std::fs::read(&path).unwrap_or_default();
        hasher.update(rel.as_bytes());
        hasher.update(b"\0");
        hasher.update(format!("{:x}", Sha256::digest(&content)).as_bytes());
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())
}

/// The lock as TOML, the same bytes for the same lock.
#[must_use]
pub(crate) fn render(lock: &Lock) -> String {
    let mut out = format!(
        "format = 1\nvolant = {}\ncore_version = {}\n",
        quoted(&lock.volant),
        quoted(&lock.core_version)
    );
    if let Some(runtime) = &lock.runtime {
        let _ = writeln!(
            out,
            "runtime = {{ python = {}, ansible_core = {}, sha256 = {} }}",
            quoted(&runtime.python),
            quoted(&runtime.ansible_core),
            quoted(&runtime.sha256)
        );
    }
    for (table, entries) in [("collection", &lock.collections), ("role", &lock.roles)] {
        for entry in entries {
            let _ = write!(
                out,
                "\n[[{table}]]\nname = {}\nversion = {}\nsha256 = {}\n",
                quoted(&entry.name),
                quoted(&entry.version),
                quoted(&entry.sha256)
            );
        }
    }
    out
}

/// A TOML basic string.
fn quoted(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Reads a lock written by [`render`], or any TOML of the same shape.
pub(crate) fn parse(text: &str) -> Result<Lock, String> {
    let doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{e}"))?;
    let text_of = |item: &toml_edit::Item, key: &str| -> Result<String, String> {
        item.get(key)
            .and_then(toml_edit::Item::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("`{key}` is missing or not a string"))
    };
    let root = doc.as_item();
    match doc.get("format").and_then(toml_edit::Item::as_integer) {
        Some(1) => {}
        _ => return Err("`format` is not 1".to_owned()),
    }
    let runtime = match doc.get("runtime") {
        None => None,
        Some(item) => {
            let table = item.as_inline_table().map_or_else(
                || item.clone(),
                |t| toml_edit::Item::Table(t.clone().into_table()),
            );
            Some(Runtime {
                python: text_of(&table, "python")?,
                ansible_core: text_of(&table, "ansible_core")?,
                sha256: text_of(&table, "sha256")?,
            })
        }
    };
    let entries = |key: &str| -> Result<Vec<Entry>, String> {
        let Some(item) = doc.get(key) else {
            return Ok(Vec::new());
        };
        let tables = item
            .as_array_of_tables()
            .ok_or_else(|| format!("`{key}` is not an array of tables"))?;
        tables
            .iter()
            .map(|t| {
                let item = toml_edit::Item::Table(t.clone());
                Ok(Entry {
                    name: text_of(&item, "name")?,
                    version: text_of(&item, "version")?,
                    sha256: text_of(&item, "sha256")?,
                })
            })
            .collect()
    };
    Ok(Lock {
        volant: text_of(root, "volant")?,
        core_version: text_of(root, "core_version")?,
        runtime,
        collections: entries("collection")?,
        roles: entries("role")?,
    })
}

/// Each way `now` differs from `locked`, one line each.
#[must_use]
pub(crate) fn differences(locked: &Lock, now: &Lock) -> Vec<String> {
    let mut out = Vec::new();
    if locked.volant != now.volant {
        out.push(format!(
            "volant: {} in the lock, {} running",
            locked.volant, now.volant
        ));
    }
    if locked.core_version != now.core_version {
        out.push(format!(
            "core_version: {} in the lock, {} running",
            locked.core_version, now.core_version
        ));
    }
    match (&locked.runtime, &now.runtime) {
        (Some(_), None) => out.push("runtime: in the lock, not installed".to_owned()),
        (None, Some(_)) => out.push("runtime: installed, not in the lock".to_owned()),
        (Some(a), Some(b)) if a != b => out.push(format!(
            "runtime: python {} and ansible-core {} in the lock, python {} and ansible-core {} installed",
            a.python, a.ansible_core, b.python, b.ansible_core
        )),
        _ => {}
    }
    for (kind, locked, now) in [
        ("collection", &locked.collections, &now.collections),
        ("role", &locked.roles, &now.roles),
    ] {
        for want in locked {
            match now.iter().find(|e| e.name == want.name) {
                None => out.push(format!("{kind} {}: in the lock, not installed", want.name)),
                Some(have) if have.version != want.version => out.push(format!(
                    "{kind} {}: {} in the lock, {} installed",
                    want.name, want.version, have.version
                )),
                Some(have) if have.sha256 != want.sha256 => out.push(format!(
                    "{kind} {} {}: its files differ from the lock",
                    want.name, want.version
                )),
                Some(_) => {}
            }
        }
        for have in now {
            if !locked.iter().any(|e| e.name == have.name) {
                out.push(format!(
                    "{kind} {} {}: installed, not in the lock",
                    have.name, have.version
                ));
            }
        }
    }
    out
}

/// `volant lock [--check]`: writes the lock, or with `check` lists how the installed tree
/// differs from it and exits 1 when it does.
#[must_use]
pub fn run(check: bool) -> i32 {
    let config = match Config::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("ERROR! {e:#}");
            return 1;
        }
    };
    let file = path();
    let now = current(&config);
    if !check {
        return match std::fs::write(&file, render(&now)) {
            Ok(()) => {
                println!("wrote {}", file.display());
                0
            }
            Err(e) => {
                eprintln!("ERROR! cannot write {}: {e}", file.display());
                1
            }
        };
    }
    let locked = match read(&file) {
        Ok(locked) => locked,
        Err(e) => {
            eprintln!("ERROR! {e}");
            return 1;
        }
    };
    let changes = differences(&locked, &now);
    for change in &changes {
        println!("{change}");
    }
    i32::from(!changes.is_empty())
}

/// The lock at `file`, or why it cannot be read.
pub(crate) fn read(file: &Path) -> Result<Lock, String> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    parse(&text).map_err(|e| format!("{} cannot be read: {e}", file.display()))
}

/// Before a run: nothing without a lock; a warning per difference; a refusal with exit 4 when
/// `--locked` or `[volant] locked` is set and the tree differs, or the lock cannot be read.
pub(crate) fn before_run(
    config: &Config,
    locked: bool,
    out: &mut crate::render::Renderer,
) -> anyhow::Result<()> {
    let locked = locked || config.locked;
    let mut warn = |text: &str| out.warning(text, false);
    let file = path();
    if !file.is_file() {
        return Ok(());
    }
    let changes = match read(&file) {
        Ok(lock) => differences(&lock, &current(config)),
        Err(e) if locked => return Err(crate::stats::Refusal::at(4, e)),
        Err(e) => {
            warn(&e);
            return Ok(());
        }
    };
    if changes.is_empty() {
        return Ok(());
    }
    if locked {
        return Err(crate::stats::Refusal::at(
            4,
            format!(
                "the installed tree differs from {}:\n{}",
                file.display(),
                changes
                    .iter()
                    .map(|c| format!("  {c}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        ));
    }
    for change in &changes {
        warn(&format!("{FILE}: {change}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
