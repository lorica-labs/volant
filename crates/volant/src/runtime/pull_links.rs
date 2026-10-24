// SPDX-License-Identifier: GPL-3.0-or-later
//! The private directory `ansible-pull` runs its children from: it starts `ansible` and
//! `ansible-playbook` next to its own `sys.argv[0]`, so there they are links to this binary,
//! which answers to those names.

#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;

/// The programs `ansible-pull` starts from the directory of its own path.
#[cfg(unix)]
const CHILDREN: [&str; 2] = ["ansible", "ansible-playbook"];

/// `<cache>/pull`, see [`prepare`], for the cache of this user and the running binary.
#[cfg(unix)]
pub(crate) fn dir() -> Result<PathBuf, String> {
    let root = crate::agent::embedded::cache_root().ok_or(
        "neither XDG_CACHE_HOME nor HOME names an absolute directory for the links ansible-pull          needs: set HOME",
    )?;
    let volant = std::env::current_exe()
        .map_err(|e| format!("cannot find the volant binary for ansible-pull's links: {e}"))?;
    prepare(&root, &volant)
}

/// `<root>/pull`, created once with mode 0700, holding `ansible` and `ansible-playbook` links
/// to `volant`; a link that points elsewhere, because the binary moved, is replaced. `root` and
/// `pull` must each be a real directory, owned by this user and writable by no one else: in a
/// cache another user can write to, a link in their place would make `ansible-pull` run their
/// programs. Returns the directory.
#[cfg(unix)]
fn prepare(root: &Path, volant: &Path) -> Result<PathBuf, String> {
    use crate::agent::embedded::{check_private, create_private};

    if !volant.exists() {
        return Err(format!(
            "the volant binary {} is gone, perhaps replaced while this ran: start volant again",
            volant.display()
        ));
    }
    let dir = root.join("pull");
    for place in [root, dir.as_path()] {
        let failed = |e: io::Error| format!("cannot use {} for ansible-pull: {e}", place.display());
        create_private(place).map_err(failed)?;
        if place == root {
            drop_group_write(place).map_err(failed)?;
        }
        check_private(place).map_err(failed)?;
    }
    if let Some(parent) = root.parent() {
        check_parent(parent)?;
    }
    for name in CHILDREN {
        link(&dir.join(name), volant)
            .map_err(|e| format!("cannot link {} in {}: {e}", name, dir.display()))?;
    }
    Ok(dir)
}

#[cfg(not(unix))]
pub(crate) fn dir() -> Result<PathBuf, String> {
    Err("volant pull is not supported on this system yet".to_owned())
}

/// Refuses the directory that holds `root` unless it is this user's or root's, and, when others
/// can write it, has the sticky bit: without it they could rename a checked directory away and
/// put their own in its place before `ansible-pull` runs the programs in it. The rule OpenSSH
/// applies to its paths, including its exception for a user-private group.
#[cfg(unix)]
fn check_parent(parent: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt as _;

    let meta = std::fs::metadata(parent)
        .map_err(|e| format!("cannot use {} for ansible-pull: {e}", parent.display()))?;
    // SAFETY: `geteuid` and `getegid` read the calling process's own credentials.
    let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let private = meta.gid() == egid && private_group(egid);
    let mode = meta.mode() & 0o7777;
    if parent_is_safe(meta.uid(), euid, mode, private) {
        return Ok(());
    }
    Err(format!(
        "{} (owner uid {}, mode {mode:o}) is not safe to keep the links of ansible-pull in,          because others could replace what is inside it: set XDG_CACHE_HOME to a directory          only you can write",
        parent.display(),
        meta.uid()
    ))
}

/// Whether a directory of `owner` and `mode` may hold the links: its owner is the user or root,
/// and anyone who can write it is stopped by the sticky bit, or is the user's own private group.
#[cfg(unix)]
fn parent_is_safe(owner: u32, euid: u32, mode: u32, private_group: bool) -> bool {
    let sticky = mode & 0o1000 != 0;
    let others = mode & 0o002 != 0;
    let group = mode & 0o020 != 0;
    (owner == euid || owner == 0) && (sticky || (!others && (!group || private_group)))
}

/// Whether `gid` is a group with no member listed: the group a user gets of their own. Accounts
/// that only have it as their primary group are not in the list, so a shared primary group is
/// not seen here; that is the cost of not walking the whole password database.
#[cfg(unix)]
fn private_group(gid: libc::gid_t) -> bool {
    let mut group = std::mem::MaybeUninit::<libc::group>::uninit();
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    let mut found: *mut libc::group = std::ptr::null_mut();
    // SAFETY: `group`, `buffer` and `found` are valid for the call, which fills the first two
    // and sets the third to the entry or null.
    let status = unsafe {
        libc::getgrgid_r(
            gid,
            group.as_mut_ptr(),
            buffer.as_mut_ptr(),
            buffer.len(),
            &raw mut found,
        )
    };
    // SAFETY: on success `found` points at `group`, whose `gr_mem` is a null-terminated list.
    status == 0
        && !found.is_null()
        && unsafe { (*found).gr_mem.is_null() || (*(*found).gr_mem).is_null() }
}

/// Takes group and other write off a directory this user owns, which other code of Volant may
/// have made with the umask's default (`0775` under a umask of 002). A link, or a directory of
/// someone else, is left for the check that follows to refuse.
#[cfg(unix)]
fn drop_group_write(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let meta = std::fs::symlink_metadata(dir)?;
    // SAFETY: `geteuid` reads the calling process's own credentials and cannot fail.
    let mine = meta.uid() == unsafe { libc::geteuid() };
    if meta.is_dir() && mine && meta.mode() & 0o022 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(meta.mode() & 0o755))?;
    }
    Ok(())
}

/// `path` as a symbolic link to `target`, written under a temporary name and renamed into place
/// so two runs at once both end with a whole link.
#[cfg(unix)]
fn link(path: &Path, target: &Path) -> io::Result<()> {
    if std::fs::read_link(path).is_ok_and(|current| current == target) {
        return Ok(());
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(all(test, unix))]
mod tests;
