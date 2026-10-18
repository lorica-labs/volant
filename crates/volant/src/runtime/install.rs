// SPDX-License-Identifier: GPL-3.0-or-later
//! Installing a runtime: the pinned Python unpacked and ansible-core installed into it by its own
//! pip, from wheels checked against the manifest. The work happens in a directory next to the
//! target, under a lock, and is renamed into place once the runtime imports the pinned
//! ansible-core, so a runtime directory is always whole.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::fetch::{extract, fetch};
use super::host_triple;
use super::manifest::{Manifest, Platform};

/// The file that says what a runtime holds. A directory without it is not a runtime.
pub(super) const RECORD: &str = "RUNTIME.json";

/// How long a run waits for another one that is installing the same runtime.
pub(super) const WAIT: Duration = Duration::from_secs(15 * 60);

/// A runtime installed under the runtimes directory.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Installed {
    pub(crate) id: String,
    pub(crate) dir: PathBuf,
    /// `<dir>/python/bin/python3`.
    pub(crate) python: PathBuf,
}

/// The runtime `id` under `runtimes`, when it is installed.
pub(crate) fn installed(runtimes: &Path, id: &str) -> Option<Installed> {
    let dir = runtimes.join(id);
    let python = interpreter(&dir);
    (dir.join(RECORD).is_file() && python.is_file()).then(|| Installed {
        id: id.to_owned(),
        dir,
        python,
    })
}

/// The runtimes installed under `runtimes`, by name.
pub(crate) fn list(runtimes: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(runtimes) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|id| !id.starts_with('.') && installed(runtimes, id).is_some())
        .collect();
    ids.sort();
    ids
}

pub(super) fn interpreter(dir: &Path) -> PathBuf {
    dir.join("python").join("bin").join("python3")
}

/// Installs the runtime `manifest` pins for this host under `runtimes`, unless it is there
/// already, and returns it. Says so on stderr when it installs.
pub(crate) fn install(runtimes: &Path, manifest: &Manifest) -> Result<Installed, String> {
    let id = manifest.id();
    if let Some(done) = installed(runtimes, &id) {
        return Ok(done);
    }
    let triple = host_triple()?;
    let platform = manifest.platform(&triple)?;
    fs::create_dir_all(runtimes).map_err(|e| {
        format!(
            "cannot create {}: {e}; check the free disk space and the permissions",
            runtimes.display()
        )
    })?;
    let _lock = Lock::acquire(&lock_path(runtimes, &id), WAIT)?;
    if let Some(done) = installed(runtimes, &id) {
        return Ok(done);
    }
    sweep_partials(runtimes, &id);
    eprintln!("Installing Volant's Python runtime ({id}) ...");
    let work = tempfile::Builder::new()
        .prefix(&partial_prefix(&id))
        .tempdir_in(runtimes)
        .map_err(|e| {
            format!(
                "cannot create a directory in {}: {e}; check the free disk space",
                runtimes.display()
            )
        })?;
    let downloads = work.path().join("downloads");
    let stage = work.path().join("runtime");
    let tarball = downloads.join(platform.python.filename()?);
    fetch(&platform.python.url, &platform.python.sha256, &tarball)?;
    let python = extract(&tarball, &stage)?;
    install_wheels(&python, platform, &downloads.join("wheels"))?;
    probe(&python, &manifest.ansible_core)?;
    write_record(&stage, &id, &triple, manifest, platform)?;
    let dir = runtimes.join(&id);
    if let Err(e) = fs::rename(&stage, &dir) {
        // A run that took over this one's lock may have finished first: its runtime is as good.
        return installed(runtimes, &id).ok_or_else(|| {
            format!(
                "cannot move the runtime into {}: {e}; check the free disk space, then run \
                 `volant runtime install` again",
                dir.display()
            )
        });
    }
    Ok(Installed {
        id,
        python: interpreter(&dir),
        dir,
    })
}

/// A command for the runtime's interpreter with only the environment it needs: no `PYTHONPATH`,
/// no user site, no `PIP_*` setting from the user, so nothing outside the runtime changes what
/// it loads or installs.
pub(crate) fn python_command(python: &Path) -> Command {
    let mut command = Command::new(python);
    command
        .env_clear()
        .env("PYTHONNOUSERSITE", "1")
        .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
        .env("PIP_NO_INPUT", "1")
        .stdin(Stdio::null());
    for var in ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE"] {
        if let Some(value) = std::env::var_os(var) {
            command.env(var, value);
        }
    }
    command
}

