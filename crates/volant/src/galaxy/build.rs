// SPDX-License-Identifier: GPL-3.0-or-later
//! A collection source tree: its `galaxy.yml`, the files a build takes, and the two things made
//! from it, as ansible-galaxy 2.19.12 makes them (`_get_meta_from_src_dir`,
//! `_normalize_galaxy_yml_manifest`, `_build_files_manifest_walk`, `_build_manifest`,
//! `_build_collection_tar`, `_build_collection_dir`): a tarball, or an installed tree.
//!
//! Volant's tarball sets every time to 0 and every owner to root, so the same tree builds to the
//! same bytes; the reference stamps the manifests with the current time.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use super::Failure;

mod files;

use files::{copy_link, executable, inner_link, make_dir, python_json, set_mode, walk};

/// `galaxy.yml`'s keys by type (`collections_galaxy_meta.yml`); the first five are required.
const REQUIRED: [&str; 5] = ["authors", "name", "namespace", "readme", "version"];
const STRINGS: [&str; 10] = [
    "namespace",
    "name",
    "version",
    "readme",
    "description",
    "license_file",
    "repository",
    "documentation",
    "homepage",
    "issues",
];
const LISTS: [&str; 4] = ["authors", "license", "tags", "build_ignore"];

/// What the walk leaves out, besides `build_ignore`.
const IGNORED: [&str; 8] = [
    "MANIFEST.json",
    "FILES.json",
    "galaxy.yml",
    "galaxy.yaml",
    ".git",
    "*.pyc",
    "*.retry",
    "tests/output",
];
pub(super) const IGNORED_DIRS: [&str; 7] =
    ["CVS", ".bzr", ".hg", ".git", ".svn", "__pycache__", ".tox"];

/// A source tree with a valid `galaxy.yml`.
pub struct Source {
    /// Absolute.
    path: PathBuf,
    info: Map<String, Value>,
}

/// One entry of `FILES.json`.
pub(super) struct Entry {
    pub(super) name: String,
    pub(super) dir: bool,
    pub(super) sha256: Option<String>,
}

impl Source {
    /// Reads `<dir>/galaxy.yml`, refusing it as the reference does.
    pub fn read(dir: &Path) -> Result<Self, Failure> {
        let file = dir.join("galaxy.yml");
        let shown = file.display();
        if !file.is_file() {
            return Err(Failure::error(format!(
                "The collection galaxy.yml path '{shown}' does not exist."
            )));
        }
        let text = std::fs::read_to_string(&file).map_err(|e| Failure::error(e.to_string()))?;
        let parsed = crate::yaml::load(&text, &shown.to_string())
            .and_then(|docs| docs.first().map_or(Ok(Value::Null), crate::yaml::to_json))
            .map_err(|e| {
                Failure::error(format!(
                    "Failed to parse the galaxy.yml at '{shown}' with the following error:\n{e}"
                ))
            })?;
        let Value::Object(mut info) = parsed else {
            return Err(Failure::error(format!(
                "The collection galaxy.yml at '{shown}' is incorrectly formatted."
            )));
        };
        let missing: Vec<&str> = REQUIRED
            .iter()
            .copied()
            .filter(|k| !info.contains_key(*k))
            .collect();
        if !missing.is_empty() {
            return Err(Failure::error(format!(
                "The collection galaxy.yml at '{shown}' is missing the following mandatory keys: {}",
                missing.join(", ")
            )));
        }
        let unknown: Vec<&str> = info
            .keys()
            .map(String::as_str)
            .filter(|k| {
                !STRINGS.contains(k)
                    && !LISTS.contains(k)
                    && *k != "dependencies"
                    && *k != "manifest"
            })
            .collect();
        if !unknown.is_empty() {
            eprintln!(
                "[WARNING]: Found unknown keys in collection galaxy.yml at '{shown}': {}",
                unknown.join(", ")
            );
        }
        for key in STRINGS {
            info.entry(key).or_insert(Value::Null);
        }
        for key in LISTS {
            let value = info.entry(key).or_insert(Value::Null);
            *value = match value.take() {
                Value::Null => json!([]),
                Value::Array(list) => Value::Array(list),
                other => json!([other]),
            };
        }
        info.entry("dependencies").or_insert_with(|| json!({}));
        if info["version"].is_null() || info["version"] == json!("") {
            info["version"] = json!("*");
        }
        if info.contains_key("manifest") {
            if !info["build_ignore"].as_array().is_none_or(Vec::is_empty) {
                return Err(Failure::error(
                    "\"build_ignore\" and \"manifest\" are mutually exclusive",
                ));
            }
            return Err(Failure::error(
                "volant galaxy cannot build a collection whose galaxy.yml has a manifest key yet",
            ));
        }
        Ok(Self {
            path: dir.to_path_buf(),
            info,
        })
    }

