// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading an entry back and writing one.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{Map, Value, json};

use super::sweep::{check_dir, sweep, write_new};
use super::{CacheKey, Entry, FORMAT, Source};
use crate::agent::embedded::create_private;
use crate::agent::{BlobFrame, keep_frame};
use crate::python::Union;

/// The entry under `key`, or `None` on any doubt: a directory others could write, a manifest that
/// does not read or is of another format, a source whose size or mtime moved, or a zip whose
/// hash is not the manifest's.
#[cfg(test)]
pub fn load(dir: &Path, key: &CacheKey) -> Option<Entry> {
    open(dir, key).ok().flatten()
}

/// `load`, telling a directory or a file that cannot be trusted (`Err`, which the run warns
/// about) from an entry that is merely absent or stale (`Ok(None)`).
pub fn open(dir: &Path, key: &CacheKey) -> io::Result<Option<Entry>> {
    match check_dir(dir) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        checked => checked?,
    }
    let Some(manifest) = read_own(&dir.join(format!("{}.json", key.0)))? else {
        return Ok(None);
    };
    let Some(zip) = read_own(&dir.join(format!("{}.zip", key.0)))? else {
        return Ok(None);
    };
    let deflate_path = format!("{}.deflate", key.0);
    // A deflated form that is missing, unreadable or not the one the manifest names is made
    // again from the zip, which was just checked, and written back: never sent as it was.
    let deflated = read_own(&dir.join(&deflate_path)).ok().flatten();
    let Some((entry, kept)) = parse(&manifest, &zip, deflated) else {
        return Ok(None);
    };
    let deflated = kept.unwrap_or_else(|| {
        let deflated = volant_protocol::encoding::deflate(&zip);
        let _ = write_new(dir, &deflate_path, &deflated);
        deflated
    });
    keep_frame(&entry.union.hash, BlobFrame::smaller(zip, deflated));
    Ok(Some(entry))
}

/// The bytes of `path`, or `None` when there is no such file, after checking the file actually
/// opened: this user's, and writable by nobody else. The directory was checked by its path, and
/// an account able to rename it (one owning a directory above it) can swap in its own between
/// that check and this read; the files it brings are its own, and fail here.
fn read_own(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        // SAFETY: `geteuid` reads the calling process's own credentials and cannot fail.
        let euid = unsafe { libc::geteuid() };
        let mode = meta.mode() & 0o7777;
        if !meta.is_file() || meta.uid() != euid || mode & 0o022 != 0 {
            return Err(io::Error::other(format!(
                "{} is owned by uid {} with mode {mode:o}, and this controller runs as uid                  {euid}; it has to be this user's, and writable by nobody else",
                path.display(),
                meta.uid()
            )));
        }
    }
    let mut bytes = Vec::new();
    io::Read::read_to_end(&mut file, &mut bytes)?;
    Ok(Some(bytes))
}

/// An entry from its manifest and zip, or `None` on any doubt, with `deflated` when it is the
/// deflated form the manifest names.
fn parse(
    manifest: &[u8],
    zip: &[u8],
    deflated: Option<Vec<u8>>,
) -> Option<(Entry, Option<Vec<u8>>)> {
    let manifest: Value = serde_json::from_slice(manifest).ok()?;
    if manifest.get("format")?.as_u64()? != FORMAT {
        return None;
    }
    let sources = sources_from(manifest.get("sources")?)?;
    if sources
        .iter()
        .any(|source| Source::now(&source.path).ok().as_ref() != Some(source))
    {
        return None;
    }
    let hash = manifest.get("hash")?.as_str()?;
    if blake3::hash(zip).to_hex().as_str() != hash {
        return None;
    }
    let deflated_hash = manifest.get("deflated")?.as_str()?;
    let deflated = deflated.filter(|bytes| blake3::hash(bytes).to_hex().as_str() == deflated_hash);
    let mut modules = BTreeMap::new();
    for (name, facts) in manifest.get("modules")?.as_object()? {
        modules.insert(name.clone(), crate::python::module_facts(name, facts).ok()?);
    }
    let mut refused = BTreeMap::new();
    for (name, why) in manifest.get("refused")?.as_object()? {
        refused.insert(name.clone(), why.as_str()?.to_string());
    }
    let mut resolved = BTreeMap::new();
    for (name, answer) in manifest.get("resolved")?.as_object()? {
        resolved.insert(
            name.clone(),
            crate::python::resolved_from(name, answer).ok()?,
        );
    }
    let entry = Entry {
        union: Union {
            hash: hash.to_string(),
            zip_b64: volant_protocol::encoding::b64_encode(zip),
            modules,
            refused,
            natives: crate::python::Natives::default(),
        },
        sources,
        resolved,
    };
    Some((entry, deflated))
}

