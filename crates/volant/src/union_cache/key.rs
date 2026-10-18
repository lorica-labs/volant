// SPDX-License-Identifier: GPL-3.0-or-later
//! The key a run files its union under, and the interpreter and files it is read from.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{CacheKey, FORMAT, InterpreterId, Place, Source};

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
    // The `-M` directories hold modules directly, not under a `library/`.
    files.extend(module_path_files(
        &crate::python::module_path(),
        SystemTime::now(),
    )?);
    let cwd = std::env::current_dir().unwrap_or_default();
    let controller = std::env::current_exe()
        .and_then(|exe| Source::now(&exe))
        .ok();
    Some(key_from(
        controller.as_ref(),
        interpreter,
        modules,
        asked,
        &env,
        &files,
        &cwd,
    ))
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
pub(super) fn plugin_files(dirs: &[PathBuf], now: SystemTime) -> Option<Vec<(PathBuf, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut walked = BTreeSet::new();
    for dir in dirs {
        out.push((dir.clone(), Vec::new()));
        for sub in ["library", "module_utils"] {
            files_under(&dir.join(sub), &mut walked, &mut out);
        }
    }
    recent_free(out, now)
}

/// `out`, unless a stamp in it is under two seconds old.
fn recent_free(out: Vec<(PathBuf, Vec<u8>)>, now: SystemTime) -> Option<Vec<(PathBuf, Vec<u8>)>> {
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

/// The `-M` directories and every file under them, with the same rule as [`plugin_files`]: `None`
/// when one was written too recently to vouch for.
fn module_path_files(dirs: &[PathBuf], now: SystemTime) -> Option<Vec<(PathBuf, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut walked = BTreeSet::new();
    for dir in dirs {
        out.push((dir.clone(), Vec::new()));
        files_under(dir, &mut walked, &mut out);
    }
    recent_free(out, now)
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
pub(super) fn configs(
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
pub(super) fn expanded(
    text: &str,
    home: Option<&Path>,
    var: impl Fn(&str) -> Option<String>,
) -> String {
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
/// `ANSIBLE_CONFIG` is read against it. `files` is every file read for the key, with its contents
/// or its stamp. The controller's own binary (`controller`, its size and mtime) is in because the
/// code that reads an entry back, refusals and fact keys, can change between two builds that
/// share a version number.
pub(super) fn key_from(
    controller: Option<&Source>,
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
    match controller {
        Some(exe) => {
            field(&exe.len.to_le_bytes());
            field(&exe.mtime_ns.to_le_bytes());
        }
        None => field(b"no controller binary"),
    }
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
