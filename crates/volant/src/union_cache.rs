// SPDX-License-Identifier: GPL-3.0-or-later
//! The run's Python module union, kept on the controller between runs.
//!
//! Building the union costs a Python start, an ansible-core import and a cold build of every
//! module: 3.8 s of a 12.7 s converged run on the roles bench, measured. Checking that the files it
//! was built from are unchanged costs about 1 ms for the 154 of them. So a run first looks for an
//! entry under a key naming everything the build read that is not a file (this controller, the
//! interpreter, the modules asked for, the `ANSIBLE_*` and `PYTHON*` environment, the
//! `ansible.cfg` ansible-core would read, the working directory), then checks each file the helper
//! reported reading still has its size and mtime, and the zip still has its hash. Anything short
//! of that rebuilds, and nothing about the cache ever fails a run.
//!
//! An entry is only as trustworthy as the directory holding it, which is private to this user
//! (0700, owner checked): the same perimeter as `~/.ansible`, whose collections and modules a
//! build reads anyway.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::agent::embedded::create_private;
use crate::agent::{BlobFrame, keep_frame};
use crate::python::{Resolved, Union};

/// The manifest's own version. An entry written in another format is rebuilt, never read.
///
/// 2: each module's facts carry `core`. A format 1 entry has none, and read as `false` it would
/// keep every native module off for as long as the entry stays valid.
///
/// 3: an entry keeps the zip's deflated form beside it, `<key>.deflate`, named by its blake3 in
/// the manifest. A format 2 entry has none.
const FORMAT: u64 = 3;

/// An entry nothing has rewritten for this long is removed by the next store: a module set a
/// playbook no longer names would otherwise stay on disk for good.
const STALE: Duration = Duration::from_secs(30 * 24 * 3600);

/// Lowercase hex blake3 of everything an entry depends on that is not a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey(String);

/// One file a build read, as it was when it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub path: PathBuf,
    pub len: u64,
    pub mtime_ns: i128,
}

impl Source {
    /// `path` as it is now, following a symbolic link as the helper's `os.stat` does.
    pub fn now(path: &Path) -> io::Result<Source> {
        let meta = fs::metadata(path)?;
        let mtime_ns = match meta.modified()?.duration_since(UNIX_EPOCH) {
            Ok(after) => i128::try_from(after.as_nanos()).unwrap_or(i128::MAX),
            Err(before) => -i128::try_from(before.duration().as_nanos()).unwrap_or(i128::MAX),
        };
        Ok(Source {
            path: path.to_path_buf(),
            len: meta.len(),
            mtime_ns,
        })
    }
}

/// What a run needs of a union without starting the helper: the union itself, the files it was
/// built from, and the answers the collections' names got, which the pre-flight checks the tasks
/// of this run against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub union: Union,
    pub sources: Vec<Source>,
    pub resolved: BTreeMap<String, Resolved>,
}

/// The interpreter a build would run under, found without running anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpreterId {
    /// The candidate as found: a virtualenv's `python` is a link to the system one, and the two
    /// import different ansible-cores.
    pub path: PathBuf,
    /// Where the links lead: a Nix profile's `python3` keeps its path and its wrapper's size
    /// across generations, and only the store path it resolves to moves.
    pub real: PathBuf,
    /// The size and mtime of the binary the link leads to, which an upgrade changes.
    pub len: u64,
    pub mtime_ns: i128,
}

/// Where a run looks for its union, and under which interpreter the entry must have been built:
/// the candidate `find_python` would start (`interpreter`), which has to be the executable the
/// helper reports running (`real`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub dir: PathBuf,
    pub key: CacheKey,
    pub interpreter: PathBuf,
    pub real: PathBuf,
}

/// What the helper says a union was built from: the interpreter that ran, as
/// `os.path.realpath(sys.executable)`, and every file it read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Traced {
    pub interpreter: PathBuf,
    pub sources: Vec<Source>,
}

impl Place {
    /// This run's place, or `None` when there is no cache directory or no interpreter to build
    /// under: the run then builds as it always did.
    pub fn here(
        modules: &BTreeSet<String>,
        asked: &[String],
        plugin_dirs: &[PathBuf],
    ) -> Option<Place> {
        let explicit = std::env::var("VOLANT_PYTHON").ok();
        let virtual_env = std::env::var("VIRTUAL_ENV").ok();
        let interpreter = interpreter_without_running(explicit.as_deref(), virtual_env.as_deref())?;
        Some(Place {
            dir: cache_dir()?,
            key: key(&interpreter, modules, asked, plugin_dirs)?,
            interpreter: interpreter.path,
            real: interpreter.real,
        })
    }
}

/// `$XDG_CACHE_HOME/volant/unions`, else `~/.cache/volant/unions`.
pub fn cache_dir() -> Option<PathBuf> {
    crate::agent::embedded::cache_root().map(|root| root.join("unions"))
}

/// The first candidate `find_python` would try that exists, without asking it whether it has
/// ansible-core. When it has not, `find_python` moves on to the next one, and the union built
/// there is not stored (see `python::union_from`): an entry is only ever filed under the
/// interpreter that built it.
///
/// `None` for a script (`#!`): a pyenv or asdf shim is the same file whatever version it runs,
/// chosen by a variable or a file this cannot see, so no key could name the interpreter behind
/// it and nothing is cached. A shim that is a link to a binary picking the version itself (mise's
/// are links to `mise`) passes here; `python::union_from` stores nothing unless the helper's own
/// `realpath(sys.executable)` is this `real`, which such a link never resolves to.
pub fn interpreter_without_running(
    explicit: Option<&str>,
    virtual_env: Option<&str>,
) -> Option<InterpreterId> {
    let path = crate::python::candidates(explicit, virtual_env)
        .iter()
        .find_map(|candidate| located(candidate))?;
    let mut head = [0u8; 2];
    let mut file = fs::File::open(&path).ok()?;
    if io::Read::read_exact(&mut file, &mut head).is_err() || head == *b"#!" {
        return None;
    }
    let real = fs::canonicalize(&path).ok()?;
    let found = Source::now(&real).ok()?;
    Some(InterpreterId {
        path,
        real,
        len: found.len,
        mtime_ns: found.mtime_ns,
    })
}

