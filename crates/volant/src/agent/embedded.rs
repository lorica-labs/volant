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
    let absolute = |var: &str| {
        std::env::var_os(var)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    let base = absolute("XDG_CACHE_HOME")
        .or_else(|| absolute("HOME").map(|home| home.join(".cache")))
        .ok_or("neither XDG_CACHE_HOME nor HOME names an absolute directory to extract them to")?;
    Ok(base
        .join("volant")
        .join("agents")
        .join(env!("CARGO_PKG_VERSION")))
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
fn create_private(dir: &Path) -> io::Result<()> {
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
fn create_private(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

/// Refuses a directory this user does not own, or one anyone else can write. Either would let
/// another account put its own program where this controller looks for an agent, and the agent
/// runs with this user's rights locally and is uploaded to every host.
#[cfg(unix)]
fn check_private(dir: &Path) -> io::Result<()> {
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
fn check_private(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
pub(super) mod tests {
    use super::*;

    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    pub(in crate::agent) struct TempDir(pub(in crate::agent) PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    pub(in crate::agent) fn tempdir() -> TempDir {
        static COUNT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "volant-embedded-test-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("the test directory is created");
        TempDir(dir)
    }

    const AGENT: &[u8] = b"#!/bin/sh\necho embedded\n";

    /// A second extraction of an agent already in place leaves the file alone.
    ///
    /// What would make this red: the comparison with what is on disk skipped, so every lookup
    /// writes a new copy and renames it over the old one. Proved on the inode, which a rewrite
    /// through a temporary name always changes, whatever the bytes are.
    #[test]
    fn extracting_an_agent_already_in_place_writes_nothing() {
        let root = tempdir();
        let dir = root.0.join("cache");
        let first = extract(&dir, "volant-agent", AGENT).unwrap();
        let inode = fs::metadata(&first).unwrap().ino();

        let second = extract(&dir, "volant-agent", AGENT).unwrap();
        assert_eq!(second, first);
        assert_eq!(
            fs::metadata(&second).unwrap().ino(),
            inode,
            "the agent was rewritten"
        );
        assert_eq!(fs::read(&second).unwrap(), AGENT);
    }

    /// A copy that was cut short, or altered without changing its length, is replaced.
    ///
    /// What would make this red: the comparison reduced to the length (the altered copy is
    /// kept) or to the file merely existing (both are kept). Either way the controller would run
    /// or upload whatever is sitting in its cache.
    #[test]
    fn a_short_or_altered_copy_is_written_again() {
        let root = tempdir();
        let dir = root.0.join("cache");
        extract(&dir, "volant-agent", AGENT).unwrap();
        let path = dir.join("volant-agent");

        let mut altered = AGENT.to_vec();
        altered[0] ^= 0xff;
        for wrong in [&AGENT[..4], &altered[..]] {
            fs::write(&path, wrong).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            extract(&dir, "volant-agent", AGENT).unwrap();
            assert_eq!(fs::read(&path).unwrap(), AGENT);
        }
    }

    /// The extracted agent is mode 0755 whatever the umask, and a copy that lost its execute bit
    /// is replaced even though its bytes are right.
    ///
    /// What would make this red: the mode left to `File::create` and the umask (the file lands
    /// 0644 and `runnable` refuses it), or the execute bit dropped from the comparison (the
    /// non-executable copy is kept).
    #[test]
    fn the_extracted_agent_is_executable() {
        let root = tempdir();
        let dir = root.0.join("cache");
        let path = extract(&dir, "volant-agent", AGENT).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o755);

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        extract(&dir, "volant-agent", AGENT).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o755);
    }

    /// A cache directory another account could write is refused, nothing is written into it, and
    /// the error gives its mode.
    ///
    /// What would make this red: the ownership and mode check skipped for a directory that
    /// already exists, which is exactly the one somebody else could have prepared.
    #[test]
    fn a_cache_directory_others_can_write_is_refused() {
        let root = tempdir();
        let dir = root.0.join("cache");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();

        let err = extract(&dir, "volant-agent", AGENT).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("mode 777"), "{err}");
        assert!(!dir.join("volant-agent").exists());
    }

    /// A new cache directory is created private to this user.
    ///
    /// What would make this red: the directory made with `create_dir_all` and the umask, which
    /// under a umask of 002 leaves it group-writable and has the check above refuse the
    /// controller's own cache on its first run.
    #[test]
    fn a_new_cache_directory_is_private() {
        let root = tempdir();
        let dir = root.0.join("a").join("cache");
        // SAFETY: `umask` only swaps this process's file mode mask; nextest runs each test in a
        // process of its own, so no other thread is creating files meanwhile.
        let previous = unsafe { libc::umask(0o002) };
        let result = extract(&dir, "volant-agent", AGENT);
        unsafe { libc::umask(previous) };
        result.unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
    }

    /// The cache is named after this controller's version, under `XDG_CACHE_HOME` when that is
    /// absolute and under `HOME` otherwise.
    ///
    /// What would make this red: the version left out of the path, or a relative
    /// `XDG_CACHE_HOME` used as it is, which would extract into whatever directory the run
    /// started in.
    #[test]
    fn the_cache_directory_follows_xdg_then_home() {
        let version = env!("CARGO_PKG_VERSION");
        let with = |xdg: Option<&str>, home: Option<&str>| {
            // SAFETY: nextest runs each test in a process of its own, and nothing else in this
            // one reads the environment while it changes.
            unsafe {
                for (var, value) in [("XDG_CACHE_HOME", xdg), ("HOME", home)] {
                    match value {
                        Some(v) => std::env::set_var(var, v),
                        None => std::env::remove_var(var),
                    }
                }
            }
            cache_dir()
        };

        assert_eq!(
            with(Some("/xdg"), Some("/home/u")).unwrap(),
            PathBuf::from(format!("/xdg/volant/agents/{version}"))
        );
        assert_eq!(
            with(Some("relative"), Some("/home/u")).unwrap(),
            PathBuf::from(format!("/home/u/.cache/volant/agents/{version}"))
        );
        assert!(with(None, None).is_err());
    }
}
