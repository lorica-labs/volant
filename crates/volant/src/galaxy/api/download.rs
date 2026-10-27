// SPDX-License-Identifier: GPL-3.0-or-later
//! A collection tarball fetched from a server, and the record of where it came from
//! (`_download_file`, `write_source_metadata`).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::json;
use sha2::{Digest as _, Sha256};

use super::{Server, VersionInfo};
use crate::galaxy::Failure;

impl Server {
    /// Downloads `url` into a new directory under `dir`, named after the tarball, and returns the
    /// file once its SHA-256 is `sha256` (`_download_file`). A mismatch leaves nothing behind.
    pub fn download(&mut self, url: &str, sha256: &str, dir: &Path) -> Result<PathBuf, Failure> {
        let tarball = url.rsplit('/').next().unwrap_or("collection.tar.gz");
        let stem = tarball.strip_suffix(".tar.gz").unwrap_or(tarball);
        let holder = tempfile::Builder::new()
            .prefix(&format!("{stem}-"))
            .tempdir_in(dir)
            .map_err(|e| Failure::error(e.to_string()))?
            .keep();
        println!("Downloading {url} to {}", holder.display());
        let fail = |failure: Failure| {
            let _ = std::fs::remove_dir_all(&holder);
            failure
        };
        let mut request = self.agent.get(url);
        if let Some(token) = self.token() {
            request = request.header("Authorization", format!("Token {token}"));
        }
        let mut response = request.call().map_err(|e| {
            fail(Failure::caused(
                format!("Unknown error when attempting to call Galaxy at '{url}'"),
                e.to_string(),
            ))
        })?;
        if !response.status().is_success() {
            return Err(fail(Failure::error(format!(
                "Failed to download collection tar from '{url}' (HTTP Code: {})",
                response.status().as_u16()
            ))));
        }
        let path = holder.join(tarball);
        let partial = holder.join(format!(".{tarball}.volant-partial"));
        let io = |e: std::io::Error| fail(Failure::error(e.to_string()));
        let mut file = std::fs::File::create(&partial).map_err(io)?;
        let mut hasher = Sha256::new();
        let mut reader = response.body_mut().as_reader();
        let mut buffer = vec![0; 64 * 1024];
        loop {
            let n = std::io::Read::read(&mut reader, &mut buffer).map_err(io)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            file.write_all(&buffer[..n]).map_err(io)?;
        }
        drop(file);
        let actual = format!("{:x}", hasher.finalize());
        if !sha256.is_empty() {
            if self.verbose >= 4 {
                println!("Validating downloaded file hash {actual} with expected hash {sha256}");
            }
            if actual != sha256 {
                return Err(fail(Failure::error(
                    "Mismatch artifact hash with downloaded file",
                )));
            }
        }
        std::fs::rename(&partial, &path).map_err(io)?;
        Ok(path)
    }
}

/// The `GALAXY.yml` an install from a server writes under
/// `<namespace>.<name>-<version>.info/` (`write_source_metadata`).
pub fn galaxy_yml(info: &VersionInfo, server: &Server) -> String {
    let source = json!({
        "download_url": info.download_url,
        "format_version": "1.0.0",
        "name": info.name,
        "namespace": info.namespace,
        "server": server.api_server(),
        "signatures": info.signatures,
        "version": info.version,
        "version_url": info.href,
    });
    crate::template::inventory_yaml(&crate::template::yaml_node(&source))
}
