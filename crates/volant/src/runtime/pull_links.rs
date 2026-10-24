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
