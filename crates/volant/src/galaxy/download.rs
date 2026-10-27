// SPDX-License-Identifier: GPL-3.0-or-later
//! `collection download`, as ansible-galaxy 2.19.12 runs it (`download_collections`): the
//! requests resolved with their dependencies, each tarball fetched and copied into the folder,
//! and a `requirements.yml` naming them for an install with `--offline` from that folder.

use std::path::Path;

use serde_json::json;

use super::Failure;
use super::api::servers::Servers;
use super::install::{Local, Mixed, Options, Wanted, download_dir, locals};
use super::resolve::{self, Request};

/// Downloads `wanted` into `output` and returns the exit code.
pub fn collections(
    wanted: Vec<Wanted>,
    output: &Path,
    servers: &mut Servers,
    options: Options,
    verbose: u8,
) -> Result<i32, Failure> {
    let requests: Vec<Request> = wanted.iter().map(Wanted::request).collect();
    let (given, tarballs) = locals(wanted);
    let mut index = Mixed::new(tarballs, servers);
    println!("Process download dependency map");
    let resolution = resolve::resolve(
        &requests,
        &mut index,
        &resolve::Options {
            pre: options.pre,
            no_deps: options.no_deps,
            ..resolve::Options::default()
        },
    )?;
    println!(
        "Starting collection download process to '{}'",
        output.display()
    );
    let mut downloads = None;
    let mut listed = Vec::new();
    let result = (|| {
        for (fqcn, version) in &resolution {
            println!(
                "Downloading collection '{fqcn}:{version}' to '{}'",
                output.display()
            );
            let source = if let Some(Local::Dir(source)) = given.get(fqcn) {
                source.build(output, true, verbose)?
            } else if let Some(Local::Tarball(tarball)) = given.get(fqcn) {
                tarball.path().to_path_buf()
            } else {
                if downloads.is_none() {
                    downloads = Some(download_dir()?);
                }
                index.fetch(fqcn, version, downloads.as_deref().expect("made"))?
            };
            let name = source
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if source != output.join(&name) {
                std::fs::copy(&source, output.join(&name))
                    .map_err(|e| Failure::error(e.to_string()))?;
            }
            println!("Collection '{fqcn}:{version}' was downloaded successfully");
            listed.push(json!({"name": name, "version": version}));
        }
        let path = output.join("requirements.yml");
        println!(
            "Writing requirements.yml file of downloaded collections to '{}'",
            path.display()
        );
        let yaml = crate::template::inventory_yaml(&crate::template::yaml_node(
            &json!({"collections": listed}),
        ));
        std::fs::write(&path, yaml).map_err(|e| Failure::error(e.to_string()))?;
        Ok(0)
    })();
    if let Some(dir) = downloads.as_ref().and_then(|d| d.parent()) {
        let _ = std::fs::remove_dir_all(dir);
    }
    result
}
