// SPDX-License-Identifier: GPL-3.0-or-later
//! The module's `PolicyRcD`: for the length of `apt-get install`, `/usr/sbin/policy-rc.d` is a
//! script that exits with `policy_rc_d`, so a package's maintainer scripts start no service.
//! A file already there is moved into a temporary directory first and put back after.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use super::super::runtime::{Root, Stop};

const POLICY: &str = "/usr/sbin/policy-rc.d";

/// The script in place, and where the file it replaced waits.
pub(super) struct Policy {
    backup: Option<PathBuf>,
}

/// Before the install. `Ok(Err)` is the module's `fail_json` message.
pub(super) fn enter(
    code: i64,
    root: &Root,
    task_env: &BTreeMap<String, String>,
) -> Result<Result<Policy, String>, Stop> {
    let policy = root.path(POLICY);
    let backup = if policy.exists() {
        Some(temp_dir(root, task_env)?)
    } else {
        None
    };
    if let Some(dir) = &backup
        && fs::rename(&policy, dir.join("policy-rc.d")).is_err()
    {
        let _ = fs::remove_dir(dir);
        // `shutil.move` copies across file systems, which this does not.
        return Err("policy-rc.d is on another file system than the temporary directory".into());
    }
    let written = fs::write(&policy, format!("#!/bin/sh\nexit {code}\n"))
        .and_then(|()| fs::set_permissions(&policy, fs::Permissions::from_mode(0o755)));
    if written.is_err() {
        return Ok(Err(format!("Failed to create or chmod {POLICY}")));
    }
    Ok(Ok(Policy { backup }))
}

/// After the install, whatever it did. `Err` is the module's `fail_json` message.
pub(super) fn exit(policy: Policy, root: &Root) -> Result<(), String> {
    match policy.backup {
        Some(dir) => {
            let kept = dir.join("policy-rc.d");
            fs::rename(&kept, root.path(POLICY))
                .and_then(|()| fs::remove_dir(&dir))
                .map_err(|_| format!("Fail to move back {} to {POLICY}", kept.display()))
        }
        None => fs::remove_file(root.path(POLICY))
            .map_err(|_| format!("Fail to remove {POLICY} (after package manipulation)")),
    }
}

/// `tempfile.mkdtemp(prefix="ansible")`: under `TMPDIR`, else `/tmp`.
fn temp_dir(root: &Root, task_env: &BTreeMap<String, String>) -> Result<PathBuf, Stop> {
    let base = task_env
        .get("TMPDIR")
        .filter(|dir| dir.starts_with('/'))
        .map_or_else(|| root.path("/tmp"), |dir| root.path(dir));
    let template = CString::new(base.join("ansibleXXXXXX").as_os_str().as_bytes())
        .map_err(|_| "a temporary directory path holds a NUL")?;
    let raw = template.into_raw();
    // SAFETY: `raw` is a NUL-terminated buffer this function owns, which `mkdtemp` rewrites in
    // place without changing its length.
    let made = unsafe { libc::mkdtemp(raw) };
    // SAFETY: `raw` came from `CString::into_raw` just above and is taken back once.
    let path = unsafe { CString::from_raw(raw) };
    if made.is_null() {
        return Err("no temporary directory for policy-rc.d".into());
    }
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(path.as_bytes())))
}
