// SPDX-License-Identifier: GPL-3.0-or-later
//! A cache of apt's own for this connection, on a host whose configuration turns apt's off.
//!
//! Docker's Ubuntu and Debian images set `Dir::Cache::pkgcache ""` and
//! `Dir::Cache::srcpkgcache ""` (`docker-clean`), so every `apt-get`, `apt-cache` and `apt-mark`
//! builds the package cache from the lists again: 1.3 s each on Ubuntu 24.04, where reading a
//! kept cache takes 30 ms. The commands the native runs then name a cache in this connection's
//! directory, which goes when the connection ends. What they print and what they leave on the
//! host are the same: the cache holds what apt would build from the same lists, and apt checks
//! it against them and against dpkg's status on every call.
//!
//! A host that keeps apt's cache, a configuration that turns off one of the two, or a directory
//! short of room for both (a cache apt cannot write is a warning in its output) keeps the
//! commands as the module runs them.

use std::collections::BTreeMap;
use std::path::Path;

use super::super::runtime::{self, Clock, Stop};
use super::status::bin_path;

/// Room left for the two caches, each about as large as the lists: 54 MB on Ubuntu 24.04 with
/// `universe`.
const ROOM: u64 = 512 << 20;

/// The options naming the private cache, or none.
pub(super) fn options(
    env: &BTreeMap<String, String>,
    task_env: &BTreeMap<String, String>,
    clock: Clock,
) -> Result<Vec<String>, Stop> {
    let Some(apt_config) = bin_path(task_env, "apt-config") else {
        return Ok(Vec::new());
    };
    let args = [
        "shell",
        "P",
        "Dir::Cache::pkgcache",
        "S",
        "Dir::Cache::srcpkgcache",
    ];
    let Ok((0, out, _)) = runtime::run_output(env, clock, &apt_config, &args)? else {
        return Ok(Vec::new());
    };
    if !turned_off(&out) {
        return Ok(Vec::new());
    }
    let Ok(dir) = crate::blobs::connection_dir("apt") else {
        return Ok(Vec::new());
    };
    if !has_room(&dir) {
        return Ok(Vec::new());
    }
    let (Some(src), Some(pkg)) = (
        dir.join("srcpkgcache.bin").to_str().map(str::to_string),
        dir.join("pkgcache.bin").to_str().map(str::to_string),
    ) else {
        return Ok(Vec::new());
    };
    Ok(vec![
        "-o".into(),
        format!("Dir::Cache::srcpkgcache={src}"),
        "-o".into(),
        format!("Dir::Cache::pkgcache={pkg}"),
    ])
}

/// Whether `apt-config shell` printed both caches as turned off. An unset option is not
/// printed at all, and then apt keeps the cache where it always does.
fn turned_off(out: &str) -> bool {
    let mut lines: Vec<&str> = out.lines().collect();
    lines.sort_unstable();
    lines == ["P=''", "S=''"]
}

fn has_room(dir: &Path) -> bool {
    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    // SAFETY: `statvfs` only writes into `stat`, which is large enough for it.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `stat` is writable.
    if unsafe { libc::statvfs(path.as_ptr(), &raw mut stat) } != 0 {
        return false;
    }
    // `u32` blocks on macOS, `u64` on Linux.
    u128::from(stat.f_bavail) * u128::from(stat.f_frsize) >= u128::from(ROOM)
}

#[cfg(test)]
mod tests {
    use super::turned_off;

    #[test]
    fn only_both_caches_turned_off_count() {
        assert!(turned_off("P=''\nS=''\n"));
        assert!(!turned_off("P='/var/cache/apt/pkgcache.bin'\nS=''\n"));
        assert!(!turned_off("S=''\n"));
        assert!(!turned_off(""));
    }
}
