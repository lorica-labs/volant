// SPDX-License-Identifier: GPL-3.0-or-later
//! The response cache of a Galaxy server, `<GALAXY_CACHE_DIR>/api.json`, as ansible-galaxy
//! 2.19.12 keeps it (`_load_cache`, `_set_cache`).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

/// The response cache file, `<GALAXY_CACHE_DIR>/api.json` (`_load_cache`, `_set_cache`).
pub(super) struct Cache {
    path: PathBuf,
    data: Map<String, Value>,
}

impl Cache {
    pub(super) fn open(dir: &Path, clear: bool, verbose: u8) -> Option<Self> {
        make_private_dir(dir);
        let path = dir.join("api.json");
        if clear && path.exists() {
            if verbose >= 4 {
                println!("Clearing cache file ({})", path.display());
            }
            let _ = std::fs::remove_file(&path);
        }
        if !path.is_file() {
            if verbose >= 4 {
                println!(
                    "Creating Galaxy API response cache file at '{}'",
                    path.display()
                );
            }
            let _ = write_private(&path, b"");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if std::fs::metadata(&path).is_ok_and(|m| m.permissions().mode() & 0o002 != 0) {
                eprintln!(
                    "[WARNING]: Galaxy cache has world writable access ({}), ignoring it as a cache source.",
                    path.display()
                );
                return None;
            }
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let data = match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) if map.get("version") == Some(&json!(1)) => map,
            _ => {
                if verbose >= 4 {
                    println!(
                        "Galaxy cache file at '{}' has an invalid version, clearing",
                        path.display()
                    );
                }
                let fresh = json!({"version": 1});
                let _ = std::fs::write(&path, fresh.to_string());
                fresh.as_object().cloned().unwrap_or_default()
            }
        };
        Some(Self { path, data })
    }

    /// The entries of one server, `host:port`.
    pub(super) fn server(&mut self, id: &str) -> &mut Map<String, Value> {
        let entry = self.data.entry(id).or_insert_with(|| json!({}));
        if !entry.is_object() {
            *entry = json!({});
        }
        entry.as_object_mut().expect("an object")
    }

    pub(super) fn save(&self) {
        let _ = std::fs::write(&self.path, Value::Object(self.data.clone()).to_string());
    }
}

/// A file of mode `0600`.
pub(super) fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)?.write_all(data)
}

/// A directory of mode `0700`, with its parents.
pub(super) fn make_private_dir(dir: &Path) {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    let _ = builder.create(dir);
}

/// The UTC time `ahead` seconds from now, as the cache writes it: `%Y-%m-%dT%H:%M:%SZ`.
pub(super) fn now(ahead: u64) -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        + ahead;
    let (days, rest) = (seconds / 86400, seconds % 86400);
    // Howard Hinnant's civil_from_days.
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}
