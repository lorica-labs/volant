// SPDX-License-Identifier: GPL-3.0-or-later
//! The read-only client of a Galaxy server's API v3, as ansible-galaxy 2.19.12 calls it
//! (`galaxy/api.py`: `g_connect`, `_call_galaxy`, `get_collection_metadata`,
//! `get_collection_versions`, `get_collection_version_metadata`; `_download_file`). HTTP goes
//! through `ureq` with `rustls`, as decision record 0012 settles: the system's roots, the proxy
//! of the environment, and no token sent again on a redirect to another host.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::Failure;

/// A server and what its discovery call found.
pub struct Server {
    /// The name ansible-galaxy gives it: `cmd_arg` for `-s`, `default`, or its section's name.
    pub name: String,
    /// The API root once found: the URL given, or that URL with `/api` added.
    api_server: String,
    /// `available_versions`, in the server's order.
    versions: Vec<(String, String)>,
    agent: ureq::Agent,
    verbose: u8,
}

/// What a collection version's record gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionInfo {
    pub namespace: String,
    pub name: String,
    pub version: String,
    /// Absolute.
    pub download_url: String,
    pub sha256: String,
    /// `namespace.name` and a version range, in the record's order.
    pub dependencies: Vec<(String, String)>,
    /// The record's own path, written as `version_url`.
    pub href: String,
    pub signatures: Vec<String>,
}