/// The sources as the helper reports them and the manifest keeps them, or `None` for anything
/// else, `null` included: a union whose sources are not all named is never kept.
pub fn sources_from(value: &Value) -> Option<Vec<Source>> {
    value
        .as_array()?
        .iter()
        .map(|source| {
            Some(Source {
                path: PathBuf::from(source.get("path")?.as_str()?),
                len: source.get("len")?.as_u64()?,
                mtime_ns: i128::from(source.get("mtime_ns")?.as_i64()?),
            })
        })
        .collect()
}

/// Writes `entry` under `key`, each file under a temporary name renamed into place, mode 0600,
/// in a directory created 0700. Two runs storing at once both leave a whole entry: the zip goes
/// first, and a manifest paired with the other run's zip fails the hash check and is rebuilt.
/// The zip is deflated here, at level 6, once for every host of this run and of the runs that
/// load the entry.
pub fn store(dir: &Path, key: &CacheKey, entry: &Entry) -> io::Result<()> {
    create_private(dir)?;
    check_dir(dir)?;
    let zip =
        volant_protocol::encoding::b64_decode(&entry.union.zip_b64).map_err(io::Error::other)?;
    let modules: Map<String, Value> = entry
        .union
        .modules
        .iter()
        .map(|(name, facts)| {
            let facts = json!({
                "module_fqn": facts.module_fqn,
                "profile": facts.profile,
                "rlimit_nofile": facts.rlimit_nofile,
                "extensions": facts.extensions,
                "core": facts.core,
            });
            (name.clone(), facts)
        })
        .collect();
    let sources: Vec<Value> = entry
        .sources
        .iter()
        .map(|source| {
            let mtime_ns = i64::try_from(source.mtime_ns).map_err(io::Error::other)?;
            let path = source.path.to_str().ok_or_else(|| {
                io::Error::other(format!("{} is not UTF-8", source.path.display()))
            })?;
            Ok(json!({ "path": path, "len": source.len, "mtime_ns": mtime_ns }))
        })
        .collect::<io::Result<_>>()?;
    let resolved: Map<String, Value> = entry
        .resolved
        .iter()
        .map(|(name, answer)| (name.clone(), crate::python::resolved_json(answer)))
        .collect();
    let deflated = volant_protocol::encoding::deflate(&zip);
    let manifest = json!({
        "format": FORMAT,
        "hash": entry.union.hash,
        "deflated": blake3::hash(&deflated).to_hex().as_str(),
        "modules": modules,
        "refused": entry.union.refused,
        "resolved": resolved,
        "sources": sources,
    });
    write_new(dir, &format!("{}.zip", key.0), &zip)?;
    write_new(dir, &format!("{}.deflate", key.0), &deflated)?;
    write_new(
        dir,
        &format!("{}.json", key.0),
        manifest.to_string().as_bytes(),
    )?;
    keep_frame(&entry.union.hash, BlobFrame::smaller(zip, deflated));
    sweep(dir, SystemTime::now());
    Ok(())
}