    fn text(&self, key: &str) -> String {
        match &self.info[key] {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        }
    }

    pub fn namespace(&self) -> String {
        self.text("namespace")
    }

    pub fn name(&self) -> String {
        self.text("name")
    }

    pub fn version(&self) -> String {
        self.text("version")
    }

    pub fn fqcn(&self) -> String {
        format!("{}.{}", self.namespace(), self.name())
    }

    /// `dependencies`: `namespace.name` and a range.
    pub fn dependencies(&self) -> Vec<(String, String)> {
        self.info["dependencies"]
            .as_object()
            .map(|d| {
                d.iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            v.as_str().map_or_else(|| v.to_string(), str::to_string),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The entries of `FILES.json`: `.`, then the tree walked depth first, each directory's
    /// entries in name order, without what is ignored.
    fn files(&self, verbose: u8) -> Result<Vec<Entry>, Failure> {
        let mut patterns: Vec<String> = IGNORED.iter().map(|p| (*p).to_string()).collect();
        patterns.push(format!("{}-{}-*.tar.gz", self.namespace(), self.name()));
        patterns.extend(
            self.info["build_ignore"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_str().map(str::to_string)),
        );
        let mut out = vec![Entry {
            name: ".".into(),
            dir: true,
            sha256: None,
        }];
        walk(&self.path, &self.path, "", &patterns, verbose, &mut out)?;
        Ok(out)
    }

    /// `MANIFEST.json` and `FILES.json`, as `json.dumps(..., indent=True)` writes them.
    fn manifests(&self, files: &[Entry]) -> (Vec<u8>, Vec<u8>) {
        let entries: Vec<Value> = files
            .iter()
            .map(|e| {
                json!({
                    "name": e.name,
                    "ftype": if e.dir { "dir" } else { "file" },
                    "chksum_type": if e.dir { Value::Null } else { json!("sha256") },
                    "chksum_sha256": e.sha256,
                    "format": 1,
                })
            })
            .collect();
        let files_json = python_json(&json!({"files": entries, "format": 1}));
        let key = |k: &str| self.info[k].clone();
        let license_file = match key("license_file") {
            Value::String(s) if s.is_empty() => Value::Null,
            other => other,
        };
        let manifest = json!({
            "collection_info": {
                "namespace": key("namespace"),
                "name": key("name"),
                "version": key("version"),
                "authors": key("authors"),
                "readme": key("readme"),
                "tags": key("tags"),
                "description": key("description"),
                "license": key("license"),
                "license_file": license_file,
                "dependencies": key("dependencies"),
                "repository": key("repository"),
                "documentation": key("documentation"),
                "homepage": key("homepage"),
                "issues": key("issues"),
            },
            "file_manifest_file": {
                "name": "FILES.json",
                "ftype": "file",
                "chksum_type": "sha256",
                "chksum_sha256": format!("{:x}", Sha256::digest(&files_json)),
                "format": 1,
            },
            "format": 1,
        });
        (python_json(&manifest), files_json)
    }

    /// Writes `<namespace>-<name>-<version>.tar.gz` under `output` and returns its path
    /// (`build_collection`).
    pub fn build(&self, output: &Path, force: bool, verbose: u8) -> Result<PathBuf, Failure> {
        let files = self.files(verbose)?;
        let target = output.join(format!(
            "{}-{}-{}.tar.gz",
            self.namespace(),
            self.name(),
            self.version()
        ));
        if target.is_dir() {
            return Err(Failure::error(format!(
                "The output collection artifact '{}' already exists, but is a directory - aborting",
                target.display()
            )));
        }
        if target.exists() && !force {
            return Err(Failure::error(format!(
                "The file '{}' already exists. You can use --force to re-create the collection artifact.",
                target.display()
            )));
        }
        let (manifest, files_json) = self.manifests(&files);
        let io = |e: std::io::Error| Failure::error(e.to_string());
        let partial = output.join(format!(
            ".{}.volant-partial",
            target.file_name().unwrap_or_default().to_string_lossy()
        ));
        let file = std::fs::File::create(&partial).map_err(io)?;
        let gz = flate2::GzBuilder::new()
            .mtime(0)
            .write(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        let mut add =
            |name: &str, mode: u32, kind: tar::EntryType, data: &[u8], link: Option<&Path>| {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(kind);
                header.set_mode(mode);
                header.set_mtime(0);
                header.set_uid(0);
                header.set_gid(0);
                header.set_size(data.len() as u64);
                match link {
                    Some(target) => tar.append_link(&mut header, name, target),
                    None => tar.append_data(&mut header, name, data),
                }
            };
        add(
            "MANIFEST.json",
            0o644,
            tar::EntryType::Regular,
            &manifest,
            None,
        )
        .map_err(io)?;
        add(
            "FILES.json",
            0o644,
            tar::EntryType::Regular,
            &files_json,
            None,
        )
        .map_err(io)?;
        for entry in files.iter().skip(1) {
            let source = self.path.join(&entry.name);
            if let Some(link) = inner_link(&source, &self.path)? {
                add(
                    &entry.name,
                    0o777,
                    tar::EntryType::Symlink,
                    &[],
                    Some(&link),
                )
                .map_err(io)?;
            } else if entry.dir {
                add(&entry.name, 0o755, tar::EntryType::Directory, &[], None).map_err(io)?;
            } else {
                let data = std::fs::read(&source).map_err(io)?;
                let mode = if executable(&source) { 0o755 } else { 0o644 };
                add(&entry.name, mode, tar::EntryType::Regular, &data, None).map_err(io)?;
            }
        }
        tar.into_inner()
            .and_then(flate2::write::GzEncoder::finish)
            .and_then(|mut f| f.flush())
            .map_err(io)?;
        std::fs::rename(&partial, &target).map_err(io)?;
        println!(
            "Created collection for {} at {}",
            self.fqcn(),
            target.display()
        );
        Ok(target)
    }

    /// Installs the tree into `dest`, which does not exist, as the reference copies a source
    /// directory (`install_src`, `_build_collection_dir`).
    pub fn install(&self, dest: &Path, verbose: u8) -> Result<(), Failure> {
        let files = self.files(verbose)?;
        let (manifest, files_json) = self.manifests(&files);
        let io = |e: std::io::Error| Failure::error(e.to_string());
        make_dir(dest).map_err(io)?;
        for (name, data) in [("MANIFEST.json", &manifest), ("FILES.json", &files_json)] {
            std::fs::write(dest.join(name), data).map_err(io)?;
            set_mode(&dest.join(name), 0o644).map_err(io)?;
        }
        let mut sorted: Vec<&Entry> = files.iter().skip(1).collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        for entry in sorted {
            let source = self.path.join(&entry.name);
            let target = dest.join(&entry.name);
            let is_link =
                std::fs::symlink_metadata(&source).is_ok_and(|m| m.file_type().is_symlink());
            if entry.dir && !is_link {
                make_dir(&target).map_err(io)?;
            } else if is_link {
                copy_link(&source, &target).map_err(io)?;
            } else {
                std::fs::copy(&source, &target).map_err(io)?;
                set_mode(&target, if executable(&source) { 0o755 } else { 0o644 }).map_err(io)?;
            }
        }
        println!(
            "Created collection for {}:{} at {}",
            self.fqcn(),
            self.version(),
            dest.display()
        );
        Ok(())
    }
}