impl Server {
    /// A server at `url`, not called yet.
    pub fn new(name: &str, url: &str, verbose: u8) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let certs: Vec<_> = rustls_native_certs::load_native_certs()
            .certs
            .iter()
            .map(|c| ureq::tls::Certificate::from_der(c.as_ref()).to_owned())
            .collect();
        let tls = ureq::tls::TlsConfig::builder()
            .root_certs(ureq::tls::RootCerts::new_with_certs(&certs))
            .build();
        let agent = ureq::Agent::config_builder()
            .tls_config(tls)
            .proxy(ureq::Proxy::try_from_env())
            .redirect_auth_headers(ureq::config::RedirectAuthHeaders::SameHost)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)))
            .build()
            .into();
        Self {
            name: name.to_string(),
            api_server: url.to_string(),
            versions: Vec::new(),
            agent,
            verbose,
        }
    }

    /// The API root, once [`Self::connect`] found it.
    pub fn api_server(&self) -> &str {
        &self.api_server
    }

    /// The discovery call, made once: the URL as given, then with `/api` added when that one is
    /// not an API root (`g_connect`).
    fn connect(&mut self, action: &str) -> Result<(), Failure> {
        if self.versions.is_empty() {
            if self.verbose >= 4 {
                println!("Initial connection to galaxy_server: {}", self.api_server);
            }
            let mut url = self.api_server.clone();
            let context = format!(
                "Error when finding available api versions from {} ({url})",
                self.name
            );
            if url == "https://galaxy.ansible.com" || url == "https://galaxy.ansible.com/" {
                url = "https://galaxy.ansible.com/api/".into();
            }
            let found = match self.call(&url, &context) {
                Ok(data) => data,
                Err(first) => {
                    if url.ends_with("/api") || url.ends_with("/api/") {
                        return Err(first.failure());
                    }
                    url = join(&[&url, "/api/"]);
                    match self.call(&url, &context) {
                        Ok(data) => data,
                        Err(e) if e.code == 404 => return Err(first.failure()),
                        Err(e) => return Err(e.failure()),
                    }
                }
            };
            let Some(Value::Object(available)) = found.get("available_versions") else {
                return Err(Failure::error(format!(
                    "Tried to find galaxy API root at {url} but no 'available_versions' are available on {}",
                    self.api_server
                )));
            };
            self.api_server = url;
            self.versions = available
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                .collect();
            if self.versions.len() == 1 && self.versions[0].0 == "v1" {
                self.versions.push(("v2".into(), "v2/".into()));
            }
            if self.verbose >= 4 {
                let keys: Vec<&str> = self.versions.iter().map(|(k, _)| k.as_str()).collect();
                println!(
                    "Found API version '{}' with Galaxy server {} ({})",
                    keys.join(", "),
                    self.name,
                    self.api_server
                );
            }
        }
        if self.v3().is_none() {
            let keys: Vec<&str> = self.versions.iter().map(|(k, _)| k.as_str()).collect();
            return Err(Failure::error(format!(
                "Galaxy action {action} requires API versions 'v3' but only '{}' are available on {} {}",
                keys.join(", "),
                self.name,
                self.api_server
            )));
        }
        Ok(())
    }

    fn v3(&self) -> Option<&str> {
        self.versions
            .iter()
            .find(|(k, _)| k == "v3")
            .map(|(_, v)| v.as_str())
    }

    /// One GET, its body read as JSON (`_call_galaxy`).
    fn call(&self, url: &str, context: &str) -> Result<Value, CallError> {
        if self.verbose >= 4 {
            println!("Calling Galaxy at {url}");
        }
        let unknown = |e: &dyn std::fmt::Display| CallError {
            code: 0,
            message: format!("Unknown error when attempting to call Galaxy at '{url}': {e}"),
        };
        let mut response = self
            .agent
            .get(url)
            .header("Accept", "application/json")
            .call()
            .map_err(|e| unknown(&e))?;
        let status = response.status();
        let body = response
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_string()
            .map_err(|e| unknown(&e))?;
        if !status.is_success() {
            return Err(CallError {
                code: status.as_u16(),
                message: galaxy_error(context, url, status, &body),
            });
        }
        serde_json::from_str(&body).map_err(|_| CallError {
            code: 0,
            message: format!("Failed to parse Galaxy response from '{url}' as JSON:\n{body}"),
        })
    }

    /// Every version of a collection the server has, following the pages' `links.next`. A
    /// collection the server does not know has none.
    pub fn collection_versions(
        &mut self,
        namespace: &str,
        name: &str,
    ) -> Result<Vec<String>, Failure> {
        self.connect("get_collection_versions")?;
        let v3 = self.v3().unwrap_or("v3/").to_string();
        let info_url = join(&[&self.api_server, &v3, "collections", namespace, name, "/"]);
        let info_context = format!(
            "Error when getting the collection info for {namespace}.{name} from {} ({})",
            self.name, self.api_server
        );
        match self.call(&info_url, &info_context) {
            Err(e) if e.code == 404 => return Ok(Vec::new()),
            other => other.map_err(CallError::failure)?,
        };
        let context = format!(
            "Error when getting available collection versions for {namespace}.{name} from {} ({})",
            self.name, self.api_server
        );
        let mut url = join(&[
            &self.api_server,
            &v3,
            "collections",
            namespace,
            name,
            "versions",
            "/?limit=100",
        ]);
        let mut versions = Vec::new();
        loop {
            let data = match self.call(&url, &context) {
                Err(e) if e.code == 404 => return Ok(Vec::new()),
                other => other.map_err(CallError::failure)?,
            };
            let key = if data.get("data").is_some() {
                "data"
            } else {
                "results"
            };
            for entry in data[key].as_array().into_iter().flatten() {
                if let Some(v) = entry["version"].as_str() {
                    versions.push(v.to_string());
                }
            }
            match data["links"]["next"].as_str() {
                Some(next) if !next.is_empty() => url = absolute(&self.api_server, next)?,
                _ => break,
            }
        }
        Ok(versions)
    }

    /// The record of one version.
    pub fn version_info(
        &mut self,
        namespace: &str,
        name: &str,
        version: &str,
    ) -> Result<VersionInfo, Failure> {
        self.connect("get_collection_version_metadata")?;
        let v3 = self.v3().unwrap_or("v3/").to_string();
        let url = join(&[
            &self.api_server,
            &v3,
            "collections",
            namespace,
            name,
            "versions",
            version,
            "/",
        ]);
        let context = format!(
            "Error when getting collection version metadata for {namespace}.{name}:{version} from {} ({})",
            self.name, self.api_server
        );
        let data = self.call(&url, &context).map_err(CallError::failure)?;
        let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
        let download = text(&data["download_url"]);
        if !download.contains("://") && !download.starts_with('/') {
            return Err(Failure::error(format!(
                "Invalid non absolute download_url: {download}"
            )));
        }
        Ok(VersionInfo {
            namespace: text(&data["namespace"]["name"]),
            name: text(&data["collection"]["name"]),
            version: text(&data["version"]),
            download_url: absolute(&self.api_server, &download)?,
            sha256: text(&data["artifact"]["sha256"]),
            dependencies: data["metadata"]["dependencies"]
                .as_object()
                .map(|d| d.iter().map(|(k, v)| (k.clone(), text(v))).collect())
                .unwrap_or_default(),
            href: text(&data["href"]),
            signatures: data["signatures"]
                .as_array()
                .map(|s| s.iter().map(|e| text(&e["signature"])).collect())
                .unwrap_or_default(),
        })
    }

    /// Downloads `url` into a new directory under `dir`, named after the tarball, and returns the
    /// file once its SHA-256 is `sha256` (`_download_file`). A mismatch leaves nothing behind.
    pub fn download(&self, url: &str, sha256: &str, dir: &Path) -> Result<PathBuf, Failure> {
        let tarball = url.rsplit('/').next().unwrap_or("collection.tar.gz");
        let stem = tarball.strip_suffix(".tar.gz").unwrap_or(tarball);
        let holder = tempfile::Builder::new()
            .prefix(&format!("{stem}-"))
            .tempdir_in(dir)
            .map_err(|e| Failure::error(e.to_string()))?
            .keep();
        println!("Downloading {url} to {}", holder.display());
        let fail = |message: String| {
            let _ = std::fs::remove_dir_all(&holder);
            Failure::error(message)
        };
        let mut response = self.agent.get(url).call().map_err(|e| {
            fail(format!(
                "Unknown error when attempting to call Galaxy at '{url}': {e}"
            ))
        })?;
        if !response.status().is_success() {
            return Err(fail(format!(
                "Failed to download collection tar from '{url}' (HTTP Code: {})",
                response.status().as_u16()
            )));
        }
        let path = holder.join(tarball);
        let partial = holder.join(format!(".{tarball}.volant-partial"));
        let mut file = std::fs::File::create(&partial).map_err(|e| fail(e.to_string()))?;
        let mut hasher = Sha256::new();
        let mut reader = response.body_mut().as_reader();
        let mut buffer = vec![0; 64 * 1024];
        loop {
            let n =
                std::io::Read::read(&mut reader, &mut buffer).map_err(|e| fail(e.to_string()))?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            file.write_all(&buffer[..n])
                .map_err(|e| fail(e.to_string()))?;
        }
        drop(file);
        let actual = format!("{:x}", hasher.finalize());
        if !sha256.is_empty() {
            if self.verbose >= 4 {
                println!("Validating downloaded file hash {actual} with expected hash {sha256}");
            }
            if actual != sha256 {
                return Err(fail("Mismatch artifact hash with downloaded file".into()));
            }
        }
        std::fs::rename(&partial, &path).map_err(|e| fail(e.to_string()))?;
        Ok(path)
    }
}

