// SPDX-License-Identifier: GPL-3.0-or-later
//! The connection password, handed to `ssh` through `SSH_ASKPASS`. The run writes the password
//! to a private file; every `ssh` it starts is told to run this program as its askpass, which
//! prints the file's text. The password is never in an argument list, in the environment of
//! the program or in anything sent to the host: only the file's path is. A password a host
//! names in `ansible_password` or one of its aliases takes the same way, in a file of its own
//! in the same directory. A plugin the controller's helper runs reads the run's password off its
//! play context, as the reference's plugins do.
//!
//! `ssh` closes every descriptor above 2 before it starts its askpass, so a descriptor the run
//! holds cannot carry the password and a path has to. The files sit in a directory of their
//! own, mode 0700, named after the process. The run removes the directory when it ends, when it
//! unwinds from a panic, when it stops on SIGINT or SIGTERM, and before it dies of SIGHUP. A
//! kill by SIGKILL, or by the out-of-memory killer, cannot run any code: the directory stays,
//! and the next run of the same user removes it, because the process in its name is gone and
//! no run holds the lock that the directory's owner took on it.

use std::collections::HashMap;
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

/// The environment variable that turns this program into an askpass for one `ssh` call.
const FILE_ENV: &str = "VOLANT_ASKPASS_FILE";

/// The start of the name of a run's private directory: `volant-askpass-<pid>-<random>`.
const DIR_PREFIX: &str = "volant-askpass-";

/// The file that holds the password of `-k` or `--connection-password-file`.
static FILE: OnceLock<PathBuf> = OnceLock::new();

/// The run's private directory, made the first time a password needs a file, and removed with
/// [`Installed`].
static PRIVATE: Mutex<Option<Private>> = Mutex::new(None);

/// The password files, by password, then the directory that holds them and the lock on it. The
/// fields drop in that order: the files go before their directory.
struct Private {
    files: HashMap<String, tempfile::TempPath>,
    dir: tempfile::TempDir,
    _lock: std::fs::File,
}

/// Which password an `ssh` call offers. `Debug` is written by hand so a `{:?}` of a value that
/// holds the password prints a placeholder.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Source<'a> {
    /// The one the command line gave, if it gave one.
    Run,
    /// The one a host variable gave. An empty one offers nothing, as in the reference: the
    /// variable is set, so no other password is tried in its place.
    Variable(&'a str),
}

impl std::fmt::Debug for Source<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Run => f.write_str("Run"),
            Source::Variable(_) => f.write_str("Variable(<redacted>)"),
        }
    }
}

/// Keeps the password directory and its files alive. They are removed when this is dropped.
pub struct Installed(());

