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

/// `<cache>/pull`, created once with mode 0700, holding `ansible` and `ansible-playbook` links
/// to `volant`; a link that points elsewhere, because the binary moved, is replaced. Returns the
/// directory.
#[cfg(unix)]
pub(crate) fn dir() -> Result<PathBuf, String> {
    let root = crate::agent::embedded::cache_root().ok_or(
        "neither XDG_CACHE_HOME nor HOME names an absolute directory for the links ansible-pull \
         needs: set HOME",
    )?;
    let volant = std::env::current_exe()
        .map_err(|e| format!("cannot find the volant binary for ansible-pull's links: {e}"))?;
    let dir = root.join("pull");
    let failed = |e: io::Error| format!("cannot prepare {}: {e}", dir.display());
    crate::agent::embedded::create_private(&dir).map_err(failed)?;
    crate::agent::embedded::check_private(&dir).map_err(failed)?;
    for name in CHILDREN {
        link(&dir.join(name), &volant).map_err(failed)?;
    }
    Ok(dir)
}

#[cfg(not(unix))]
pub(crate) fn dir() -> Result<PathBuf, String> {
    Err("volant pull is not supported on this system yet".to_owned())
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