/// The `GALAXY.yml` an install from a server writes under
/// `<namespace>.<name>-<version>.info/` (`write_source_metadata`).
pub fn galaxy_yml(info: &VersionInfo, server: &Server) -> String {
    let source = serde_json::json!({
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

/// A failed call: the HTTP code, 0 when there is none, and the message.
struct CallError {
    code: u16,
    message: String,
}

impl CallError {
    fn failure(self) -> Failure {
        Failure::error(self.message)
    }
}

/// `GalaxyError`'s message for an HTTP error answer.
pub(super) fn galaxy_error(
    context: &str,
    url: &str,
    status: ureq::http::StatusCode,
    body: &str,
) -> String {
    let code = status.as_u16();
    let reason = status.canonical_reason().unwrap_or_default();
    let info: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let parts: Vec<&str> = url.split('/').collect();
    if parts.contains(&"v2") {
        let message = info["message"].as_str().unwrap_or(reason);
        let galaxy_code = info["code"].as_str().unwrap_or("Unknown");
        format!("{context} (HTTP Code: {code}, Message: {message} Code: {galaxy_code})")
    } else if parts.contains(&"v3") {
        let errors = match info["errors"].as_array() {
            Some(list) if !list.is_empty() => list.clone(),
            _ => vec![Value::Null],
        };
        let lines: Vec<String> = errors
            .iter()
            .map(|e| {
                let message = e["detail"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .or_else(|| e["title"].as_str().filter(|s| !s.is_empty()))
                    .unwrap_or(reason);
                let galaxy_code = e["code"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("Unknown");
                format!("(HTTP Code: {code}, Message: {message} Code: {galaxy_code})")
            })
            .collect();
        format!("{context} {}", lines.join(", "))
    } else {
        let message = info["default"].as_str().unwrap_or(reason);
        format!("{context} (HTTP Code: {code}, Message: {message})")
    }
}

/// `_urljoin`: each part without its slashes, joined by one; a last part of `/` leaves the
/// trailing slash, and one of `/?q` the query after it.
fn join(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| p.trim_matches('/'))
        .collect::<Vec<_>>()
        .join("/")
}

/// `urljoin(api_server, link)` for a link that is a full URL or a path from the host's root.
fn absolute(api_server: &str, link: &str) -> Result<String, Failure> {
    if link.contains("://") {
        return Ok(link.to_string());
    }
    if !link.starts_with('/') {
        return Err(Failure::error(format!(
            "Invalid non absolute pagination link: {link}"
        )));
    }
    let root = api_server
        .find("://")
        .and_then(|at| {
            api_server[at + 3..]
                .find('/')
                .map(|slash| &api_server[..at + 3 + slash])
        })
        .unwrap_or(api_server);
    Ok(format!("{root}{link}"))
}