/// Downloads every wheel the platform pins into `wheels`, each checked against its hash, and
/// installs them with pip from that directory alone, which checks the hashes again.
fn install_wheels(python: &Path, platform: &Platform, wheels: &Path) -> Result<(), String> {
    let mut requirements = Vec::new();
    for wheel in &platform.wheels {
        let file = &wheel.file;
        fetch(&file.url, &file.sha256, &wheels.join(file.filename()?))?;
        requirements.push(format!(
            "{}=={} --hash=sha256:{}",
            wheel.name, wheel.version, file.sha256
        ));
    }
    let list = wheels.join("requirements.txt");
    fs::write(&list, requirements.join("\n") + "\n").map_err(|e| {
        format!(
            "cannot write {}: {e}; check the free disk space",
            list.display()
        )
    })?;
    let output = python_command(python)
        .args(["-I", "-m", "pip", "install", "--isolated", "--no-cache-dir"])
        .args(["--no-index", "--find-links"])
        .arg(wheels)
        .args(["--require-hashes", "--only-binary", ":all:"])
        .args(["--no-warn-script-location", "--quiet", "--requirement"])
        .arg(&list)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", python.display()))?;
    if !output.status.success() {
        return Err(format!(
            "pip could not install ansible-core into the runtime ({}): {}; check the free disk \
             space, then run `volant runtime install` again",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Checks that the runtime imports the pinned ansible-core.
pub(super) fn probe(python: &Path, version: &str) -> Result<(), String> {
    let output = python_command(python)
        .args([
            "-I",
            "-c",
            "import sys, ansible; sys.stdout.write(ansible.__version__)",
        ])
        .output()
        .map_err(|e| format!("cannot run {}: {e}", python.display()))?;
    let found = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || found != version {
        return Err(format!(
            "the runtime does not import ansible-core {version} after its install (it found \
             {found:?}: {})",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

fn write_record(
    stage: &Path,
    id: &str,
    triple: &str,
    manifest: &Manifest,
    platform: &Platform,
) -> Result<(), String> {
    let installed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let wheels: Vec<_> = platform
        .wheels
        .iter()
        .map(|wheel| {
            serde_json::json!({
                "name": wheel.name,
                "version": wheel.version,
                "sha256": wheel.file.sha256,
            })
        })
        .collect();
    let record = serde_json::json!({
        "id": id,
        "triple": triple,
        "python": manifest.python,
        "python_release": manifest.python_release,
        "python_sha256": platform.python.sha256,
        "ansible_core": manifest.ansible_core,
        "wheels": wheels,
        "installed_at": installed_at,
    });
    let path = stage.join(RECORD);
    fs::write(&path, format!("{record:#}\n")).map_err(|e| {
        format!(
            "cannot write {}: {e}; check the free disk space",
            path.display()
        )
    })
}

/// Deletes the runtime `id` under `runtimes`, and says whether it was there.
pub(crate) fn remove(runtimes: &Path, id: &str) -> Result<bool, String> {
    if id.is_empty() || id.starts_with('.') || id.contains(['/', '\\']) {
        return Err(format!("{id:?} is not the name of a runtime"));
    }
    let dir = runtimes.join(id);
    if !dir.exists() {
        return Ok(false);
    }
    let _lock = Lock::acquire(&lock_path(runtimes, id), WAIT)?;
    fs::remove_dir_all(&dir).map_err(|e| format!("cannot delete {}: {e}", dir.display()))?;
    Ok(true)
}

/// Deletes the whole runtimes directory, and returns the runtimes it held.
pub(crate) fn remove_all(runtimes: &Path) -> Result<Vec<String>, String> {
    let ids = list(runtimes);
    match fs::remove_dir_all(runtimes) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(format!("cannot delete {}: {e}", runtimes.display()))
        }
        _ => Ok(ids),
    }
}

pub(super) fn lock_path(runtimes: &Path, id: &str) -> PathBuf {
    runtimes.join(format!(".{id}.lock"))
}

pub(super) fn partial_prefix(id: &str) -> String {
    format!(".{id}.partial-")
}

/// Deletes what earlier installs of `id` left when they were killed. Only the lock holder calls
/// it, so none of them is still in use. One that cannot be deleted stays; it is tried again next
/// time, and the new install works in a directory of its own.
pub(super) fn sweep_partials(runtimes: &Path, id: &str) {
    let prefix = partial_prefix(id);
    let Ok(entries) = fs::read_dir(runtimes) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(&prefix))
        {
            _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// The install lock: a file holding the pid of the run that installs. It is created whole (the
/// pid is written to a temporary file that is then hard-linked to the lock's name, which fails
/// when the name exists), so a reader never sees it empty. A lock whose pid is dead is taken
/// over. Two runs taking over the same dead lock at the same instant can both go on; each then
/// works in its own directory and the second rename finds the first runtime in place.
pub(super) struct Lock {
    path: PathBuf,
}

impl Lock {
    pub(super) fn acquire(path: &Path, wait: Duration) -> Result<Self, String> {
        let start = Instant::now();
        let mut told = false;
        loop {
            match Self::create(path) {
                Ok(()) => {
                    return Ok(Self {
                        path: path.to_owned(),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(format!(
                        "cannot create the install lock {}: {e}; check the free disk space and \
                         the permissions",
                        path.display()
                    ));
                }
            }
            let holder = fs::read_to_string(path)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok());
            match holder {
                Some(pid) if alive(pid) => {
                    if start.elapsed() >= wait {
                        return Err(format!(
                            "another Volant run (pid {pid}) is still installing the Python \
                             runtime: wait for it to finish, or delete {} if that process is \
                             not Volant",
                            path.display()
                        ));
                    }
                    if !told {
                        eprintln!(
                            "Waiting for another Volant run (pid {pid}) to finish installing \
                             the Python runtime ..."
                        );
                        told = true;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                // Its holder died, or it names none: the next turn of the loop creates it again.
                _ => _ = fs::remove_file(path),
            }
        }
    }

    fn create(path: &Path) -> io::Result<()> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let mut pid = tempfile::Builder::new()
            .prefix(path.file_name().unwrap_or(OsStr::new("lock")))
            .tempfile_in(dir)?;
        writeln!(pid, "{}", std::process::id())?;
        fs::hard_link(pid.path(), path)
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        _ = fs::remove_file(&self.path);
    }
}

/// Whether a process `pid` exists. One that belongs to another user counts.
#[cfg(unix)]
fn alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 sends nothing; `kill` only checks that the process exists.
    let sent = unsafe { libc::kill(pid, 0) };
    sent == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn alive(_: i32) -> bool {
    true
}

#[cfg(test)]
mod tests;
