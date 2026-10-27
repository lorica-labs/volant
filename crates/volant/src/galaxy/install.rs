// SPDX-License-Identifier: GPL-3.0-or-later
//! `collection install` of tarballs, as ansible-galaxy 2.19.12 runs it (`install_collections`,
//! `install`): what is already there at the same version is left alone, the rest replaces any
//! older tree.

use std::path::{Path, PathBuf};

use super::Failure;
use super::artifact::{Meta, Tarball};

/// The install options that change what is installed.
#[derive(Debug, Default, Clone, Copy)]
pub struct Options {
    pub force: bool,
    pub force_with_deps: bool,
    pub ignore_errors: bool,
    pub upgrade: bool,
}

/// Installs `wanted` under `output`, the `ansible_collections` directory, and returns the exit
/// code.
pub fn collections(
    wanted: &[(Tarball, Meta)],
    output: &Path,
    options: Options,
) -> Result<i32, Failure> {
    println!("Starting galaxy collection install process");
    std::fs::create_dir_all(output).map_err(|e| Failure::error(e.to_string()))?;
    let existing = installed(output);
    let present = |meta: &Meta| {
        !(options.force || options.force_with_deps)
            && existing
                .iter()
                .any(|(fqcn, version)| *fqcn == meta.fqcn() && *version == meta.version)
    };
    if wanted.iter().all(|(_, meta)| present(meta)) && !options.upgrade {
        println!(
            "Nothing to do. All requested collections are already installed. If you want to reinstall them, consider using `--force`."
        );
        return Ok(0);
    }
    println!("Process install dependency map");
    println!("Starting collection install process");
    for (tarball, meta) in wanted {
        let shown = format!("{}:{}", meta.fqcn(), meta.version);
        if present(meta) {
            println!("'{shown}' is already installed, skipping.");
            continue;
        }
        let dest = output.join(&meta.namespace).join(&meta.name);
        println!("Installing '{shown}' to '{}'", dest.display());
        if dest.exists() {
            std::fs::remove_dir_all(&dest).map_err(|e| Failure::error(e.to_string()))?;
        }
        match tarball.install(&dest) {
            Ok(()) => {
                remove_source_metadata(output, meta);
                println!("{shown} was installed successfully");
            }
            Err(err) if options.ignore_errors => eprintln!(
                "[WARNING]: Failed to install collection {shown} but skipping due to --ignore-errors being set. Error: {}",
                err.message
            ),
            Err(err) => return Err(err),
        }
    }
    Ok(0)
}

/// The collections installed under `output`, by `namespace.name`, with the version their
/// `MANIFEST.json` gives.
pub fn installed(output: &Path) -> Vec<(String, String)> {
    let dirs = |dir: &Path| -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        found.sort();
        found
    };
    let mut out = Vec::new();
    for namespace in dirs(output) {
        for collection in dirs(&namespace) {
            let Ok(text) = std::fs::read(collection.join("MANIFEST.json")) else {
                continue;
            };
            let Ok(json) = serde_json::from_slice::<serde_json::Value>(&text) else {
                continue;
            };
            let info = &json["collection_info"];
            if let (Some(ns), Some(name), Some(version)) = (
                info["namespace"].as_str(),
                info["name"].as_str(),
                info["version"].as_str(),
            ) {
                out.push((format!("{ns}.{name}"), version.to_string()));
            }
        }
    }
    out
}

/// Removes the `<namespace>.<name>-*.info` directories an earlier install from a server left.
fn remove_source_metadata(output: &Path, meta: &Meta) {
    let prefix = format!("{}-", meta.fqcn());
    for entry in std::fs::read_dir(output).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && name.ends_with(".info") {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}
