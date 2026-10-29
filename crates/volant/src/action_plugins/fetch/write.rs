// SPDX-License-Identifier: GPL-3.0-or-later
//! Where a fetched file lands: beside its place first, then renamed over it once its sum is right.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value};
use volant_protocol::TaskResult;
use volant_protocol::encoding::{sha1_hex, sha1_hex_of};

use super::Fetch;
use super::paths::walk;

impl Fetch {
    /// The result when the local file already has the remote sum.
    pub(super) fn unchanged(&self, path: &Path, remote: &str) -> Option<TaskResult> {
        let local = std::fs::File::open(path).and_then(sha1_hex_of).ok();
        if local.as_deref() != Some(remote) {
            return None;
        }
        // Measured: `changed: false`, the local sum, and no remote one.
        let mut out = Map::new();
        out.insert("changed".into(), Value::Bool(false));
        out.insert("checksum".into(), Value::String(remote.to_string()));
        out.insert("dest".into(), Value::String(path.display().to_string()));
        out.insert("file".into(), Value::String(self.source.clone()));
        out.insert("md5sum".into(), Value::Null);
        out.insert("remote_checksum".into(), Value::Null);
        Some(TaskResult(out))
    }

    /// The bytes written next to their place, checked, then renamed over it.
    pub(super) fn write(&self, path: PathBuf, data: &[u8], remote: &str) -> TaskResult {
        let tmp = match self.begin(&path) {
            Ok(tmp) => tmp,
            Err(msg) => return TaskResult::failed_with(msg),
        };
        let fail = |err: std::io::Error| format!("Failed to fetch the file: {err}");
        let written = std::fs::OpenOptions::new()
            .write(true)
            .open(&tmp)
            .and_then(|mut file| file.write_all(data))
            .and_then(|()| std::fs::read(&tmp))
            .map(|b| sha1_hex(&b))
            .map_err(fail);
        match written {
            Ok(new) => self.finish(&path, &tmp, new, remote),
            Err(msg) => {
                let _ = std::fs::remove_file(&tmp);
                TaskResult::failed_with(msg)
            }
        }
    }

    /// The empty file beside `path` its bytes are written to, with the mode of the file it will
    /// replace, once the walk from `dest` found nothing in the way.
    pub(super) fn begin(&self, path: &Path) -> Result<PathBuf, String> {
        // A name of this write's own: two hosts fetching `flat` to one path run at once, each on
        // its own driver, and a shared temporary name would mix their bytes.
        static WRITES: AtomicU64 = AtomicU64::new(0);
        let mut tmp = path.to_path_buf().into_os_string();
        tmp.push(format!(
            ".{}-{}.volant-tmp",
            std::process::id(),
            WRITES.fetch_add(1, Ordering::Relaxed)
        ));
        let tmp = PathBuf::from(tmp);
        // A `dest` a host named is walked from the directory of the list, so no link and no
        // directory another user controls stands between it and the file.
        self.contained().and_then(|anchor| {
            walk(
                anchor.as_deref().unwrap_or(self.walk_root()),
                path,
                true,
                anchor.is_some(),
            )
        })?;
        let fail = |err: std::io::Error| format!("Failed to fetch the file: {err}");
        // The file being replaced keeps its mode, as the reference's write in place does: a
        // `0600` kubeconfig fetched again stays `0600`. `walk` refused a link at `path`.
        let kept = std::fs::symlink_metadata(path)
            .ok()
            .filter(std::fs::Metadata::is_file)
            .map(|m| m.permissions());
        let file = std::fs::File::create_new(&tmp).map_err(fail)?;
        if let Some(kept) = kept
            && let Err(err) = file.set_permissions(kept)
        {
            let _ = std::fs::remove_file(&tmp);
            return Err(fail(err));
        }
        Ok(tmp)
    }

    /// `tmp`, whose bytes sum to `new`, checked against the host's sum and renamed over `path`.
    pub(super) fn finish(&self, path: &Path, tmp: &Path, new: String, remote: &str) -> TaskResult {
        let dest = Value::String(path.display().to_string());
        let mut out = Map::new();
        if self.validate_checksum && new != remote {
            // Unlike the reference, which leaves the mismatched file in place, nothing is written.
            let _ = std::fs::remove_file(tmp);
            out.insert("failed".into(), Value::Bool(true));
            out.insert("msg".into(), Value::String("checksum mismatch".into()));
            out.insert("file".into(), Value::String(self.source.clone()));
        } else if let Err(err) = std::fs::rename(tmp, path) {
            let _ = std::fs::remove_file(tmp);
            return TaskResult::failed_with(format!("Failed to fetch the file: {err}"));
        } else {
            out.insert("changed".into(), Value::Bool(true));
        }
        out.insert("checksum".into(), Value::String(new));
        out.insert("dest".into(), dest);
        // No MD5 here; the reference too reports `None` where it cannot compute one.
        out.insert("md5sum".into(), Value::Null);
        out.insert("remote_checksum".into(), Value::String(remote.to_string()));
        out.insert("remote_md5sum".into(), Value::Null);
        TaskResult(out)
    }
}
