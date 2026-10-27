// SPDX-License-Identifier: GPL-3.0-or-later
//! `collection list` and `collection verify`, as ansible-galaxy 2.19.12 runs them
//! (`execute_list_collection`, `find_existing_collections`, `verify_collections`,
//! `verify_local_collection`). Both read the installed trees under the collections paths.
//! ansible-core's own internal collection, which the reference lists from inside its Python
//! package, is not listed: Volant has no such package.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use super::Failure;
use super::api::servers::Servers;
use super::artifact::Tarball;
use super::install::download_dir;
use super::requirements::{Collection, Kind, is_collection_name};

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
fn installed(paths: &[PathBuf], namespace: Option<&str>, name: Option<&str>) -> Vec<Installed> {
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
            Some(Installed {
                version: version_of(&path),
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
    let found = installed(&paths, namespace, collection);
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

/// What `verify` compares an installed collection with.
pub struct Verify<'a> {
    pub paths: &'a [PathBuf],
    pub offline: bool,
    pub ignore_errors: bool,
}

/// `collection verify`: each requested collection checked against its own manifest, or against
/// the server's copy of the same version. Exit 1 when one differs.
pub fn verify(
    wanted: &[Collection],
    servers: &mut Servers,
    options: &Verify,
) -> Result<i32, Failure> {
    let mut failed = false;
    for want in wanted {
        let shown = format!(
            "{}:{}",
            want.name.as_deref().unwrap_or_default(),
            want.version
        );
        match verify_one(want, servers, options) {
            Ok(ok) => failed |= !ok,
            Err(err) if options.ignore_errors => {
                eprintln!(
                    "[WARNING]: Failed to verify collection '{shown}' but skipping due to --ignore-errors being set. Error: {}",
                    err.summary()
                );
            }
            Err(err) => return Err(err),
        }
    }
    Ok(i32::from(failed))
}

fn verify_one(want: &Collection, servers: &mut Servers, options: &Verify) -> Result<bool, Failure> {
    if want.kind != Kind::Galaxy {
        let kind = match want.kind {
            Kind::File => "file",
            Kind::Dir => "dir",
            Kind::Git => "git",
            Kind::Url => "url",
            Kind::Subdirs => "subdirs",
            Kind::Galaxy => "galaxy",
        };
        return Err(Failure::error(format!(
            "'{kind}' type is not supported. The format namespace.name is expected."
        )));
    }
    let fqcn = want.name.clone().unwrap_or_default();
    let dir = locate(&fqcn, options.paths)?;
    let local_version = version_of(&dir);
    let remote = if options.offline {
        None
    } else {
        let version = if want.version == "*" {
            local_version.clone()
        } else {
            want.version.clone()
        };
        let (info, at) = record(servers, &fqcn, &version, &want.version)?;
        Some((version, info, at))
    };
    println!("Verifying '{fqcn}:{local_version}'.");
    println!("Installed collection found at '{}'", dir.display());
    if let Some((version, _, _)) = &remote
        && *version != local_version
    {
        println!("{fqcn} has the version '{local_version}' but is being compared to '{version}'");
        return Ok(false);
    }
    let mut modified: Vec<String> = Vec::new();
    let hash = |p: &Path| {
        std::fs::read(p)
            .ok()
            .map(|d| format!("{:x}", Sha256::digest(&d)))
    };
    let (manifest, files, downloads) = match &remote {
        None => {
            println!(
                "MANIFEST.json hash: {}",
                hash(&dir.join("MANIFEST.json")).unwrap_or_default()
            );
            let read = |n: &str| -> Value {
                std::fs::read(dir.join(n))
                    .ok()
                    .and_then(|d| serde_json::from_slice(&d).ok())
                    .unwrap_or(Value::Null)
            };
            let manifest = read("MANIFEST.json");
            let files_name = manifest["file_manifest_file"]["name"]
                .as_str()
                .unwrap_or("FILES.json")
                .to_string();
            (manifest, read(&files_name), None)
        }
        Some((_, info, at)) => {
            let holder = download_dir()?;
            let file = servers.list[*at].download(&info.download_url, &info.sha256, &holder)?;
            let (tarball, _) = Tarball::open(&file.display().to_string())?;
            let manifest_data = tarball.member("MANIFEST.json").unwrap_or_default();
            let remote_hash = format!("{:x}", Sha256::digest(&manifest_data));
            if hash(&dir.join("MANIFEST.json")).as_deref() != Some(remote_hash.as_str()) {
                modified.push("MANIFEST.json".into());
            }
            println!("MANIFEST.json hash: {remote_hash}");
            let manifest: Value = serde_json::from_slice(&manifest_data).unwrap_or(Value::Null);
            let files_name = manifest["file_manifest_file"]["name"]
                .as_str()
                .unwrap_or("FILES.json")
                .to_string();
            let files: Value =
                serde_json::from_slice(&tarball.member(&files_name).unwrap_or_default())
                    .unwrap_or(Value::Null);
            (manifest, files, Some(holder))
        }
    };
    compare(&dir, &manifest, &files, &mut modified);
    if let Some(holder) = downloads.as_ref().and_then(|d| d.parent()) {
        let _ = std::fs::remove_dir_all(holder);
    }
    if modified.is_empty() {
        let what = if remote.is_some() {
            "match the remote collection"
        } else {
            "are internally consistent with its manifest"
        };
        println!("Successfully verified that checksums for '{fqcn}:{local_version}' {what}.");
        Ok(true)
    } else {
        println!("Collection {fqcn} contains modified content in the following files:");
        for name in modified {
            println!("    {name}");
        }
        Ok(false)
    }
}

/// The installed collection's directory: the first search path that has it with a
/// `MANIFEST.json`.
fn locate(fqcn: &str, paths: &[PathBuf]) -> Result<PathBuf, Failure> {
    let (namespace, name) = fqcn.split_once('.').unwrap_or((fqcn, ""));
    let mut missing = format!("Collection {fqcn} is not installed in any of the collection paths.");
    for path in paths {
        let dir = root(path).join(namespace).join(name);
        if !dir.is_dir() {
            continue;
        }
        if !dir.join("MANIFEST.json").is_file() {
            missing = format!(
                "Collection {fqcn} does not have a MANIFEST.json. A MANIFEST.json is expected if the collection has been built and installed via ansible-galaxy"
            );
            continue;
        }
        return Ok(dir);
    }
    Err(Failure::error(missing))
}

/// The record of `version` on the first server that has it, as `MultiGalaxyAPIProxy` asks them.
fn record(
    servers: &mut Servers,
    fqcn: &str,
    version: &str,
    asked: &str,
) -> Result<(super::api::VersionInfo, usize), Failure> {
    let (namespace, name) = fqcn.split_once('.').unwrap_or((fqcn, ""));
    // The reference asks for the signatures first, then for the metadata: the same record twice.
    for server in &mut servers.list {
        if server.version_info(namespace, name, version).is_ok() {
            break;
        }
    }
    let mut last = None;
    for at in 0..servers.list.len() {
        match servers.list[at].version_info(namespace, name, version) {
            Ok(info) => return Ok((info, at)),
            Err(err) => last = Some(err),
        }
    }
    Err(last.unwrap_or_else(|| {
        Failure::error(format!(
            "Failed to find remote collection '{fqcn}:{asked}' on any of the galaxy servers"
        ))
    }))
}

/// Checks `FILES.json` against the manifest, every file it lists against its checksum, and the
/// tree for anything it does not list; adds what differs to `modified`.
fn compare(dir: &Path, manifest: &Value, files: &Value, modified: &mut Vec<String>) {
    let hash = |p: &Path| {
        std::fs::read(p)
            .ok()
            .map(|d| format!("{:x}", Sha256::digest(&d)))
    };
    let file_manifest = &manifest["file_manifest_file"];
    let files_name = file_manifest["name"].as_str().unwrap_or("FILES.json");
    let kind = file_manifest["chksum_type"].as_str().unwrap_or("sha256");
    if hash(&dir.join(files_name))
        != file_manifest[format!("chksum_{kind}")]
            .as_str()
            .map(str::to_string)
    {
        modified.push(files_name.to_string());
    }
    let mut known_files: BTreeSet<PathBuf> = [dir.join("MANIFEST.json"), dir.join("FILES.json")]
        .into_iter()
        .collect();
    let mut known_dirs: BTreeSet<PathBuf> = BTreeSet::new();
    for entry in files["files"].as_array().into_iter().flatten() {
        let entry_name = entry["name"].as_str().unwrap_or_default();
        match entry["ftype"].as_str() {
            Some("file") => {
                known_files.insert(dir.join(entry_name));
                let kind = entry["chksum_type"].as_str().unwrap_or("sha256");
                if hash(&dir.join(entry_name))
                    != entry[format!("chksum_{kind}")].as_str().map(str::to_string)
                {
                    modified.push(entry_name.to_string());
                }
            }
            Some("dir") => {
                known_dirs.insert(dir.join(entry_name));
            }
            _ => {}
        }
    }
    extra(dir, dir, &known_files, &known_dirs, modified);
}

/// Files and directories under `dir` that the manifest does not name, in `os.walk` order:
/// a directory's files, then its subdirectories, then each subdirectory's own. `*.pyc` files
/// are left aside.
fn extra(
    top: &Path,
    dir: &Path,
    files: &BTreeSet<PathBuf>,
    dirs: &BTreeSet<PathBuf>,
    out: &mut Vec<String>,
) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    let rel = |p: &Path| {
        p.strip_prefix(top)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/")
    };
    let (subdirs, plain): (Vec<PathBuf>, Vec<PathBuf>) =
        entries.into_iter().partition(|p| p.is_dir());
    for file in plain {
        if file.extension().is_some_and(|e| e == "pyc") {
            continue;
        }
        if !files.contains(&file) {
            out.push(rel(&file));
        }
    }
    for sub in &subdirs {
        if !dirs.contains(sub) {
            out.push(rel(sub));
        }
    }
    for sub in subdirs {
        extra(top, &sub, files, dirs, out);
    }
}
