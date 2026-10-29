// SPDX-License-Identifier: GPL-3.0-or-later
//! The role side of a Galaxy server, API v1 (`lookup_role_by_name`, `fetch_role_related`), and
//! the plain download a role archive takes (`GalaxyRole.fetch`).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{Server, absolute, join};
use crate::galaxy::Failure;

impl Server {
    /// The role record of `owner.name`, or `None` when the server has none. Prints the
    /// reference's `- downloading role` line.
    pub fn lookup_role(&mut self, src: &str) -> Result<Option<Value>, Failure> {
        self.connect("lookup_role_by_name", "v1")?;
        let (owner, name) = src.rsplit_once('.').unwrap_or(("", src));
        println!("- downloading role '{name}', owned by {owner}");
        let v1 = self.v1();
        let url = join(&[
            &self.api_server,
            &v1,
            "roles",
            &format!("?owner__username={owner}&name={name}"),
        ]);
        let context = format!("Error when finding role {src}");
        let data = self
            .call(&url, &context, false, None)
            .map_err(|e| e.failure)?;
        Ok(data["results"].as_array().and_then(|r| r.first()).cloned())
    }

    /// The versions of the role numbered `id`, every page. A failure is a warning and leaves
    /// the versions found so far, as `fetch_role_related` has it.
    pub fn role_versions(&mut self, id: &Value) -> Vec<Value> {
        let mut out = Vec::new();
        if let Err(failure) = self.role_pages(id, &mut out) {
            eprintln!(
                "[WARNING]: Unable to retrieve role (id={id}) data (versions), but this is not fatal so we continue: {}",
                failure.summary()
            );
            out.clear();
        }
        out
    }

    fn role_pages(&mut self, id: &Value, out: &mut Vec<Value>) -> Result<(), Failure> {
        self.connect("fetch_role_related", "v1")?;
        let v1 = self.v1();
        let mut url = join(&[
            &self.api_server,
            &v1,
            "roles",
            &id.to_string(),
            "versions",
            "?page_size=50",
        ]);
        let context = format!("Error when getting the versions of role {id}");
        loop {
            let data = self
                .call(&url, &context, false, None)
                .map_err(|e| e.failure)?;
            out.extend(data["results"].as_array().cloned().unwrap_or_default());
            match data["next_link"].as_str() {
                Some(next) => url = absolute(&self.api_server, next)?,
                None => return Ok(()),
            }
        }
    }

    fn v1(&self) -> String {
        self.versions
            .iter()
            .find(|(k, _)| k == "v1")
            .map_or_else(|| "v1/".to_string(), |(_, v)| v.clone())
    }

    /// Downloads `url` to a new file under `dir`, with no checksum to check. A failure is
    /// printed as an error and gives `None`, as `GalaxyRole.fetch` has it.
    pub fn fetch(&mut self, url: &str, dir: &Path) -> Option<PathBuf> {
        match self.plain_download(url, dir) {
            Ok(path) => Some(path),
            Err(message) => {
                eprintln!("[ERROR]: failed to download the file: {message}");
                None
            }
        }
    }

    fn plain_download(&mut self, url: &str, dir: &Path) -> Result<PathBuf, String> {
        let mut response = self.agent.get(url).call().map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!(
                "HTTP Error {}: {}",
                response.status().as_u16(),
                response.status().canonical_reason().unwrap_or_default()
            ));
        }
        let path = crate::galaxy::temp_dir(dir, "tmp")
            .map_err(|f| f.message)?
            .join("role.tar.gz");
        let mut file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
        std::io::copy(&mut response.body_mut().as_reader(), &mut file)
            .and_then(|_| file.flush())
            .map_err(|e| e.to_string())?;
        Ok(path)
    }
}
