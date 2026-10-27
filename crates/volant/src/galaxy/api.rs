// SPDX-License-Identifier: GPL-3.0-or-later
//! The read-only client of a Galaxy server's API v3, as ansible-galaxy 2.19.12 calls it
//! (`galaxy/api.py`: `g_connect`, `_call_galaxy`, `get_collection_metadata`,
//! `get_collection_versions`, `get_collection_version_metadata`, the response cache;
//! `_download_file`; `MultiGalaxyAPIProxy` for several servers). HTTP goes through `ureq` with
//! `rustls`, as decision record 0012 settles: the system's roots, the proxy of the environment,
//! and no token sent again on a redirect to another host.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use super::Failure;
use super::settings::ServerSettings;

/// A server and what its discovery call found.
pub struct Server {
    pub name: String,
    /// The API root once found: the URL given, or that URL with `/api` added.
    api_server: String,
    /// `available_versions`, in the server's order.
    versions: Vec<(String, String)>,
    agent: ureq::Agent,
    verbose: u8,
    /// The token of the settings, else the one in the token file once read.
    token: Option<String>,
    /// The token file, read at the first call that needs it when the settings give no token.
    token_file: Option<PathBuf>,
    cache: Option<Cache>,
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

/// Where the cache and the token file are, and what to do with the cache.
pub struct Files<'a> {
    /// `None` under `--no-cache`.
    pub cache_dir: Option<&'a Path>,
    pub clear_cache: bool,
    pub token_path: &'a Path,
}