impl Drop for Installed {
    fn drop(&mut self) {
        PRIVATE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}

/// Stores the connection password of the command line, if there is one, in a file only this
/// user can read, and makes every `ssh` the run starts ask this program for it. The value that
/// comes back has to live to the end of the run: it also removes the files of the passwords
/// that hosts name in a variable.
pub fn install(password: Option<&str>) -> std::io::Result<Installed> {
    let installed = Installed(());
    if let Some(password) = password {
        let file = file_of(password)?;
        FILE.set(file)
            .map_err(|_| std::io::Error::other("a connection password is already installed"))?;
    }
    Ok(installed)
}

/// The file that holds `password`, made with the run's directory on first use.
fn file_of(password: &str) -> std::io::Result<PathBuf> {
    use std::io::Write as _;
    let mut private = PRIVATE.lock().unwrap_or_else(PoisonError::into_inner);
    let private = if let Some(private) = private.as_mut() {
        private
    } else {
        let (dir, lock) = private_dir()?;
        private.insert(Private {
            files: HashMap::new(),
            dir,
            _lock: lock,
        })
    };
    if let Some(path) = private.files.get(password) {
        return Ok(path.to_path_buf());
    }
    let mut file = tempfile::Builder::new()
        .prefix("password-")
        .tempfile_in(private.dir.path())?;
    file.write_all(password.as_bytes())?;
    file.flush()?;
    let path = file.into_temp_path();
    let shown = path.to_path_buf();
    private.files.insert(password.to_string(), path);
    Ok(shown)
}

/// The directories a run's own directory can sit in: the user's runtime directory when it is
/// private, then the temporary directory.
fn bases() -> Vec<PathBuf> {
    #[cfg(unix)]
    let runtime = {
        let uid = effective_uid();
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .filter(|dir| {
                std::fs::symlink_metadata(dir)
                    .is_ok_and(|m| m.is_dir() && m.uid() == uid && m.mode() & 0o077 == 0)
            })
    };
    #[cfg(not(unix))]
    let runtime: Option<PathBuf> = None;
    runtime.into_iter().chain([std::env::temp_dir()]).collect()
}

#[cfg(unix)]
pub(crate) fn effective_uid() -> u32 {
    // SAFETY: `geteuid` takes no argument and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
pub(crate) fn effective_uid() -> u32 {
    0
}

/// A new directory, mode 0700 and named after this process, in the first base that takes it, and
/// the open descriptor that holds an exclusive lock on it. The kernel drops the lock when the
/// process ends, whatever ended it, so a directory nobody locks has no run behind it.
fn private_dir() -> std::io::Result<(tempfile::TempDir, std::fs::File)> {
    let prefix = format!("{DIR_PREFIX}{}-", std::process::id());
    let mut last = std::io::Error::other("no directory to keep the password file in");
    for base in bases() {
        let mut builder = tempfile::Builder::new();
        builder.prefix(&prefix);
        // Mode 0700 from the `mkdir` itself: the default is the umask's, 0755 on most accounts.
        #[cfg(unix)]
        builder.permissions(std::fs::Permissions::from_mode(0o700));
        let made = builder.tempdir_in(base);
        match made {
            Ok(dir) => {
                let lock = std::fs::File::open(dir.path())?;
                // A file system with no locks leaves the sweep to the process number alone.
                let _ = lock.try_lock();
                return Ok((dir, lock));
            }
            Err(err) => last = err,
        }
    }
    Err(last)
}

/// Removes the directories that runs of this user left when they were killed: one whose name
/// carries the number of a process that is gone. Called at the start of a run.
pub fn remove_stale() {
    let uid = effective_uid();
    for base in bases() {
        remove_stale_in(&base, uid);
    }
}

/// [`remove_stale`] in one directory. Only a real directory, owned by `uid`, with mode 0700,
/// that no run holds the lock of goes: a symlink, a file, another user's directory, a directory
/// of another mode or a directory of a live run stays. The lock is what tells a live run, since
/// a process number means nothing to a run in another PID namespace on the same directory.
fn remove_stale_in(base: &Path, uid: u32) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let pid = entry.file_name().to_str().and_then(owner_pid);
        if pid.is_none_or(crate::runtime::install::alive) {
            continue;
        }
        let path = entry.path();
        // The lock stays held while the directory goes, so no other sweep judges it meanwhile.
        if let Some(_held) = claim_stale(&path, uid) {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// The open, locked directory at `path` when it is a real directory of `uid`, mode 0700, that no
/// run holds the lock of. It is opened without following a link, and judged by what the
/// descriptor says.
#[cfg(unix)]
fn claim_stale(path: &Path, uid: u32) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let Ok(dir) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
    else {
        return None;
    };
    let mine = dir
        .metadata()
        .is_ok_and(|m| m.is_dir() && m.uid() == uid && m.mode() & 0o7777 == 0o700);
    // A file system with no locks leaves the judgement to the process number alone.
    (mine && !matches!(dir.try_lock(), Err(std::fs::TryLockError::WouldBlock))).then_some(dir)
}

/// Without owners and modes to judge by, nothing is swept.
#[cfg(not(unix))]
fn claim_stale(_: &Path, _: u32) -> Option<std::fs::File> {
    None
}

/// Removes the run's password directory at once. For a stop that runs no destructor: the
/// process is about to die of a signal.
pub fn remove_now() {
    if let Some(private) = PRIVATE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
    {
        let _ = std::fs::remove_dir_all(private.dir.path());
    }
}

/// The process number in the name of a run's directory.
fn owner_pid(name: &str) -> Option<i32> {
    let (pid, _) = name.strip_prefix(DIR_PREFIX)?.split_once('-')?;
    pid.parse().ok().filter(|pid| *pid > 0)
}

/// The password of `-k` or `--connection-password-file`, as a plugin's play context carries it.
pub fn run_password() -> Option<String> {
    std::fs::read_to_string(FILE.get()?).ok()
}

/// Whether an `ssh` call with this source has a password to offer.
pub fn offers(source: Source<'_>) -> bool {
    match source {
        Source::Run => FILE.get().is_some(),
        Source::Variable(password) => !password.is_empty(),
    }
}

/// The environment an `ssh` call of this run is started with, empty when it has no password.
pub fn environment(source: Source<'_>) -> std::io::Result<Vec<(&'static str, OsString)>> {
    let file = match source {
        Source::Run => FILE.get().cloned(),
        Source::Variable("") => None,
        Source::Variable(password) => Some(file_of(password)?),
    };
    let (Some(file), Ok(me)) = (file, std::env::current_exe()) else {
        return Ok(Vec::new());
    };
    Ok(vec![
        ("SSH_ASKPASS", me.into_os_string()),
        ("SSH_ASKPASS_REQUIRE", "force".into()),
        (FILE_ENV, file.into_os_string()),
    ])
}

/// What `ssh` is told when it asks `prompt`: the password for a prompt that ends in
/// `password:`, nothing for any other question. A host-key confirmation or a key passphrase
/// may name a file or a host with "password" in it and must never get the password.
fn reply(file: &Path, prompt: &str) -> Option<Vec<u8>> {
    let lower = prompt.trim().to_ascii_lowercase();
    if lower.contains("passphrase") || lower.contains("yes/no") || !lower.ends_with("password:") {
        return None;
    }
    let mut text = std::fs::read(file).ok()?;
    text.push(b'\n');
    Some(text)
}

/// When `ssh` started this program as its askpass, answers and exits; otherwise returns.
pub fn answer_if_asked() {
    use std::io::Write as _;
    let Some(file) = std::env::var_os(FILE_ENV) else {
        return;
    };
    let prompt = std::env::args().nth(1).unwrap_or_default();
    let code = match reply(Path::new(&file), &prompt) {
        Some(text) if std::io::stdout().write_all(&text).is_ok() => 0,
        _ => 1,
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests;
