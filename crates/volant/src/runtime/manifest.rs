// SPDX-License-Identifier: GPL-3.0-or-later
//! The pinned runtime: `manifest.toml`, written by `just runtime-lock`. For each platform it names
//! one python-build-standalone archive and the ansible-core wheels, each with its URL and SHA-256.

use std::collections::BTreeMap;

use toml_edit::{DocumentMut, Item, Table};

use super::INSTEAD;

/// The manifest built into this controller.
const PINNED: &str = include_str!("manifest.toml");

/// The versions a runtime is made of, and what each platform downloads to get them.
#[derive(Debug)]
pub(crate) struct Manifest {
    /// The CPython version, `3.13.16`.
    pub(crate) python: String,
    /// The python-build-standalone release the builds come from, `20261013`.
    pub(crate) python_release: String,
    /// The ansible-core version every platform installs.
    pub(crate) ansible_core: String,
    platforms: BTreeMap<String, Platform>,
    /// The SHA-256 of the manifest's text.
    digest: String,
}

/// What one triple downloads.
#[derive(Debug)]
pub(crate) struct Platform {
    /// The `install_only` archive, which unpacks to `python/`.
    pub(crate) python: Artifact,
    /// ansible-core and every dependency, as wheels built for this platform.
    pub(crate) wheels: Vec<Wheel>,
}

/// A file to download and the SHA-256 it must have, 64 lowercase hex digits.
#[derive(Debug)]
pub(crate) struct Artifact {
    pub(crate) url: String,
    pub(crate) sha256: String,
}

impl Artifact {
    /// The file name the URL ends with, which pip needs to read a wheel's tags. Refused when it
    /// is empty or could name another directory once joined to a local path.
    pub(crate) fn filename(&self) -> Result<&str, String> {
        let name = self.url.rsplit('/').next().unwrap_or_default();
        if name.is_empty() || name == "." || name.contains("..") || name.contains(['\\', '?', '#'])
        {
            return Err(format!("{} does not end with a file name", self.url));
        }
        Ok(name)
    }
}

#[derive(Debug)]
pub(crate) struct Wheel {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) file: Artifact,
}

impl Manifest {
    /// The manifest built into this controller.
    pub(crate) fn pinned() -> Result<Self, String> {
        Self::parse(PINNED)
    }

    /// Reads a manifest, refusing one with a missing key, a URL that is not https or a hash
    /// that is not a SHA-256.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        read(text).map_err(|e| format!("the runtime manifest is invalid: {e}"))
    }

    /// The name of the runtime installed from this manifest: `cpython-<version>-ansible-core-`
    /// `<version>-` and the first 8 hex digits of the manifest's SHA-256, so any change to the
    /// pins names another runtime.
    pub(crate) fn id(&self) -> String {
        format!(
            "cpython-{}-ansible-core-{}-{}",
            self.python,
            self.ansible_core,
            &self.digest[..8]
        )
    }

    /// What `triple` downloads, or a refusal naming it when nothing is pinned for it.
    pub(crate) fn platform(&self, triple: &str) -> Result<&Platform, String> {
        self.platforms
            .get(triple)
            .ok_or_else(|| format!("no managed Python is pinned for {triple}: {INSTEAD}"))
    }
}

fn read(text: &str) -> Result<Manifest, String> {
    let doc: DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| e.to_string())?;
    let root = doc.as_table();
    let ansible_core = string(root, "", "ansible-core")?;
    let mut platforms = BTreeMap::new();
    for (triple, item) in table(root, "", "platform")? {
        let at = format!("platform.{triple}.");
        let entry = item
            .as_table()
            .ok_or_else(|| format!("platform.{triple} is not a table"))?;
        let python = artifact(table(entry, &at, "python")?, &format!("{at}python."))?;
        let wheels = entry
            .get("wheel")
            .and_then(Item::as_array_of_tables)
            .ok_or_else(|| format!("{at}wheel is missing or not an array of tables"))?
            .iter()
            .enumerate()
            .map(|(i, wheel)| {
                let at = format!("{at}wheel[{i}].");
                Ok(Wheel {
                    name: string(wheel, &at, "name")?,
                    version: string(wheel, &at, "version")?,
                    file: artifact(wheel, &at)?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        if !wheels
            .iter()
            .any(|wheel| wheel.name == "ansible-core" && wheel.version == ansible_core)
        {
            return Err(format!("{at}wheel has no ansible-core {ansible_core}"));
        }
        platforms.insert(triple.to_owned(), Platform { python, wheels });
    }
    Ok(Manifest {
        python: string(root, "", "python")?,
        python_release: string(root, "", "python-release")?,
        ansible_core,
        platforms,
        digest: format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(text)),
    })
}

fn table<'a>(parent: &'a Table, at: &str, key: &str) -> Result<&'a Table, String> {
    parent
        .get(key)
        .and_then(Item::as_table)
        .ok_or_else(|| format!("{at}{key} is missing or not a table"))
}

fn string(parent: &Table, at: &str, key: &str) -> Result<String, String> {
    parent
        .get(key)
        .and_then(Item::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("{at}{key} is missing or not a string"))
}

fn artifact(parent: &Table, at: &str) -> Result<Artifact, String> {
    let url = string(parent, at, "url")?;
    if !url.starts_with("https://") {
        return Err(format!("{at}url is not an https URL: {url}"));
    }
    let sha256 = string(parent, at, "sha256")?;
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(format!(
            "{at}sha256 is not 64 lowercase hex digits: {sha256}"
        ));
    }
    let artifact = Artifact { url, sha256 };
    artifact.filename().map_err(|e| format!("{at}url: {e}"))?;
    Ok(artifact)
}
