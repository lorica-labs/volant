// SPDX-License-Identifier: GPL-3.0-or-later
//! `collection verify` (`verify_collections`): installed collections checked against their
//! manifests or the servers' copies.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::{root, version_of};
use crate::galaxy::Failure;
use crate::galaxy::api::servers::Servers;
use crate::galaxy::artifact::Tarball;
use crate::galaxy::install::download_dir;
use crate::galaxy::requirements::{Collection, Kind};

/// What `verify` compares an installed collection with.
pub struct Verify<'a> {
    pub paths: &'a [PathBuf],
    pub offline: bool,
    pub ignore_errors: bool,
    pub verbose: u8,
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

/// `verify` takes names only.
fn galaxy_only(want: &Collection) -> Result<(), Failure> {
    let kind = match want.kind {
        Kind::Galaxy => return Ok(()),
        Kind::File => "file",
        Kind::Dir => "dir",
        Kind::Git => "git",
        Kind::Url => "url",
        Kind::Subdirs => "subdirs",
    };
    Err(Failure::error(format!(
        "'{kind}' type is not supported. The format namespace.name is expected."
    )))
}

fn verify_one(want: &Collection, servers: &mut Servers, options: &Verify) -> Result<bool, Failure> {
    galaxy_only(want)?;
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
        Some((version, info, at)) => {
            let holder = download_dir()?;
            let file = servers.list[*at].download(
                &info.download_url,
                &info.sha256,
                &holder,
                &format!("{fqcn}:{version}"),
            )?;
            if options.verbose >= 3 {
                println!("Remote collection cached as '{}'", file.display());
            }
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
) -> Result<(crate::galaxy::api::VersionInfo, usize), Failure> {
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
