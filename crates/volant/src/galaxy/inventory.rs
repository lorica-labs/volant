// SPDX-License-Identifier: GPL-3.0-or-later
//! `collection list` and `collection verify`, as ansible-galaxy 2.19.12 runs them
//! (`execute_list_collection`, `find_existing_collections`, `verify_collections`,
//! `verify_local_collection`). Both read the installed trees under the collections paths.
//! ansible-core's own internal collection, which the reference lists from inside its Python
//! package, is not listed: Volant has no such package.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::Failure;
use super::requirements::is_collection_name;

mod verify;

pub use verify::{Verify, verify};

/// An installed collection: where it is, its name and its version (`*` when its manifest gives
/// none).
struct Installed {
    path: PathBuf,
    fqcn: String,
    version: String,
}

/// The `ansible_collections` directory a search path stands for.
fn root(path: &Path) -> PathBuf {
    super::collections_root(path)
}

/// The collections under `paths`, sorted by their directories.
fn installed(
    paths: &[PathBuf],
    namespace: Option<&str>,
    name: Option<&str>,
    verbose: u8,
) -> Vec<Installed> {
    let mut found = BTreeSet::new();
    for path in paths {
        let root = root(path);
        let Ok(namespaces) = std::fs::read_dir(&root) else {
            continue;
        };
        for ns in namespaces.flatten().filter(|e| e.path().is_dir()) {
            for coll in std::fs::read_dir(ns.path()).into_iter().flatten().flatten() {
                if coll.path().is_dir() {
                    found.insert(coll.path());
                }
            }
        }
    }
    found
        .into_iter()
        .filter_map(|path| {
            let ns = path.parent()?.file_name()?.to_string_lossy().into_owned();
            let coll = path.file_name()?.to_string_lossy().into_owned();
            if namespace.is_some_and(|n| n != ns) || name.is_some_and(|n| n != coll) {
                return None;
            }
            let version = version_of(&path);
            if verbose >= 3 {
                println!(
                    "Found installed collection {ns}.{coll}:{version} at '{}'",
                    path.display()
                );
            }
            Some(Installed {
                version,
                fqcn: format!("{ns}.{coll}"),
                path,
            })
        })
        .collect()
}

/// The version of an installed or source collection: `MANIFEST.json`, else `galaxy.yml`, else
/// `*`.
fn version_of(path: &Path) -> String {
    if let Ok(data) = std::fs::read(path.join("MANIFEST.json"))
        && let Ok(json) = serde_json::from_slice::<Value>(&data)
        && let Some(version) = json["collection_info"]["version"].as_str()
    {
        return version.to_string();
    }
    if path.join("galaxy.yml").is_file()
        && let Ok(source) = super::build::Source::read(path)
    {
        return source.version();
    }
    "*".to_string()
}

/// `collection list [NAME] [-p PATH]... [--format human|yaml|json]`.
pub fn list(
    given: &[PathBuf],
    configured: &[PathBuf],
    name: Option<&str>,
    format: &str,
    verbose: u8,
) -> Result<i32, Failure> {
    let (namespace, collection) = match name {
        Some(name) => {
            if !is_collection_name(name.split(':').next().unwrap_or(name)) {
                return Err(Failure::error(format!(
                    "Invalid collection name '{name}', name must be in the format <namespace>.<collection>. \nPlease make sure namespace and collection name contains characters from [a-zA-Z0-9_] only."
                )));
            }
            let (n, c) = name.split_once('.').unwrap_or((name, ""));
            (Some(n), Some(c))
        }
        None => (None, None),
    };
    let mut paths: Vec<PathBuf> = given.to_vec();
    for path in configured {
        if !paths.contains(path) {
            paths.push(path.clone());
        }
    }
    let found = installed(&paths, namespace, collection, verbose);
    let fqcn_width = found
        .iter()
        .map(|c| c.fqcn.chars().count())
        .max()
        .unwrap_or(0);
    let version_width = found
        .iter()
        .map(|c| c.version.chars().count())
        .max()
        .unwrap_or(0);
    let mut by_path: Vec<(String, Map<String, Value>)> = Vec::new();
    let mut shown: BTreeSet<String> = BTreeSet::new();
    for coll in &found {
        let dir = coll
            .path
            .parent()
            .and_then(Path::parent)
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        if format == "human" {
            if shown.insert(dir.clone()) {
                println!(
                    "\n# {dir}\n{:fw$} {:vw$}\n{} {}",
                    "Collection",
                    "Version",
                    "-".repeat(fqcn_width.max(10)),
                    "-".repeat(version_width.max(7)),
                    fw = fqcn_width,
                    vw = version_width,
                );
            }
            println!(
                "{:fw$} {:vw$}",
                coll.fqcn,
                coll.version,
                fw = fqcn_width.max(10),
                vw = version_width.max(7)
            );
        } else {
            if !by_path.iter().any(|(p, _)| *p == dir) {
                by_path.push((dir.clone(), Map::new()));
            }
            if let Some((_, map)) = by_path.iter_mut().find(|(p, _)| *p == dir) {
                map.insert(coll.fqcn.clone(), json!({"version": coll.version}));
            }
        }
    }
    let (warnings, usable) = path_warnings(&paths, configured);
    if !(name.is_some() && !found.is_empty()) {
        for warning in warnings {
            eprintln!("[WARNING]: {warning}");
        }
    }
    if found.is_empty() && !usable {
        return Err(Failure::options(
            "- None of the provided paths were usable. Please specify a valid path with --collections-path",
        ));
    }
    let output = Value::Object(
        by_path
            .into_iter()
            .map(|(p, m)| (p, Value::Object(m)))
            .collect(),
    );
    match format {
        "json" => println!("{}", python_dumps(&output)),
        "yaml" => print!(
            "{}",
            crate::template::inventory_yaml(&crate::template::yaml_node(&output))
        ),
        _ => {}
    }
    Ok(0)
}

/// The warnings for the search paths that cannot be read, a configured one only when it exists
/// as something else than a directory, and whether one path can be read.
fn path_warnings(paths: &[PathBuf], configured: &[PathBuf]) -> (Vec<String>, bool) {
    let mut warnings = Vec::new();
    let mut usable = false;
    for path in paths {
        if !path.exists() {
            if !configured.contains(path) {
                warnings.push(format!(
                    "- the configured path {} does not exist.",
                    path.display()
                ));
            }
        } else if !path.is_dir() {
            warnings.push(format!(
                "- the configured path {}, exists, but it is not a directory.",
                path.display()
            ));
        } else {
            usable = true;
        }
    }
    (warnings, usable)
}

/// `json.dumps(value)` with its default separators, `", "` and `": "`, on one line.
fn python_dumps(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let items: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", Value::String(k.clone()), python_dumps(v)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
        Value::Array(list) => {
            let items: Vec<String> = list.iter().map(python_dumps).collect();
            format!("[{}]", items.join(", "))
        }
        other => other.to_string(),
    }
}
