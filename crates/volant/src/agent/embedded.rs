// SPDX-License-Identifier: GPL-3.0-or-later
//! The agents built into this controller, and the cache directory they are run from.
//!
//! A release build embeds its agents (see `build.rs` and `VOLANT_EMBED_AGENTS_DIR`), so a
//! controller installed on its own can still run a task. The agent is spawned locally and
//! uploaded over ssh from a file path, so each embedded agent is written to a per-version cache
//! directory the first time it is needed and reused from there afterwards.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

// Defines `AGENTS: &[(&str, &[u8])]`, file name and bytes, empty unless the build embedded any.
include!(concat!(env!("OUT_DIR"), "/embedded_agents.rs"));

/// The embedded agents together with the directory they are extracted to. The directory is
/// resolved once, at discovery; a failure to name one is kept and reported with the lookup that
/// needed it, rather than failing a run that finds its agent elsewhere.
#[derive(Debug, Clone)]
pub(super) struct Embedded {
    agents: &'static [(&'static str, &'static [u8])],
    dir: Result<PathBuf, String>,
}

impl Embedded {
    /// This build's own agents, or `None` when it embeds none.
    pub(super) fn built_in() -> Option<Self> {
        (!AGENTS.is_empty()).then(|| Self::new(AGENTS, cache_dir()))
    }

    pub(super) fn new(
        agents: &'static [(&'static str, &'static [u8])],
        dir: Result<PathBuf, String>,
    ) -> Self {
        Self { agents, dir }
    }

    /// The path of the embedded agent named `file`, extracted if it has to be. `Ok(None)` when
    /// this controller embeds no agent by that name; `Err` says why one it does embed could not be
    /// put on disk.
    pub(super) fn find(&self, file: &str) -> Result<Option<PathBuf>, String> {
        let Some((_, bytes)) = self.agents.iter().find(|(name, _)| *name == file) else {
            return Ok(None);
        };
        let dir = self.dir.as_ref().map_err(Clone::clone)?;
        extract(dir, file, bytes)
            .map(Some)
            .map_err(|err| format!("extracting {file} into {}: {err}", dir.display()))
    }

    pub(super) fn describe(&self) -> String {
        match &self.dir {
            Ok(dir) => format!("the agents built into this controller ({})", dir.display()),
            Err(err) => format!("the agents built into this controller ({err})"),
        }
    }
}

/// `$XDG_CACHE_HOME/volant/agents/<version>`, or `~/.cache/volant/agents/<version>`. The version
/// keeps two installed controllers from taking turns rewriting each other's agents.
fn cache_dir() -> Result<PathBuf, String> {
    let base = cache_root()
        .ok_or("neither XDG_CACHE_HOME nor HOME names an absolute directory to extract them to")?;
    Ok(base.join("agents").join(env!("CARGO_PKG_VERSION")))
}

/// `$XDG_CACHE_HOME/volant`, or `~/.cache/volant`, when either names an absolute directory: a
/// relative one would put the cache in whatever directory the run started in.
pub(crate) fn cache_root() -> Option<PathBuf> {
    let absolute = |var: &str| {
        std::env::var_os(var)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    absolute("XDG_CACHE_HOME")
        .or_else(|| absolute("HOME").map(|home| home.join(".cache")))
        .map(|base| base.join("volant"))
}

/// Writes `bytes` to `dir/name` with mode 0755, unless an executable file with exactly those
/// bytes is already there, and returns its path.
///
/// The file is written under a temporary name in the same directory and renamed into place, so a
/// run never sees half an agent and two runs extracting at once both end up with a whole one.
/// Nothing is synced: a file a crash left short or zero-filled fails the comparison on the next
/// call and is written again.
fn extract(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    static UNIQUE: AtomicU64 = AtomicU64::new(0);

    create_private(dir)?;
    check_private(dir)?;
    let path = dir.join(name);
    if holds(&path, bytes)? {
        return Ok(path);
    }
    let tmp = dir.join(format!(
        "{name}.tmp.{}.{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    ));
    let written = fs::File::create_new(&tmp)
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|()| make_executable(&tmp))
        .and_then(|()| fs::rename(&tmp, &path));
    if let Err(err) = written {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(path)
}

/// Whether `path` is an executable file holding exactly `bytes`. The length is compared first so
/// a truncated copy costs no read.
fn holds(path: &Path, bytes: &[u8]) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && meta.len() == bytes.len() as u64 && executable(&meta) => {
            Ok(fs::read(path)? == bytes)
        }
        Ok(_) => Ok(false),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

#[cfg(unix)]
fn executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 == 0o111
}

#[cfg(not(unix))]
fn executable(_meta: &fs::Metadata) -> bool {
    true
}

/// Set after the write rather than at creation, where the umask would decide the mode.
#[cfg(unix)]
fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Creates the directory mode 0700, its parents as the umask has them, and says nothing when it
/// already exists: whether an existing one can be trusted is [`check_private`]'s answer.
#[cfg(unix)]
pub(crate) fn create_private(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        other => other,
    }
}

#[cfg(not(unix))]
pub(crate) fn create_private(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

/// Refuses a directory this user does not own, or one anyone else can write. Either would let
/// another account put its own program where this controller looks for an agent, and the agent
/// runs with this user's rights locally and is uploaded to every host.
#[cfg(unix)]
pub(crate) fn check_private(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let meta = fs::metadata(dir)?;
    // SAFETY: `geteuid` reads the calling process's own credentials and cannot fail.
    let euid = unsafe { libc::geteuid() };
    let mode = meta.mode() & 0o7777;
    if !meta.is_dir() || meta.uid() != euid || mode & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "it is owned by uid {} with mode {mode:o}, and this controller runs as uid {euid}; \
                 it has to be a directory only its owner can write",
                meta.uid()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn check_private(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests;
