// SPDX-License-Identifier: GPL-3.0-or-later
//! What the host's files say: dpkg's status, the cache stamp, the system Python.

use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::super::runtime::Root;
use super::{LISTS, STAMP};

/// One paragraph of a dpkg-style file, the fields the fast path reads.
#[derive(Debug, Default)]
pub(super) struct Stanza<'a> {
    pub(super) status: &'a str,
    pub(super) architecture: &'a str,
    pub(super) auto_installed: bool,
}

impl Stanza<'_> {
    /// The third word of `Status:`, what `current_state` reports.
    pub(super) fn current(&self) -> &str {
        self.status.split_whitespace().nth(2).unwrap_or_default()
    }
}

/// Every paragraph by its `Package:`.
pub(super) fn stanzas(text: &str) -> HashMap<&str, Vec<Stanza<'_>>> {
    let mut map: HashMap<&str, Vec<Stanza>> = HashMap::new();
    for paragraph in text.split("\n\n") {
        let mut package = None;
        let mut stanza = Stanza::default();
        for line in paragraph.lines() {
            let Some((field, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match field {
                "Package" => package = Some(value),
                "Status" => stanza.status = value,
                "Architecture" => stanza.architecture = value,
                "Auto-Installed" => stanza.auto_installed = value == "1",
                _ => {}
            }
        }
        if let Some(package) = package {
            map.entry(package).or_default().push(stanza);
        }
    }
    map
}

/// One paragraph for `name`, and it reads `install ok installed` (any other want or state,
/// `hold` and `deinstall` included, is left to the module), on `arch` or `all` when one is given.
pub(super) fn is_installed(
    status: &HashMap<&str, Vec<Stanza>>,
    name: &str,
    arch: Option<&str>,
) -> bool {
    match status.get(name).map(Vec::as_slice) {
        Some([entry]) => {
            entry
                .status
                .split_whitespace()
                .eq(["install", "ok", "installed"])
                && arch.is_none_or(|arch| entry.architecture == arch || entry.architecture == "all")
        }
        _ => false,
    }
}

/// Whether `/usr/bin/python3` (or `/usr/bin/python`) can import python-apt, which is where the
/// module goes when its own interpreter cannot: a `python3.N` whose `apt_pkg` extension for that
/// version is in `dist-packages`. A system python of another version (the known
/// `No module named 'apt_pkg'` breakage), or one this cannot name, is not.
pub(super) fn system_python_has_apt(root: &Root) -> bool {
    let packages = root.path("/usr/lib/python3/dist-packages");
    if !packages.join("apt/__init__.py").is_file() {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(&packages) else {
        return false;
    };
    let extensions: Vec<String> = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("apt_pkg.cpython-3") && name.ends_with(".so"))
        .collect();
    ["/usr/bin/python3", "/usr/bin/python"]
        .iter()
        .any(|python| {
            let Ok(real) = std::fs::canonicalize(root.path(python)) else {
                return false;
            };
            let Some(minor) = real
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix("python3."))
                .filter(|minor| !minor.is_empty() && minor.bytes().all(|b| b.is_ascii_digit()))
            else {
                return false;
            };
            let tag = format!("apt_pkg.cpython-3{minor}-");
            extensions.iter().any(|name| name.starts_with(&tag))
        })
}

/// A file's text, `None` when it does not exist.
pub(super) fn read(root: &Root, path: &str) -> Result<Option<String>, String> {
    match std::fs::read_to_string(root.path(path)) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("{path}: {err}")),
    }
}

/// `get_cache_mtime`: the stamp's, else the lists directory's, else none.
pub(super) fn cache_mtime(root: &Root) -> Result<Option<(i64, i64)>, String> {
    for path in [STAMP, LISTS] {
        match std::fs::metadata(root.path(path)) {
            Ok(meta) => return Ok(Some((meta.mtime(), meta.mtime_nsec()))),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("{path}: {err}")),
        }
    }
    Ok(None)
}

/// `datetime.fromtimestamp(st_mtime)` as seconds and microseconds: `st_mtime` is the float
/// `sec + nsec * 1e-9`, and the microseconds are rounded half to even, carrying into the second.
/// `cache_update_time` is then that second (`mktime` of the tuple, which drops the microseconds):
/// neither the float's floor nor its rounding.
pub(super) fn from_timestamp(sec: i64, nsec: i64) -> (i64, i64) {
    let float = sec as f64 + nsec as f64 * 1e-9;
    let whole = float.trunc();
    let micros = ((float - whole) * 1e6).round_ties_even();
    let carry = micros >= 1e6;
    let micros = if carry { micros - 1e6 } else { micros };
    (whole as i64 + i64::from(carry), micros as i64)
}

/// The local zone's offset from UTC at `seconds`, as the C library reads it.
#[cfg_attr(
    target_env = "musl",
    expect(
        deprecated,
        reason = "libc marks `time_t` deprecated on musl ahead of its 64-bit change"
    )
)]
pub(super) fn utc_offset(seconds: i64) -> Option<libc::c_long> {
    let time = libc::time_t::try_from(seconds).ok()?;
    // SAFETY: `tm` is plain data, and `localtime_r` writes it whole or returns null.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let done = unsafe { libc::localtime_r(&raw const time, &raw mut tm) };
    (!done.is_null()).then_some(tm.tm_gmtoff)
}

/// `module.get_bin_path(name)`: the first executable file of that name along the module's
/// `PATH`, with `/sbin`, `/usr/sbin` and `/usr/local/sbin` added when they exist.
pub(super) fn bin_path(task_env: &BTreeMap<String, String>, name: &str) -> Option<PathBuf> {
    let path = task_env
        .get("PATH")
        .cloned()
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default();
    let mut dirs: Vec<&str> = path.split(':').collect();
    for sbin in ["/sbin", "/usr/sbin", "/usr/local/sbin"] {
        if !dirs.contains(&sbin) && Path::new(sbin).exists() {
            dirs.push(sbin);
        }
    }
    dirs.into_iter()
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|path| {
            std::fs::metadata(path)
                .is_ok_and(|meta| !meta.is_dir() && meta.permissions().mode() & 0o111 != 0)
        })
}
