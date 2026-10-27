// SPDX-License-Identifier: GPL-3.0-or-later
//! A collection tarball: its manifest, and its install under a collections path, as
//! ansible-galaxy 2.19.12 does it (`_get_meta_from_tar`, `install_artifact`, `_extract_tar_file`,
//! `_extract_tar_dir`). Only what `FILES.json` lists is written, each file checked against its
//! SHA-256 first.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest as _, Sha256};

use super::Failure;

/// A tarball read into memory, member by member. A name given twice keeps its last member, as
/// `tarfile.getmember` does.
pub struct Tarball {
    /// Absolute, as `tarfile` names it in its messages.
    path: PathBuf,
    members: HashMap<String, Member>,
}

struct Member {
    kind: tar::EntryType,
    #[cfg_attr(not(unix), allow(dead_code))]
    mode: u32,
    data: Vec<u8>,
    link: Option<String>,
}

/// The `collection_info` of a tarball's `MANIFEST.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub namespace: String,
    pub name: String,
    pub version: String,
    /// `namespace.name` and the version range, in the manifest's order.
    pub dependencies: Vec<(String, String)>,
}

impl Meta {
    pub fn fqcn(&self) -> String {
        format!("{}.{}", self.namespace, self.name)
    }
}

impl Tarball {
    /// Opens the tarball `given` names, and reads its manifest. The messages name the path as
    /// given.
    pub fn open(given: &str) -> Result<(Self, Meta), Failure> {
        let path = crate::galaxy::resolve(given);
        if !path.exists() {
            return Err(Failure::error(format!(
                "Unable to find collection artifact file at '{given}'."
            )));
        }
        let members = read(&path).map_err(|_| {
            Failure::error(format!(
                "Collection artifact at '{given}' is not a valid tar file."
            ))
        })?;
        let tarball = Self { path, members };
        let Some(manifest) = tarball.members.get("MANIFEST.json") else {
            return Err(Failure::error(format!(
                "Collection at '{given}' does not contain the required file MANIFEST.json."
            )));
        };
        let json: serde_json::Value = serde_json::from_slice(&manifest.data).map_err(|_| {
            Failure::error(
                "Collection tar file member MANIFEST.json does not contain a valid json string.",
            )
        })?;
        let Some(info) = json.get("collection_info") else {
            return Err(unexpected("'collection_info'"));
        };
        let text = |key: &str| {
            info.get(key)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let dependencies = info
            .get("dependencies")
            .and_then(|d| d.as_object())
            .map(|d| {
                d.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("*").to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let meta = Meta {
            namespace: text("namespace"),
            name: text("name"),
            version: text("version"),
            dependencies,
        };
        Ok((tarball, meta))
    }

    /// The content of the member named `name`.
    pub fn member(&self, name: &str) -> Option<Vec<u8>> {
        self.members.get(name).map(|m| m.data.clone())
    }

    /// The tarball's absolute path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Installs the collection into `dest`, `<path>/ansible_collections/<namespace>/<name>`,
    /// which does not exist. On failure nothing of it is left, nor its namespace directory once
    /// empty.
    pub fn install(&self, dest: &Path) -> Result<(), Failure> {
        let done = self.extract_all(dest);
        if done.is_err() {
            let _ = std::fs::remove_dir_all(dest);
            if let Some(namespace) = dest.parent()
                && std::fs::read_dir(namespace).is_ok_and(|mut e| e.next().is_none())
            {
                let _ = std::fs::remove_dir(namespace);
            }
        }
        done
    }

    fn extract_all(&self, dest: &Path) -> Result<(), Failure> {
        self.extract_file("MANIFEST.json", dest, None)?;
        let Some(files) = self.members.get("FILES.json") else {
            return Err(unexpected("\"filename 'FILES.json' not found\""));
        };
        let files: serde_json::Value =
            serde_json::from_slice(&files.data).map_err(|e| unexpected(&e.to_string()))?;
        self.extract_file("FILES.json", dest, None)?;
        let entries = files
            .get("files")
            .and_then(|f| f.as_array())
            .ok_or_else(|| unexpected("'files'"))?;
        for entry in entries {
            let name = entry
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or_default();
            if name == "." {
                continue;
            }
            if entry.get("ftype").and_then(|t| t.as_str()) == Some("file") {
                let hash = entry.get("chksum_sha256").and_then(|h| h.as_str());
                self.extract_file(name, dest, hash)?;
            } else {
                self.extract_dir(name, dest)?;
            }
        }
        Ok(())
    }

    fn extract_file(&self, name: &str, dest: &Path, expected: Option<&str>) -> Result<(), Failure> {
        let Some(member) = self.members.get(name) else {
            return Err(Failure::error(format!(
                "Collection tar at '{}' does not contain the expected file '{name}'.",
                self.path.display()
            )));
        };
        // `tarfile.extractfile` follows a link to the member it points at, so a link's checksum
        // is its target's.
        let content = if member.kind == tar::EntryType::Symlink {
            let base = Path::new(name).parent().unwrap_or(Path::new(""));
            let target = folded(&base.join(member.link.as_deref().unwrap_or_default()));
            let key = target.to_string_lossy().replace('\\', "/");
            self.members.get(&key).map_or(&member.data, |m| &m.data)
        } else {
            &member.data
        };
        let actual = format!("{:x}", Sha256::digest(content));
        if expected.is_some_and(|e| !e.is_empty() && e != actual) {
            return Err(Failure::error(format!(
                "Checksum mismatch for '{name}' inside collection at '{}'",
                self.path.display()
            )));
        }
        let target = folded(&dest.join(name));
        let parent = target.parent().unwrap_or(dest);
        if !is_child(parent, dest) {
            return Err(Failure::error(format!(
                "Cannot extract tar entry '{name}' as it will be placed outside the collection directory"
            )));
        }
        make_dirs(parent).map_err(io)?;
        if member.kind == tar::EntryType::Symlink {
            return self.link(name, member, &target, dest);
        }
        let partial = target.with_file_name(format!(
            ".{}.volant-partial",
            target.file_name().unwrap_or_default().to_string_lossy()
        ));
        std::fs::write(&partial, &member.data).map_err(io)?;
        std::fs::rename(&partial, &target).map_err(io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = if member.mode & 0o100 == 0 {
                0o644
            } else {
                0o755
            };
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode)).map_err(io)?;
        }
        Ok(())
    }

    fn extract_dir(&self, name: &str, dest: &Path) -> Result<(), Failure> {
        let Some(member) = self.members.get(name) else {
            return Err(Failure::error(format!(
                "Unable to extract '{name}' from collection"
            )));
        };
        let target = dest.join(name);
        if let Some(parent) = target.parent() {
            make_dirs(parent).map_err(io)?;
        }
        if member.kind == tar::EntryType::Symlink {
            return self.link(name, member, &target, dest);
        }
        if !target.is_dir() {
            make_dirs(&target).map_err(io)?;
        }
        Ok(())
    }

    /// A link entry, refused when it resolves outside the collection.
    fn link(&self, name: &str, member: &Member, target: &Path, dest: &Path) -> Result<(), Failure> {
        let link = member.link.clone().unwrap_or_default();
        let resolved = folded(&target.parent().unwrap_or(dest).join(&link));
        if !is_child(&resolved, dest) {
            return Err(Failure::error(format!(
                "Cannot extract symlink '{name}' in collection: path points to location outside of collection 'b'{link}''"
            )));
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&link, target).map_err(io)?;
        #[cfg(not(unix))]
        std::fs::copy(&resolved, target).map_err(io)?;
        Ok(())
    }
}

/// Every member of a gzipped or plain tar file.
fn read(path: &Path) -> std::io::Result<HashMap<String, Member>> {
    let raw = std::fs::read(path)?;
    let data = if raw.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&raw[..]).read_to_end(&mut out)?;
        out
    } else {
        raw
    };
    let mut archive = tar::Archive::new(&data[..]);
    let mut members = HashMap::new();
    let mut any = false;
    for entry in archive.entries()? {
        let mut entry = entry?;
        any = true;
        let name = entry
            .path()?
            .to_string_lossy()
            .trim_end_matches('/')
            .to_string();
        let kind = entry.header().entry_type();
        let mode = entry.header().mode().unwrap_or(0o644);
        let link = entry.link_name()?.map(|l| l.to_string_lossy().into_owned());
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        members.insert(
            name,
            Member {
                kind,
                mode,
                data: bytes,
                link,
            },
        );
    }
    if any {
        Ok(members)
    } else {
        Err(std::io::Error::other("no tar member"))
    }
}

/// `path` with `.` and `..` folded out of its text, as `os.path.abspath` leaves it.
fn folded(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// `_is_child_path`: `path` is `parent` or lies under it.
fn is_child(path: &Path, parent: &Path) -> bool {
    path == parent || path.starts_with(parent)
}

/// Creates `dir` and its parents. The ones inside a collection get `0755`, as the reference's
/// `os.makedirs(..., mode=0o755)` gives them.
fn make_dirs(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o755);
    }
    builder.create(dir)
}

fn io(error: std::io::Error) -> Failure {
    Failure::error(error.to_string())
}

/// An exception the reference does not handle, reported as it reports one: exit 250.
fn unexpected(what: &str) -> Failure {
    Failure::unexpected(what)
}