impl Server {
    /// A server, not called yet. Its response cache is loaded now, as the reference loads it
    /// for each server it builds.
    pub fn new(settings: &ServerSettings, files: &Files, verbose: u8) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let certs: Vec<_> = rustls_native_certs::load_native_certs()
            .certs
            .iter()
            .map(|c| ureq::tls::Certificate::from_der(c.as_ref()).to_owned())
            .collect();
        let tls = ureq::tls::TlsConfig::builder()
            .root_certs(ureq::tls::RootCerts::new_with_certs(&certs))
            .disable_verification(!settings.validate_certs)
            .build();
        let agent = ureq::Agent::config_builder()
            .tls_config(tls)
            .proxy(ureq::Proxy::try_from_env())
            .redirect_auth_headers(ureq::config::RedirectAuthHeaders::SameHost)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(settings.timeout)))
            .build()
            .into();
        let cache = files
            .cache_dir
            .and_then(|dir| Cache::open(dir, files.clear_cache, verbose));
        Self {
            name: settings.name.clone(),
            api_server: settings.url.clone(),
            versions: Vec::new(),
            agent,
            verbose,
            token: settings.token.clone(),
            token_file: settings
                .token
                .is_none()
                .then(|| files.token_path.to_path_buf()),
            cache,
        }
    }

    /// The API root, once found.
    pub fn api_server(&self) -> &str {
        &self.api_server
    }

    /// The token to send: the settings', else the token file's, which is created empty when
    /// missing (`GalaxyToken`). Never printed.
    fn token(&mut self) -> Option<String> {
        if let Some(path) = self.token_file.take() {
            if !path.exists() {
                let _ = write_private(&path, b"");
                if self.verbose >= 3 {
                    println!("Created {}", path.display());
                }
            }
            if self.verbose >= 3 {
                println!("Opened {}", path.display());
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            self.token = text
                .lines()
                .find_map(|l| l.trim().strip_prefix("token:"))
                .map(|t| t.trim().trim_matches(['\'', '"']).to_string())
                .filter(|t| !t.is_empty());
        }
        self.token.clone()
    }

    /// The discovery call, made once: the URL as given, then with `/api` added when that one
    /// fails (`g_connect`).
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
            let found = match self.call(&url, &context, true, None) {
                Ok(data) => data,
                Err(first) => {
                    if url.ends_with("/api") || url.ends_with("/api/") {
                        return Err(first.failure());
                    }
                    url = join(&[&url, "/api/"]);
                    match self.call(&url, &context, true, None) {
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

    /// One GET, its body read as JSON (`_call_galaxy`). With `cache`, an answer less than a day
    /// old is taken from the response cache under `key`, the URL's path by default, and a new
    /// one is kept there; the pages of a list are kept together under one key.
    fn call(
        &mut self,
        url: &str,
        context: &str,
        cache: bool,
        key: Option<&str>,
    ) -> Result<Value, CallError> {
        let key = key.map_or_else(|| path_of(url).to_string(), str::to_string);
        let id = cache_id(url);
        let query = url.split_once('?').map_or("", |(_, q)| q);
        let paginated = query
            .split('&')
            .any(|p| p.starts_with("page=") || p.starts_with("offset="));
        if cache && let Some(store) = &mut self.cache {
            let server = store.server(&id);
            match server.get(&key) {
                Some(entry)
                    if !paginated
                        && entry["expires"]
                            .as_str()
                            .is_some_and(|e| e > now(0).as_str()) =>
                {
                    if entry["paginated"] == Value::Bool(true) {
                        let results = entry["results"].clone();
                        return Ok(if key.contains("/v3/") {
                            json!({"links": {"next": null}, "results": results})
                        } else {
                            json!({"next": null, "results": results})
                        });
                    }
                    return entry.get("results").cloned().ok_or(CallError {
                        code: 0,
                        failure: Failure::unexpected("'results'"),
                    });
                }
                _ if !paginated => {
                    server.insert(
                        key.clone(),
                        json!({"expires": now(86400), "paginated": false}),
                    );
                }
                _ => {}
            }
        }
        let token = self.token();
        if self.verbose >= 4 {
            println!("Calling Galaxy at {url}");
        }
        let unknown = |e: &dyn std::fmt::Display| CallError {
            code: 0,
            failure: Failure::caused(
                format!("Unknown error when attempting to call Galaxy at '{url}'"),
                e.to_string(),
            ),
        };
        let mut request = self.agent.get(url).header("Accept", "application/json");
        if let Some(token) = &token {
            request = request.header("Authorization", format!("Token {token}"));
        }
        let mut response = request.call().map_err(|e| unknown(&e))?;
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
                failure: Failure::error(galaxy_error(context, url, status, &body)),
            });
        }
        let data: Value = serde_json::from_str(&body).map_err(|_| CallError {
            code: 0,
            failure: Failure::error(format!(
                "Failed to parse Galaxy response from '{url}' as JSON:\n{body}"
            )),
        })?;
        if cache && let Some(store) = &mut self.cache {
            let entry = store.server(&id).entry(key).or_insert_with(|| json!({}));
            let list = ["data", "results"]
                .into_iter()
                .find(|k| data.get(*k).is_some());
            match list {
                Some(list) => {
                    entry["paginated"] = Value::Bool(true);
                    if !entry["results"].is_array() {
                        entry["results"] = json!([]);
                    }
                    let kept = entry["results"].as_array_mut().expect("an array");
                    kept.extend(data[list].as_array().cloned().unwrap_or_default());
                }
                None => entry["results"] = data.clone(),
            }
        }
        Ok(data)
    }

    /// Every version of a collection the server has, following the pages' `links.next`. A
    /// collection the server does not know has none.
    fn versions_of(&mut self, namespace: &str, name: &str) -> Result<Vec<String>, CallError> {
        self.connect("get_collection_versions")
            .map_err(CallError::other)?;
        let v3 = self.v3().unwrap_or("v3/").to_string();
        let first = join(&[
            &self.api_server,
            &v3,
            "collections",
            namespace,
            name,
            "versions",
            "/?limit=100",
        ]);
        let key = path_of(&first).to_string();
        if self.cache.is_some() {
            let info_url = join(&[&self.api_server, &v3, "collections", namespace, name, "/"]);
            let info_context = format!(
                "Error when getting the collection info for {namespace}.{name} from {} ({})",
                self.name, self.api_server
            );
            let modified = match self.call(&info_url, &info_context, false, None) {
                Err(e) if e.code == 404 => return Ok(Vec::new()),
                other => other?["updated_at"].clone(),
            };
            let id = cache_id(&first);
            let store = self.cache.as_mut().expect("a cache");
            let server = store.server(&id);
            let fqcn = format!("{namespace}.{name}");
            let cached = server
                .entry("modified")
                .or_insert_with(|| json!({}))
                .get(&fqcn)
                .cloned();
            if cached.as_ref() != Some(&modified) {
                server["modified"][&fqcn] = modified;
                server.remove(&key);
                store.save();
            }
        }
        let context = format!(
            "Error when getting available collection versions for {namespace}.{name} from {} ({})",
            self.name, self.api_server
        );
        let mut url = first;
        let mut versions = Vec::new();
        loop {
            let data = match self.call(&url, &context, true, Some(&key)) {
                Err(e) if e.code == 404 => return Ok(Vec::new()),
                other => other?,
            };
            let list = if data.get("data").is_some() {
                "data"
            } else {
                "results"
            };
            for entry in data[list].as_array().into_iter().flatten() {
                if let Some(v) = entry["version"].as_str() {
                    versions.push(v.to_string());
                }
            }
            match data["links"]["next"].as_str() {
                Some(next) if !next.is_empty() => {
                    url = absolute(&self.api_server, next).map_err(CallError::other)?;
                }
                _ => break,
            }
        }
        if let Some(store) = &self.cache {
            store.save();
        }
        Ok(versions)
    }

    /// [`Self::versions_of`] for one server alone.
    pub fn collection_versions(
        &mut self,
        namespace: &str,
        name: &str,
    ) -> Result<Vec<String>, Failure> {
        self.versions_of(namespace, name).map_err(|e| e.failure)
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
        let data = self
            .call(&url, &context, true, None)
            .map_err(|e| e.failure)?;
        if let Some(store) = &self.cache {
            store.save();
        }
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

/// Several servers, asked in order (`MultiGalaxyAPIProxy`).
pub struct Servers {
    pub list: Vec<Server>,
}

impl Servers {
    /// The versions of a collection on every server that answers, each with the index of its
    /// server. A server that cannot be reached is skipped with the reference's warning; an HTTP
    /// error skips it silently. When none answers, the last error is the result.
    pub fn versions(&mut self, fqcn: &str) -> Result<Vec<(String, usize)>, Failure> {
        let (namespace, name) = fqcn.split_once('.').unwrap_or((fqcn, ""));
        let mut found = Vec::new();
        let mut answered = false;
        let mut last = None;
        for (at, server) in self.list.iter_mut().enumerate() {
            match server.versions_of(namespace, name) {
                Ok(versions) => {
                    answered = true;
                    found.extend(versions.into_iter().map(|v| (v, at)));
                }
                Err(e) => {
                    if e.code == 0 {
                        eprintln!(
                            "[WARNING]: Skipping Galaxy server {}. Got an unexpected error when getting available versions of collection {fqcn}: {}",
                            server.api_server(),
                            e.failure.summary()
                        );
                    }
                    last = Some(e.failure);
                }
            }
        }
        match last {
            Some(failure) if !answered => Err(failure),
            _ => Ok(found),
        }
    }
}

/// The response cache file, `<GALAXY_CACHE_DIR>/api.json` (`_load_cache`, `_set_cache`).
struct Cache {
    path: PathBuf,
    data: Map<String, Value>,
}

impl Cache {
    fn open(dir: &Path, clear: bool, verbose: u8) -> Option<Self> {
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
    fn server(&mut self, id: &str) -> &mut Map<String, Value> {
        let entry = self.data.entry(id).or_insert_with(|| json!({}));
        if !entry.is_object() {
            *entry = json!({});
        }
        entry.as_object_mut().expect("an object")
    }

    fn save(&self) {
        let _ = std::fs::write(&self.path, Value::Object(self.data.clone()).to_string());
    }
}

/// A file of mode `0600`.
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
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
fn make_private_dir(dir: &Path) {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    let _ = builder.create(dir);
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

/// A failed call: the HTTP code, 0 when there is none.
struct CallError {
    code: u16,
    failure: Failure,
}

impl CallError {
    fn other(failure: Failure) -> Self {
        Self { code: 0, failure }
    }

    fn failure(self) -> Failure {
        self.failure
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
    Ok(format!("{}{link}", origin(api_server)))
}

/// `scheme://host[:port]` of a URL.
fn origin(url: &str) -> &str {
    url.find("://")
        .and_then(|at| url[at + 3..].find('/').map(|slash| &url[..at + 3 + slash]))
        .unwrap_or(url)
}

/// The path of a URL, without its query.
fn path_of(url: &str) -> &str {
    let rest = &url[origin(url).len()..];
    rest.split('?').next().unwrap_or(rest)
}

/// `get_cache_id`: `host:port`, the port empty when the URL has none.
fn cache_id(url: &str) -> String {
    let host_port = origin(url).split("://").nth(1).unwrap_or_default();
    let host_port = host_port.rsplit('@').next().unwrap_or(host_port);
    match host_port.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => format!("{host}:{port}"),
        _ => format!("{host_port}:"),
    }
}

/// The UTC time `ahead` seconds from now, as the cache writes it: `%Y-%m-%dT%H:%M:%SZ`.
fn now(ahead: u64) -> String {
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