/// The file a candidate names: itself when it holds a `/`, as `Command` reads it, else the first
/// of that name on `PATH`.
pub fn located(candidate: &str) -> Option<PathBuf> {
    if candidate.contains('/') {
        let path = std::path::absolute(candidate).ok()?;
        return path.is_file().then_some(path);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(candidate))
        .find(|path| path.is_file())
}

/// The key of this run's union, read from the process: its environment, its working directory,
/// every `ansible.cfg` ansible-core might read (`configs`), and the files the playbooks' and
/// roles' plugin directories hold (`plugin_files`). `None` when one of those files is too recent
/// to vouch for: the run then builds and keeps nothing, as it does without a cache directory.
pub fn key(
    interpreter: &InterpreterId,
    modules: &BTreeSet<String>,
    asked: &[String],
    plugin_dirs: &[PathBuf],
) -> Option<CacheKey> {
    let env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    let mut files = configs(
        std::env::var_os("ANSIBLE_CONFIG").as_deref(),
        std::env::var_os("HOME").as_deref().map(Path::new),
    );
    files.extend(plugin_files(plugin_dirs, SystemTime::now())?);
    let cwd = std::env::current_dir().unwrap_or_default();
    Some(key_from(interpreter, modules, asked, &env, &files, &cwd))
}

/// Each directory of `dirs`, then every file under its `library/` and `module_utils/`: which
/// module a name finds, and what its `module_utils` import reads, is decided there. The sources
/// an entry keeps cannot see it: a `library/` created after the entry was written has no mtime in
/// them, and a module shadowing one of ansible-core's would go unseen.
///
/// A file is stamped, not read: the real path it leads to, its size and its mtime, the same
/// check a kept source gets, so a warm run over a large `module_utils/` tree stats it rather
/// than reading every byte. A link to a directory is followed, and a directory reached a second
/// time is not walked again, which ends a loop of links.
///
/// `None` when a file changed less than two seconds before `now`, the helper's racy-timestamp
/// rule (`sources`): rewritten within the same tick of a coarse clock, it would keep its size
/// and mtime and the key would still name the union built from what it held before.
fn plugin_files(dirs: &[PathBuf], now: SystemTime) -> Option<Vec<(PathBuf, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut walked = BTreeSet::new();
    for dir in dirs {
        out.push((dir.clone(), Vec::new()));
        for sub in ["library", "module_utils"] {
            files_under(&dir.join(sub), &mut walked, &mut out);
        }
    }
    let now_ns = i128::try_from(now.duration_since(UNIX_EPOCH).ok()?.as_nanos()).ok()?;
    let recent = out.iter().any(|(_, stamp)| {
        stamp
            .rchunks_exact(16)
            .next()
            .and_then(|mtime| mtime.try_into().ok())
            .is_some_and(|mtime| i128::from_le_bytes(mtime) > now_ns - 2_000_000_000)
    });
    (!recent).then_some(out)
}

/// Every file under `dir`, at any depth, in a stable order, each with its stamp: the real path,
/// then the size and the mtime in nanoseconds, little-endian, the mtime last.
fn files_under(dir: &Path, walked: &mut BTreeSet<PathBuf>, out: &mut Vec<(PathBuf, Vec<u8>)>) {
    let Ok(real) = fs::canonicalize(dir) else {
        return;
    };
    if !walked.insert(real) {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            files_under(&path, walked, out);
            continue;
        }
        let (Ok(real), Ok(source)) = (fs::canonicalize(&path), Source::now(&path)) else {
            continue;
        };
        let mut stamp = real.as_os_str().as_encoded_bytes().to_vec();
        stamp.extend_from_slice(&source.len.to_le_bytes());
        stamp.extend_from_slice(&source.mtime_ns.to_le_bytes());
        out.push((path, stamp));
    }
}

/// Every configuration file ansible-core's `find_ini_config_file` may pick, with its contents:
/// `$ANSIBLE_CONFIG` as a file, or as a directory holding `ansible.cfg`, then `./ansible.cfg`,
/// `~/.ansible.cfg` and `/etc/ansible/ansible.cfg`.
///
/// All of those that exist, not the one it picks: which one wins depends on rules (a missing
/// `ANSIBLE_CONFIG` falls through, a world-writable working directory is skipped) that a copy
/// here could get wrong, and a file hashed for nothing costs a rebuild, never a stale union.
fn configs(
    ansible_config: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
) -> Vec<(PathBuf, Vec<u8>)> {
    let mut candidates = Vec::new();
    if let Some(explicit) = ansible_config {
        let mut forms = vec![PathBuf::from(explicit)];
        if let Some(text) = explicit.to_str() {
            forms.push(PathBuf::from(expanded(text, home, |name| {
                std::env::var(name).ok()
            })));
        }
        for form in forms {
            candidates.push(form.join("ansible.cfg"));
            candidates.push(form);
        }
    }
    candidates.push(PathBuf::from("ansible.cfg"));
    if let Some(home) = home {
        candidates.push(home.join(".ansible.cfg"));
    }
    candidates.push(PathBuf::from("/etc/ansible/ansible.cfg"));
    candidates
        .into_iter()
        .filter_map(|path| fs::read(&path).ok().map(|text| (path, text)))
        .collect()
}

