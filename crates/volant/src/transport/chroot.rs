// SPDX-License-Identifier: GPL-3.0-or-later
//! The `community.general.chroot` connection: the agent's link through `chroot <dir>`, for a
//! directory tree on the controller, with the options of community.general 11.4.2.
//!
//! The upload is the `stdin` strategy through `chroot <dir> /bin/sh -c`, so the script that
//! checks and writes the agent's directory runs inside the tree: no path it writes can leave it,
//! whatever symbolic links the tree holds.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::choose::remote_tmp_of;
use super::defaults::{ConnectionDefaults, ConnectionFailsTask};
use super::exec::{ExecTarget, Lines};
use super::names::{
    CHROOT_DISABLE_ROOT_CHECK, CHROOT_EXE, CHROOT_EXECUTABLE, PODMAN_HOST, first_set,
};

/// The plugin's full name, which a short `chroot` resolves to.
pub(super) const PLUGIN: &str = "community.general.chroot";

/// What the environment and `ansible.cfg` say for the plugin; a host variable speaks over each.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChrootSettings {
    /// `ANSIBLE_CHROOT_EXE`, else `[chroot_connection] exe`.
    pub exe: Option<String>,
    /// `ANSIBLE_CHROOT_DISABLE_ROOT_CHECK`, else `[chroot_connection] disable_root_check`.
    pub disable_root_check: Option<bool>,
    /// `ANSIBLE_EXECUTABLE`, else `[defaults] executable`.
    pub executable: Option<String>,
}

/// The exec target of a host whose connection is `community.general.chroot`, or the plugin's
/// refusal, which fails the task before anything runs.
pub(super) fn target(
    host_name: &str,
    vars: &Map<String, Value>,
    defaults: &ConnectionDefaults,
) -> anyhow::Result<ExecTarget> {
    let text = |names: &[&str]| {
        first_set(vars, names)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let settings = &defaults.plugins.chroot;
    let dir = text(&PODMAN_HOST).unwrap_or_else(|| host_name.to_string());
    let executable = text(&CHROOT_EXECUTABLE)
        .or_else(|| settings.executable.clone())
        .unwrap_or_else(|| "/bin/sh".to_string());
    let exe = text(&CHROOT_EXE)
        .or_else(|| settings.exe.clone())
        .unwrap_or_else(|| "chroot".to_string());
    let skip_root_check = first_set(vars, &CHROOT_DISABLE_ROOT_CHECK)
        .map(super::docker::flag)
        .or(settings.disable_root_check)
        .unwrap_or(false);
    refusal(Path::new(&dir), skip_root_check).map_err(ConnectionFailsTask::refused)?;
    let found = program(&exe, std::env::var_os("PATH").as_deref());
    let chroot = found
        .as_ref()
        .map_or_else(|| exe.clone(), |path| path.display().to_string());
    Ok(ExecTarget {
        unavailable: found
            .is_none()
            .then(|| format!("{exe} command not found in PATH")),
        copy: None,
        user_probe: None,
        prefix: vec![chroot, dir.clone()],
        name: dir,
        kind: "CHROOT".into(),
        lines: Lines::Chroot,
        shell: executable,
        user: None,
        remote_tmp: remote_tmp_of(host_name, vars, defaults)?,
        connect_timeout: defaults.connect_timeout,
    })
}

/// What the plugin refuses before `chroot` runs, in its order and words: a path that is not a
/// directory, one with no `bin/sh` (measured on 2.19.12, the reference raises the `stat` error
/// rather than the message it means to give), then a controller not running as root.
fn refusal(dir: &Path, skip_root_check: bool) -> Result<(), String> {
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    let shell = dir.join("bin/sh");
    match shell.metadata() {
        Err(_) => {
            return Err(format!(
                "[Errno 2] No such file or directory: '{}'",
                shell.display()
            ));
        }
        Ok(meta) if !super::docker::executable(&meta) => {
            return Err(format!(
                "{} does not look like a chrootable dir (/bin/sh missing)",
                dir.display()
            ));
        }
        Ok(_) => {}
    }
    if !skip_root_check && !running_as_root() {
        return Err("chroot connection requires running as root. You can override this check with the `disable_root_check` option.".to_string());
    }
    Ok(())
}

#[cfg(unix)]
fn running_as_root() -> bool {
    // SAFETY: `geteuid` cannot fail and touches nothing.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
fn running_as_root() -> bool {
    false
}

/// `exe` itself when it is absolute, else the first of that name on `path` that can run.
fn program(exe: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let runnable = |file: &Path| {
        file.metadata()
            .is_ok_and(|meta| meta.is_file() && super::docker::executable(&meta))
    };
    if Path::new(exe).is_absolute() {
        let file = PathBuf::from(exe);
        return runnable(&file).then_some(file);
    }
    std::env::split_paths(path?)
        .map(|dir| dir.join(exe))
        .find(|file| runnable(file))
}

#[cfg(all(test, unix))]
mod tests;