/// `text` as ansible-core's `unfrackpath` reads a path: `$VAR` and `${VAR}` replaced by their
/// values (an unset one left as written, as `os.path.expandvars` does), then a leading `~` by
/// `home`. `ANSIBLE_CONFIG='~/proj/ansible.cfg'` is left unexpanded by a Dockerfile `ENV`, a
/// systemd `Environment=` or a CI variable, and ansible-core still reads the file.
fn expanded(text: &str, home: Option<&Path>, var: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let (name, used) = if let Some(braced) = after.strip_prefix('{') {
            match braced.find('}') {
                Some(end) => (&braced[..end], end + 2),
                None => ("", 0),
            }
        } else {
            let end = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(after.len());
            (&after[..end], end)
        };
        match (!name.is_empty()).then(|| var(name)).flatten() {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[at..=at + used]),
        }
        rest = &after[used..];
    }
    out.push_str(rest);
    match (home, out.strip_prefix('~')) {
        (Some(home), Some(tail)) if tail.is_empty() || tail.starts_with('/') => {
            format!("{}{tail}", home.display())
        }
        _ => out,
    }
}

/// [`key`] with the process's part handed in.
///
/// The environment is `ANSIBLE_*` (every setting ansible-core reads there), `PYTHON*` (which
/// ansible-core the interpreter imports) and `HOME` (where `~` puts the collections). The working
/// directory is in because a relative path in either the environment or a relative
/// `ANSIBLE_CONFIG` is read against it. `files` is every file read for the key, with its contents.
fn key_from(
    interpreter: &InterpreterId,
    modules: &BTreeSet<String>,
    asked: &[String],
    env: &[(OsString, OsString)],
    files: &[(PathBuf, Vec<u8>)],
    cwd: &Path,
) -> CacheKey {
    let mut hasher = blake3::Hasher::new();
    // Each field carries its length, so no two different lists hash as one.
    let mut field = |bytes: &[u8]| {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    field(&FORMAT.to_le_bytes());
    field(env!("CARGO_PKG_VERSION").as_bytes());
    field(crate::python::HELPER.as_bytes());
    field(interpreter.path.as_os_str().as_encoded_bytes());
    field(interpreter.real.as_os_str().as_encoded_bytes());
    field(&interpreter.len.to_le_bytes());
    field(&interpreter.mtime_ns.to_le_bytes());
    field(b"modules");
    for module in modules {
        field(module.as_bytes());
    }
    field(b"asked");
    for name in asked {
        field(name.as_bytes());
    }
    field(b"env");
    let mut read: Vec<&(OsString, OsString)> = env
        .iter()
        .filter(|(name, _)| {
            let name = name.as_encoded_bytes();
            name.starts_with(b"ANSIBLE_") || name.starts_with(b"PYTHON") || name == b"HOME"
        })
        .collect();
    read.sort();
    for (name, value) in read {
        field(name.as_encoded_bytes());
        field(value.as_encoded_bytes());
    }
    field(b"cfg");
    for (path, text) in files {
        field(path.as_os_str().as_encoded_bytes());
        field(text);
    }
    field(b"cwd");
    field(cwd.as_os_str().as_encoded_bytes());
    CacheKey(hasher.finalize().to_hex().to_string())
}

/// The entry under `key`, or `None` on any doubt: a directory others could write, a manifest that
/// does not read or is of another format, a source whose size or mtime moved, or a zip whose
/// hash is not the manifest's.
pub fn load(dir: &Path, key: &CacheKey) -> Option<Entry> {
    open(dir, key).ok().flatten()
}

/// [`load`], telling a directory or a file that cannot be trusted (`Err`, which the run warns
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

/// `bytes` to `dir/name`, through a temporary file only this user can read.
fn write_new(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    static UNIQUE: AtomicU64 = AtomicU64::new(0);
    let tmp = dir.join(format!(
        "{name}.tmp.{}.{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options
        .open(&tmp)
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|()| fs::rename(&tmp, dir.join(name)));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// Refuses the cache directory, or the `volant` directory holding it, when either is a symbolic
/// link or is not this user's, when the cache directory can be written by anyone else, or when
/// the one above it can be written by everyone. Both are checked where they are, without
/// following a link: a `unions` planted as a link to `~/.ssh` in a `volant` directory another
/// account created under a shared `XDG_CACHE_HOME` would otherwise pass as private.
///
/// The `volant` directory may be group-writable: it is created under the umask, and a umask of
/// 002 with a group of one's own is the default for accounts on several distributions.
fn check_dir(dir: &Path) -> io::Result<()> {
    for (dir, forbidden) in [(Some(dir), 0o022), (dir.parent(), 0o002)] {
        let Some(dir) = dir else {
            continue;
        };
        let meta = fs::symlink_metadata(dir)?;
        if !meta.is_dir() {
            return Err(io::Error::other(format!(
                "{} is not a directory of its own",
                dir.display()
            )));
        }
        #[cfg(not(unix))]
        let _ = forbidden;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // SAFETY: `geteuid` reads the calling process's own credentials and cannot fail.
            let euid = unsafe { libc::geteuid() };
            let mode = meta.mode() & 0o7777;
            if meta.uid() != euid || mode & forbidden != 0 {
                return Err(io::Error::other(format!(
                    "{} is owned by uid {} with mode {mode:o}, and this controller runs as uid \
                     {euid}; it has to be this user's, and writable by nobody else",
                    dir.display(),
                    meta.uid()
                )));
            }
        }
    }
    Ok(())
}

/// Whether `name` is one this cache writes: `<key>.zip`, `<key>.deflate`, `<key>.json`, or a
/// temporary file of one of them. Nothing else in the directory is the sweep's to remove.
fn written_here(name: &str) -> bool {
    let (Some(key), Some(rest)) = (name.get(..64), name.get(64..)) else {
        return false;
    };
    let tmp = |rest: &str| {
        let mut parts = rest.split('.');
        parts.next() == Some("tmp")
            && parts.clone().count() == 2
            && parts.all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        && [".zip", ".deflate", ".json"].iter().any(|ext| {
            rest.strip_prefix(ext)
                .is_some_and(|after| after.is_empty() || after.strip_prefix('.').is_some_and(tmp))
        })
}

/// Removes every file of this cache in `dir` nothing has written for [`STALE`]: entries of module
/// sets no run asks for any more, and temporary files a run that died left behind. An entry
/// still in use but older than that is rebuilt once. A link is never followed, and a file this
/// cache did not name is never touched.
fn sweep(dir: &Path, now: SystemTime) {
    let Some(limit) = now.checked_sub(STALE) else {
        return;
    };
    let Ok(files) = fs::read_dir(dir) else {
        return;
    };
    for file in files.flatten() {
        if !file.file_name().to_str().is_some_and(written_here) {
            continue;
        }
        // `DirEntry::metadata` does not follow a link, and a link is not a file.
        let old = file.metadata().is_ok_and(|meta| {
            meta.is_file() && meta.modified().is_ok_and(|modified| modified < limit)
        });
        if old {
            let _ = fs::remove_file(file.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::python::ModuleFacts;

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tempdir() -> TempDir {
        static COUNT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "volant-union-cache-test-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    /// A union of one module whose zip is `bytes`, and one answer of each kind for the
    /// collections, so the manifest is read back whole.
    fn entry(root: &Path, bytes: &[u8]) -> Entry {
        let core = root.join("site/ansible/module_utils/basic.py");
        let files = root.join("coll/ansible_collections/ns/c/FILES.json");
        for (path, text) in [(&core, "basic"), (&files, "{\"files\": [0]}")] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        let modules = BTreeMap::from([(
            "ns.c.m".to_string(),
            ModuleFacts {
                module_fqn: "ansible_collections.ns.c.plugins.modules.m".into(),
                profile: "legacy".into(),
                rlimit_nofile: 0,
                extensions: Map::new(),
                core: false,
            },
        )]);
        let resolved = BTreeMap::from([
            (
                "ns.c.m".to_string(),
                Resolved::Module {
                    fqcn: "ns.c.m".into(),
                    collection: Some(("ns.c".into(), "1.0.0".into())),
                },
            ),
            (
                "ns.c.act".to_string(),
                Resolved::ActionPlugin {
                    fqcn: "ns.c.act".into(),
                },
            ),
            (
                "ns.c.gone".to_string(),
                Resolved::Unusable {
                    reason: "removed".into(),
                },
            ),
            (
                "no.such.m".to_string(),
                Resolved::Missing {
                    collection: Some("no.such".into()),
                },
            ),
            (
                "ns.c.none".to_string(),
                Resolved::Missing { collection: None },
            ),
        ]);
        Entry {
            union: Union {
                hash: blake3::hash(bytes).to_hex().to_string(),
                zip_b64: volant_protocol::encoding::b64_encode(bytes),
                modules,
                refused: BTreeMap::from([("ns.c.gone".to_string(), "removed".to_string())]),
                natives: crate::python::Natives::default(),
            },
            sources: [core, files]
                .iter()
                .map(|path| Source::now(path).unwrap())
                .collect(),
            resolved,
        }
    }

    fn interpreter() -> InterpreterId {
        InterpreterId {
            path: "/venv/bin/python".into(),
            real: "/usr/bin/python3.12".into(),
            len: 1,
            mtime_ns: 2,
        }
    }

    fn some_key() -> CacheKey {
        key_from(
            &interpreter(),
            &BTreeSet::from(["ns.c.m".to_string()]),
            &["ns.c.m".to_string()],
            &[],
            &[],
            Path::new("/work"),
        )
    }

    /// Moves `path`'s mtime a second back without touching its bytes or its length.
    fn age(path: &Path) {
        let file = fs::File::options().write(true).open(path).unwrap();
        let mtime = file.metadata().unwrap().modified().unwrap();
        file.set_modified(mtime - Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn a_second_run_with_nothing_changed_hits_the_cache() {
        let root = tempdir();
        let dir = root.0.join("unions");
        let stored = entry(&root.0, b"PK\x03\x04 zip");
        store(&dir, &some_key(), &stored).unwrap();
        let loaded = load(&dir, &some_key()).expect("nothing changed");
        assert_eq!(loaded, stored);
        assert_eq!(loaded.union.hash, stored.union.hash);
    }

    /// Review Focus 2: ansible-core upgraded at the same path, one of its files rewritten with
    /// the same length. The mtime moving is enough to rebuild.
    ///
    /// What would make this red: the mtime left out of the check, which serves the old zip, and
    /// its old `module_utils`, until a file happens to change length.
    #[test]
    fn a_source_changed_in_place_misses_the_cache() {
        let root = tempdir();
        let dir = root.0.join("unions");
        let stored = entry(&root.0, b"PK\x03\x04 zip");
        store(&dir, &some_key(), &stored).unwrap();
        let core = &stored.sources[0].path;
        fs::write(core, "BASIC").unwrap();
        age(core);
        assert_eq!(Source::now(core).unwrap().len, stored.sources[0].len);
        assert_eq!(load(&dir, &some_key()), None);
    }

    /// A collection upgraded in place: `ansible-galaxy collection install --upgrade` rewrites its
    /// `FILES.json`, and the entry is rebuilt.
    #[test]
    fn a_collection_upgrade_misses_the_cache() {
        let root = tempdir();
        let dir = root.0.join("unions");
        let stored = entry(&root.0, b"PK\x03\x04 zip");
        store(&dir, &some_key(), &stored).unwrap();
        let files = &stored.sources[1].path;
        fs::write(files, "{\"files\": [1]}").unwrap();
        age(files);
        assert_eq!(load(&dir, &some_key()), None);
    }

    /// Every input that is not a file has its own key.
    ///
    /// What would make this red: `ANSIBLE_MODULE_COMPRESSION` or `ANSIBLE_COLLECTIONS_PATH`
    /// left out, which serves a union built under another configuration.
    #[test]
    fn an_ansible_environment_change_is_another_key() {
        let modules = BTreeSet::from(["ping".to_string()]);
        let with = |env: &[(&str, &str)], cfg: &[(PathBuf, Vec<u8>)], cwd: &str| {
            let env: Vec<(OsString, OsString)> = env
                .iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect();
            key_from(&interpreter(), &modules, &[], &env, cfg, Path::new(cwd))
        };
        let base = with(&[("TERM", "xterm")], &[], "/work");
        assert_eq!(base, with(&[("TERM", "dumb")], &[], "/work"));
        for other in [
            with(
                &[("ANSIBLE_MODULE_COMPRESSION", "ZIP_STORED")],
                &[],
                "/work",
            ),
            with(&[("ANSIBLE_COLLECTIONS_PATH", "/c")], &[], "/work"),
            with(&[("PYTHONPATH", "/p")], &[], "/work"),
            with(
                &[],
                &[("ansible.cfg".into(), b"[defaults]\n".to_vec())],
                "/work",
            ),
            with(&[], &[], "/elsewhere"),
            key_from(
                &InterpreterId {
                    mtime_ns: 3,
                    ..interpreter()
                },
                &modules,
                &[],
                &[],
                &[],
                Path::new("/work"),
            ),
            // A Nix profile's `python3`: the same path and size, another store path behind it.
            key_from(
                &InterpreterId {
                    real: "/nix/store/other-python3/bin/python3".into(),
                    ..interpreter()
                },
                &modules,
                &[],
                &[],
                &[],
                Path::new("/work"),
            ),
            key_from(
                &interpreter(),
                &BTreeSet::from(["ping".to_string(), "stat".to_string()]),
                &[],
                &[],
                &[],
                Path::new("/work"),
            ),
        ] {
            assert_ne!(base, other);
        }
    }

    /// Writes `text` to `path` and sets its mtime `ago` seconds back, old enough to vouch for.
    fn written(path: &Path, text: &str, ago: u64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(ago))
            .unwrap();
    }

    /// The key over `dirs` and nothing else, the plugin files read as `key` reads them.
    fn plugin_key(dirs: &[PathBuf]) -> CacheKey {
        key_from(
            &interpreter(),
            &BTreeSet::from(["stat".to_string()]),
            &[],
            &[],
            &plugin_files(dirs, SystemTime::now() + Duration::from_secs(3))
                .expect("nothing recent"),
            Path::new("/work"),
        )
    }

    /// A module written, or rewritten in place, in a playbook's `library/` or `module_utils/`
    /// is another key, and so is a playbook directory named in another place of the list.
    ///
    /// What would make this red: the key leaving the plugin directories out, which serves the
    /// union built before a `library/stat.py` existed, with ansible-core's own `stat`, since no
    /// source it kept names a directory that was not there; or a rewrite of the same size keeping
    /// the key, which the mtime in each file's stamp is there to catch.
    #[test]
    fn a_module_written_in_a_playbook_s_library_is_another_key() {
        let root = tempdir();
        let (play, role) = (root.0.join("play"), root.0.join("role"));
        fs::create_dir_all(&play).unwrap();
        fs::create_dir_all(&role).unwrap();
        let both = [play.clone(), role.clone()];
        let mut seen = vec![plugin_key(&both)];
        assert_ne!(
            plugin_key(&[role.clone(), play.clone()]),
            seen[0],
            "the order"
        );
        let stat = play.join("library").join("stat.py");
        written(&stat, "one", 100);
        seen.push(plugin_key(&both));
        written(&stat, "two", 50);
        seen.push(plugin_key(&both));
        written(&role.join("module_utils").join("net").join("a.py"), "", 100);
        seen.push(plugin_key(&both));
        for (i, one) in seen.iter().enumerate() {
            for other in &seen[i + 1..] {
                assert_ne!(one, other);
            }
        }
        assert_eq!(plugin_key(&both), seen[3], "nothing changed since");
    }

    /// A plugin file changed less than two seconds ago gives no key, so nothing is kept.
    ///
    /// What would make this red: the racy-timestamp rule left out, which files a union under the
    /// stamp of a file that can still change without moving its size or its mtime.
    #[test]
    fn a_plugin_file_too_recent_gives_no_key() {
        let root = tempdir();
        fs::create_dir_all(root.0.join("library")).unwrap();
        fs::write(root.0.join("library").join("m.py"), "x").unwrap();
        let dirs = [root.0.clone()];
        assert!(plugin_files(&dirs, SystemTime::now()).is_none());
        assert!(plugin_files(&dirs, SystemTime::now() + Duration::from_secs(3)).is_some());
    }

    /// An edit behind a linked directory in `module_utils/` is another key, and a link back to an
    /// ancestor is walked no further.
    ///
    /// What would make this red: a link to a directory read as a file and dropped, which leaves
    /// everything behind it out of the key.
    #[cfg(unix)]
    #[test]
    fn an_edit_behind_a_linked_directory_is_another_key() {
        let root = tempdir();
        let (role, shared) = (root.0.join("role"), root.0.join("shared"));
        written(&shared.join("m.py"), "one", 100);
        let utils = role.join("module_utils");
        fs::create_dir_all(&utils).unwrap();
        std::os::unix::fs::symlink(&shared, utils.join("shared")).unwrap();
        std::os::unix::fs::symlink(&utils, utils.join("again")).unwrap();
        let before = plugin_key(std::slice::from_ref(&role));
        written(&shared.join("m.py"), "three", 100);
        assert_ne!(plugin_key(std::slice::from_ref(&role)), before);
    }

    /// An entry that does not read whole is rebuilt, not trusted.
    #[test]
    fn a_corrupt_entry_is_rebuilt_not_trusted() {
        let root = tempdir();
        let dir = root.0.join("unions");
        let key = some_key();
        let stored = entry(&root.0, b"PK\x03\x04 zip");
        store(&dir, &key, &stored).unwrap();
        let zip = dir.join(format!("{}.zip", key.0));
        let bytes = fs::read(&zip).unwrap();
        fs::write(&zip, &bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(load(&dir, &key), None, "a truncated zip");

        store(&dir, &key, &stored).unwrap();
        let manifest = dir.join(format!("{}.json", key.0));
        fs::write(&manifest, "{\"format\": 1, \"hash\": ").unwrap();
        assert_eq!(load(&dir, &key), None, "a manifest that is not JSON");

        store(&dir, &key, &stored).unwrap();
        let text = fs::read_to_string(&manifest).unwrap();
        fs::write(
            &manifest,
            text.replace(&format!("\"format\":{FORMAT}"), "\"format\":99"),
        )
        .unwrap();
        assert_eq!(load(&dir, &key), None, "another format");

        fs::remove_file(&manifest).unwrap();
        assert_eq!(load(&dir, &key), None, "no entry");
    }

    /// An entry written before the facts carried `core` is a miss, not a union whose modules all
    /// read as someone else's: the payloads are rebuilt and say what they are.
    ///
    /// What would make this red: the format left at 1 when `core` was added.
    #[test]
    fn an_entry_from_before_core_is_a_miss() {
        let root = tempdir();
        let dir = root.0.join("unions");
        let key = some_key();
        let mut stored = entry(&root.0, b"PK zip");
        stored
            .union
            .modules
            .values_mut()
            .for_each(|f| f.core = true);
        store(&dir, &key, &stored).unwrap();
        assert!(load(&dir, &key).is_some(), "the current format reads back");
        let manifest = dir.join(format!("{}.json", key.0));
        let text = fs::read_to_string(&manifest).unwrap();
        let old = text
            .replace(&format!("\"format\":{FORMAT}"), "\"format\":1")
            .replace(",\"core\":true", "")
            .replace("\"core\":true,", "");
        assert!(!old.contains("core"), "{old}");
        fs::write(&manifest, old).unwrap();
        assert_eq!(load(&dir, &key), None);
    }

    /// A zip long enough to be sent deflated.
    const LONG_ZIP: &[u8] = b"PK\x03\x04 a stored zip, as the helper builds it; ";

    fn long_zip() -> Vec<u8> {
        LONG_ZIP.repeat(200)
    }

    /// The bytes a frame carries, decoded as the agent decodes them.
    fn unpacked(frame: &BlobFrame) -> Vec<u8> {
        match frame.encoding {
            volant_protocol::BlobEncoding::Raw => frame.bytes.clone(),
            volant_protocol::BlobEncoding::Deflate => {
                volant_protocol::encoding::inflate(&frame.bytes, frame.len as usize).unwrap()
            }
        }
    }

    /// The frame kept for a union is found by the zip's hash and holds that zip: storing an
    /// entry keeps its deflated form, and the frame then comes from it without decoding the
    /// union again, while another zip gets its own.
    ///
    /// What would make this red: the frames keyed by anything but the zip's content (the cache
    /// key, the host), under which a union rebuilt in place would go up as the one before it.
    #[tokio::test]
    async fn the_frame_kept_for_a_union_is_found_by_its_content_alone() {
        let root = tempdir();
        let dir = root.0.join("unions");
        let zip = long_zip();
        let stored = entry(&root.0, &zip);
        store(&dir, &some_key(), &stored).unwrap();
        let kept = crate::agent::frame(&stored.union.hash, "not base64 at all")
            .await
            .expect("kept by the store");
        assert_eq!(kept.encoding, volant_protocol::BlobEncoding::Deflate);
        assert_eq!(unpacked(&kept), zip);

        let other = [b"another zip ".as_slice(), &zip].concat();
        let other_hash = blake3::hash(&other).to_hex().to_string();
        let fresh =
            crate::agent::frame(&other_hash, &volant_protocol::encoding::b64_encode(&other))
                .await
                .unwrap();
        assert_eq!(unpacked(&fresh), other);
        let again = crate::agent::frame(&stored.union.hash, "").await.unwrap();
        assert_eq!(unpacked(&again), zip);
    }

    /// An entry of format 2, which has no deflated form, is a miss, never served, even with a
    /// `.deflate` file beside it. Two checks hold that, each tried alone: the format, and the
    /// deflated form's hash, which a format 3 manifest must carry.
    ///
    /// What would make this red: the format left at 2 or the format check relaxed (the case
    /// that keeps the hash), or the hash read as optional (the format 3 case without one).
    #[test]
    fn a_format_two_entry_is_a_miss() {
        let root = tempdir();
        let dir = root.0.join("unions");
        let key = some_key();
        let stored = entry(&root.0, &long_zip());
        store(&dir, &key, &stored).unwrap();
        let manifest = dir.join(format!("{}.json", key.0));
        let text = fs::read_to_string(&manifest).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        let hash = format!("\"deflated\":\"{}\",", value["deflated"].as_str().unwrap());
        let (three, two) = ("\"format\":3", "\"format\":2");
        assert!(text.contains(three) && text.contains(&hash), "{text}");
        for (why, old) in [
            ("format 2 with a hash", text.replace(three, two)),
            ("format 2", text.replace(three, two).replace(&hash, "")),
            ("format 3 without a hash", text.replace(&hash, "")),
        ] {
            fs::write(&manifest, old).unwrap();
            assert_eq!(load(&dir, &key), None, "{why}");
        }
        fs::write(&manifest, text).unwrap();
        assert_eq!(load(&dir, &key), Some(stored));
    }

    /// A deflated form that is missing, or is not the one the manifest names, is made again from
    /// the zip and written back, and the frame holds the zip: the entry is still a hit.
    ///
    /// What would make this red: the `.deflate` file read without its hash checked, which sends
    /// every host whatever that file holds, and the agents refuse it one by one; or a missing
    /// one taken for a miss, which rebuilds the whole union for a file the zip can give back.
    #[tokio::test]
    async fn a_missing_or_corrupt_deflated_form_is_made_again_never_sent() {
        let root = tempdir();
        let dir = root.0.join("unions");
        let key = some_key();
        let zip = long_zip();
        let stored = entry(&root.0, &zip);
        store(&dir, &key, &stored).unwrap();
        let path = dir.join(format!("{}.deflate", key.0));
        let good = fs::read(&path).unwrap();
        let wrong =
            volant_protocol::encoding::deflate(&[b"not this union ".as_slice(), &zip].concat());
        for (why, damage) in [
            ("missing", None),
            ("another stream", Some(wrong)),
            ("truncated", Some(good[..good.len() / 2].to_vec())),
        ] {
            match damage {
                None => fs::remove_file(&path).unwrap(),
                Some(bytes) => fs::write(&path, bytes).unwrap(),
            }
            crate::agent::forget_frames();
            assert_eq!(load(&dir, &key), Some(stored.clone()), "{why}");
            let kept = crate::agent::frame(&stored.union.hash, "not base64 at all")
                .await
                .expect(why);
            assert_eq!(unpacked(&kept), zip, "{why}");
            assert_eq!(fs::read(&path).unwrap(), good, "{why}: written back");
        }
    }

    /// What would make this red: the zip read back without its hash checked, which hands the
    /// hosts a blob whose name says something else.
    #[test]
    fn a_zip_whose_hash_moved_is_never_loaded() {
        let root = tempdir();
        let dir = root.0.join("unions");
        let key = some_key();
        store(&dir, &key, &entry(&root.0, b"PK\x03\x04 zip")).unwrap();
        let zip = dir.join(format!("{}.zip", key.0));
        let mut bytes = fs::read(&zip).unwrap();
        bytes[4] ^= 1;
        fs::write(&zip, bytes).unwrap();
        assert_eq!(load(&dir, &key), None);
    }

    /// The directory is created private, the files are readable by this user alone, and a
    /// directory others can write is never read from.
    #[cfg(unix)]
    #[test]
    fn the_cache_is_private_to_this_user() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempdir();
        let dir = root.0.join("unions");
        let key = some_key();
        store(&dir, &key, &entry(&root.0, b"PK\x03\x04 zip")).unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(format!("{}.zip", key.0))), 0o600);
        assert_eq!(mode(&dir.join(format!("{}.json", key.0))), 0o600);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(load(&dir, &key), None);
    }

    /// What would make this red: nothing ever removed, so every module set a playbook ever
    /// named stays in the user's home for good.
    #[test]
    fn an_entry_left_alone_for_a_month_is_swept() {
        let root = tempdir();
        let dir = root.0.join("unions");
        let key = some_key();
        store(&dir, &key, &entry(&root.0, b"PK\x03\x04 zip")).unwrap();
        sweep(&dir, SystemTime::now() + STALE - Duration::from_secs(60));
        assert!(load(&dir, &key).is_some(), "a recent entry is kept");
        sweep(&dir, SystemTime::now() + STALE + Duration::from_secs(60));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }

    /// The interpreter is found where `find_python` would look first, without running it, and
    /// named by the path it was found at.
    #[test]
    fn the_interpreter_is_found_without_running_it() {
        let root = tempdir();
        let python = root.0.join("venv/bin/python");
        fs::create_dir_all(python.parent().unwrap()).unwrap();
        fs::write(&python, "not a python").unwrap();
        let venv = root.0.join("venv").display().to_string();
        let found = interpreter_without_running(None, Some(&venv)).unwrap();
        assert_eq!(found.path, python);
        assert_eq!(found.len, 12);
        let explicit = python.display().to_string();
        assert_eq!(
            interpreter_without_running(Some(&explicit), None).unwrap(),
            found
        );
        assert_eq!(
            interpreter_without_running(Some("/nowhere/python"), None),
            None
        );
    }

    /// A `#!` shim caches nothing.
    ///
    /// What would make this red: the shim taken for the interpreter. pyenv's `python3` is one
    /// script for every version, so `pyenv global` from a 3.11 with ansible-core 2.17 to a 3.12
    /// with 2.19.12 keeps the key, every 3.11 source is still on disk unchanged, and the 2.17
    /// union is served.
    #[test]
    fn a_shim_interpreter_is_never_a_key() {
        let root = tempdir();
        let shim = root.0.join("shims/python3");
        fs::create_dir_all(shim.parent().unwrap()).unwrap();
        fs::write(
            &shim,
            "#!/usr/bin/env bash\nexec pyenv exec python3 \"$@\"\n",
        )
        .unwrap();
        let explicit = shim.display().to_string();
        assert_eq!(interpreter_without_running(Some(&explicit), None), None);
    }

    /// The interpreter is named by its path and by where its links lead.
    #[cfg(unix)]
    #[test]
    fn the_interpreter_names_where_its_link_leads() {
        let root = tempdir();
        let real = root.0.join("store/python3");
        let link = root.0.join("profile/python3");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        fs::write(&real, "ELF").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let found = interpreter_without_running(Some(&link.display().to_string()), None).unwrap();
        assert_eq!(found.path, link);
        assert_eq!(found.real, fs::canonicalize(&real).unwrap());
    }

    /// `ANSIBLE_CONFIG` naming a directory is read as ansible-core reads it, `<dir>/ansible.cfg`.
    ///
    /// What would make this red: the directory form left out, which keeps the key when its
    /// `ansible.cfg` moves `collections_path` to a directory with a newer collection, while every
    /// tracked file stays where it was.
    #[test]
    fn an_ansible_config_directory_is_read_for_the_key() {
        let root = tempdir();
        let cfg = root.0.join("ansible.cfg");
        let key_now = || {
            let found = configs(Some(root.0.as_os_str()), None);
            key_from(
                &interpreter(),
                &BTreeSet::new(),
                &[],
                &[],
                &found,
                Path::new("/work"),
            )
        };
        fs::write(&cfg, "[defaults]\ncollections_path = ./colls\n").unwrap();
        let before = key_now();
        fs::write(&cfg, "[defaults]\ncollections_path = ./colls-next\n").unwrap();
        assert_ne!(key_now(), before);
        // A file named directly, and one that is not there, are both what they are.
        assert_eq!(configs(Some(cfg.as_os_str()), None)[0].0, cfg);
        let home = root.0.join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join(".ansible.cfg"), "[defaults]\n").unwrap();
        let fell_through = configs(Some(root.0.join("absent.cfg").as_os_str()), Some(&home));
        assert!(
            fell_through
                .iter()
                .any(|(path, _)| *path == home.join(".ansible.cfg")),
            "{fell_through:?}"
        );
    }

    /// `ANSIBLE_CONFIG` is expanded as ansible-core's `unfrackpath` expands it before it is looked
    /// for.
    ///
    /// What would make this red: `~/proj/ansible.cfg` read as written, which names no file, so
    /// editing the one ansible-core does read keeps the key.
    #[test]
    fn an_ansible_config_is_expanded_before_it_is_read() {
        let var = |name: &str| (name == "PROJ").then(|| "/srv/proj".to_string());
        let home = Some(Path::new("/home/u"));
        assert_eq!(
            expanded("~/a/ansible.cfg", home, var),
            "/home/u/a/ansible.cfg"
        );
        assert_eq!(
            expanded("$PROJ/ansible.cfg", home, var),
            "/srv/proj/ansible.cfg"
        );
        assert_eq!(expanded("${PROJ}/x/~", home, var), "/srv/proj/x/~");
        assert_eq!(expanded("$NOPE/${NOPE}/$", home, var), "$NOPE/${NOPE}/$");
        assert_eq!(expanded("~other/a", home, var), "~other/a");

        let root = tempdir();
        let cfg = root.0.join("proj/ansible.cfg");
        fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        fs::write(
            &cfg,
            "[defaults]
",
        )
        .unwrap();
        let found = configs(Some("~/proj/ansible.cfg".as_ref()), Some(&root.0));
        assert!(found.iter().any(|(path, _)| *path == cfg), "{found:?}");
    }

    /// A cache directory that is a link is neither read nor written, and a `volant` directory
    /// everyone can write is refused with it.
    ///
    /// What would make this red: the link followed, which on a shared `XDG_CACHE_HOME` lets
    /// another account point `unions` at `~/.ssh`, where the store writes and the sweep deletes.
    #[cfg(unix)]
    #[test]
    fn a_linked_or_shared_cache_directory_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempdir();
        let target = root.0.join("dot-ssh");
        fs::create_dir_all(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let volant = root.0.join("volant");
        fs::create_dir_all(&volant).unwrap();
        let dir = volant.join("unions");
        std::os::unix::fs::symlink(&target, &dir).unwrap();
        let key = some_key();
        assert!(store(&dir, &key, &entry(&root.0, b"PK")).is_err());
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        assert_eq!(load(&dir, &key), None);

        fs::remove_file(&dir).unwrap();
        store(&dir, &key, &entry(&root.0, b"PK")).unwrap();
        fs::set_permissions(&volant, fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(load(&dir, &key), None);
        fs::set_permissions(&volant, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(
            load(&dir, &key).is_some(),
            "a group-writable volant/ is the 002 umask"
        );
    }

    /// The sweep removes only what this cache wrote, never through a link.
    ///
    /// What would make this red: every old file removed, which clears whatever else sits in the
    /// directory, and whatever a link in it leads to.
    #[test]
    fn the_sweep_removes_only_its_own_files() {
        let root = tempdir();
        let dir = root.0.join("unions");
        let key = some_key();
        store(&dir, &key, &entry(&root.0, b"PK")).unwrap();
        let hex = "a".repeat(64);
        for name in [
            "id_ed25519".to_string(),
            format!("{hex}.zip.bak"),
            format!("{hex}.json.tmp.x.1"),
            format!("{}.zip", "A".repeat(64)),
        ] {
            fs::write(dir.join(name), "not the cache's").unwrap();
        }
        fs::write(dir.join(format!("{hex}.json.tmp.12.3")), "a dead run's").unwrap();
        sweep(&dir, SystemTime::now() + STALE + Duration::from_secs(60));
        let mut left: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|f| f.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                format!("{}.zip", "A".repeat(64)),
                format!("{hex}.json.tmp.x.1"),
                format!("{hex}.zip.bak"),
                "id_ed25519".to_string(),
            ]
        );
    }
}
