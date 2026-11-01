// SPDX-License-Identifier: GPL-3.0-or-later
//! `collection install`, as ansible-galaxy 2.19.12 runs it (`install_collections`, `install`):
//! what is already there at an acceptable version is left alone, the rest is resolved against
//! the servers and the tarballs at hand, fetched, checked and unpacked in the resolver's order.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::Failure;
use super::api::VersionInfo;
use super::api::download::galaxy_yml;
use super::api::servers::Servers;
use super::artifact::{Meta, Tarball};
use super::build::Source;
use super::requirements::{Collection, Kind};
use super::resolve::{self, Index, Request};
use super::version::meets;

/// The install options that change what is installed.
#[derive(Debug, Default, Clone, Copy)]
pub struct Options {
    pub force: bool,
    pub force_with_deps: bool,
    pub ignore_errors: bool,
    pub upgrade: bool,
    pub no_deps: bool,
    pub pre: bool,
}

/// A collection asked for: a tarball read already, or a name and a range for the servers.
pub enum Wanted {
    Tarball(Tarball, Meta),
    Dir(Source),
    Galaxy { fqcn: String, range: String },
}

impl Wanted {
    pub(super) fn request(&self) -> Request {
        match self {
            Self::Tarball(_, meta) => Request::direct(&meta.fqcn(), &meta.version),
            Self::Dir(source) => Request::direct(&source.fqcn(), &source.version()),
            Self::Galaxy { fqcn, range } => Request::direct(fqcn, range),
        }
    }
}

/// The requirements the command line or a file gave, read: a tarball is opened and a git source
/// cloned now, so their errors come before any output, as the reference reads them while
/// parsing. The clones are removed when the returned directories are dropped.
pub fn prepare(
    collections: &[Collection],
    ignore_certs: bool,
) -> Result<(Vec<Wanted>, Vec<tempfile::TempDir>), Failure> {
    let mut wanted = Vec::new();
    let mut clones = Vec::new();
    for c in collections {
        let source = c.source.as_deref().unwrap_or_default();
        wanted.push(match c.kind {
            Kind::File => {
                let (tarball, meta) = Tarball::open(source)?;
                Wanted::Tarball(tarball, meta)
            }
            Kind::Dir => Wanted::Dir(Source::read(&super::resolve(source))?),
            Kind::Git => {
                let checkout = super::git::checkout(source, &c.version, ignore_certs)?;
                let read = Source::read(&checkout.collection)?;
                clones.push(checkout.clone);
                Wanted::Dir(read)
            }
            Kind::Galaxy => Wanted::Galaxy {
                fqcn: c.name.clone().unwrap_or_default(),
                range: c.version.clone(),
            },
            Kind::Url | Kind::Subdirs => {
                return Err(Failure::error(
                    "volant galaxy collection install from a URL or a namespace directory is not supported yet",
                ));
            }
        });
    }
    Ok((wanted, clones))
}

/// The version and dependencies of each local collection, by `namespace.name`.
pub(super) type Versions = HashMap<String, (String, Vec<(String, String)>)>;

/// A collection given on the command line or in a file rather than found on a server.
pub(super) enum Local {
    Tarball(Tarball),
    Dir(Source),
}

/// The local collections of `wanted`, and what the resolver reads of them: version and
/// dependencies.
pub(super) fn locals(wanted: Vec<Wanted>) -> (HashMap<String, Local>, Versions) {
    let mut given = HashMap::new();
    let mut index = HashMap::new();
    for item in wanted {
        match item {
            Wanted::Tarball(tarball, meta) => {
                index.insert(
                    meta.fqcn(),
                    (meta.version.clone(), meta.dependencies.clone()),
                );
                given.insert(meta.fqcn(), Local::Tarball(tarball));
            }
            Wanted::Dir(source) => {
                index.insert(source.fqcn(), (source.version(), source.dependencies()));
                given.insert(source.fqcn(), Local::Dir(source));
            }
            Wanted::Galaxy { .. } => {}
        }
    }
    (given, index)
}

/// Where the versions and the dependencies come from: the tarballs given, then the servers.
pub(super) struct Mixed<'a> {
    tarballs: HashMap<String, (String, Vec<(String, String)>)>,
    servers: &'a mut Servers,
    /// The server each version was found on, the first that lists it.
    found: HashMap<(String, String), usize>,
    records: HashMap<(String, String), VersionInfo>,
}

impl<'a> Mixed<'a> {
    pub(super) fn new(tarballs: Versions, servers: &'a mut Servers) -> Self {
        Self {
            tarballs,
            servers,
            found: HashMap::new(),
            records: HashMap::new(),
        }
    }

    /// Downloads a pinned version from the server that listed it into a new directory under
    /// `dir`, and returns the tarball.
    pub(super) fn fetch(
        &mut self,
        fqcn: &str,
        version: &str,
        dir: &Path,
    ) -> Result<PathBuf, Failure> {
        let info = self.record(fqcn, version)?;
        let at = self.at(fqcn, version);
        self.servers.list[at].download(
            &info.download_url,
            &info.sha256,
            dir,
            &format!("{fqcn}:{version}"),
        )
    }

    fn at(&self, fqcn: &str, version: &str) -> usize {
        self.found
            .get(&(fqcn.to_string(), version.to_string()))
            .copied()
            .unwrap_or(0)
    }

    fn record(&mut self, fqcn: &str, version: &str) -> Result<VersionInfo, Failure> {
        let key = (fqcn.to_string(), version.to_string());
        if let Some(info) = self.records.get(&key) {
            return Ok(info.clone());
        }
        let at = self.found.get(&key).copied().unwrap_or(0);
        let (namespace, name) = fqcn.split_once('.').unwrap_or((fqcn, ""));
        let info = self.servers.list[at].version_info(namespace, name, version)?;
        self.records.insert(key, info.clone());
        Ok(info)
    }
}

impl Index for Mixed<'_> {
    fn versions(&mut self, fqcn: &str) -> Result<Vec<String>, Failure> {
        if let Some((version, _)) = self.tarballs.get(fqcn) {
            return Ok(vec![version.clone()]);
        }
        let listed = self.servers.versions(fqcn)?;
        let mut out = Vec::new();
        for (version, at) in listed {
            self.found
                .entry((fqcn.to_string(), version.clone()))
                .or_insert(at);
            if !out.contains(&version) {
                out.push(version);
            }
        }
        Ok(out)
    }

    fn dependencies(
        &mut self,
        fqcn: &str,
        version: &str,
    ) -> Result<Vec<(String, String)>, Failure> {
        if let Some((_, dependencies)) = self.tarballs.get(fqcn) {
            return Ok(dependencies.clone());
        }
        Ok(self.record(fqcn, version)?.dependencies)
    }
}

/// Installs `wanted` under `output`, the `ansible_collections` directory, and returns the exit
/// code.
pub fn collections(
    wanted: Vec<Wanted>,
    output: &Path,
    servers: &mut Servers,
    options: Options,
    verbose: u8,
) -> Result<i32, Failure> {
    println!("Starting galaxy collection install process");
    std::fs::create_dir_all(output).map_err(|e| Failure::error(e.to_string()))?;
    let existing = installed(output, verbose);
    let requests: Vec<Request> = wanted.iter().map(Wanted::request).collect();
    let forced = options.force || options.force_with_deps;
    let unsatisfied = requests.iter().any(|r| {
        forced
            || !existing
                .iter()
                .any(|(fqcn, version)| *fqcn == r.fqcn && meets(version, &r.range))
    });
    if !unsatisfied && !options.upgrade {
        println!(
            "Nothing to do. All requested collections are already installed. If you want to reinstall them, consider using `--force`."
        );
        return Ok(0);
    }
    let preferred: Vec<(String, String)> = if options.force_with_deps {
        Vec::new()
    } else if options.force {
        existing
            .iter()
            .filter(|(fqcn, _)| !requests.iter().any(|r| r.fqcn == *fqcn))
            .cloned()
            .collect()
    } else {
        existing.clone()
    };
    println!("Process install dependency map");
    let (mut files, tarballs) = locals(wanted);
    let mut index = Mixed::new(tarballs, servers);
    let resolution = resolve::resolve(
        &requests,
        &mut index,
        &resolve::Options {
            pre: options.pre,
            upgrade: options.upgrade,
            no_deps: options.no_deps,
            preferred: preferred.clone(),
        },
    )?;
    println!("Starting collection install process");
    let mut downloads: Option<PathBuf> = None;
    let mut done = Ok(0);
    for (fqcn, version) in resolution {
        let shown = format!("{fqcn}:{version}");
        if preferred.iter().any(|(f, v)| *f == fqcn && *v == version) {
            println!("'{shown}' is already installed, skipping.");
            continue;
        }
        let one = install_one(
            &fqcn,
            &version,
            output,
            &mut index,
            &mut files,
            &mut downloads,
            verbose,
        );
        match one {
            Ok(()) => {}
            Err(err) if options.ignore_errors => eprintln!(
                "[WARNING]: Failed to install collection {shown} but skipping due to --ignore-errors being set. Error: {}",
                err.summary()
            ),
            Err(err) => {
                done = Err(err);
                break;
            }
        }
    }
    if let Some(dir) = downloads.as_ref().and_then(|d| d.parent()) {
        let _ = std::fs::remove_dir_all(dir);
    }
    done
}

/// One pin: from its tarball, or downloaded from the server that listed it.
fn install_one(
    fqcn: &str,
    version: &str,
    output: &Path,
    index: &mut Mixed,
    files: &mut HashMap<String, Local>,
    downloads: &mut Option<PathBuf>,
    verbose: u8,
) -> Result<(), Failure> {
    let (namespace, name) = fqcn.split_once('.').unwrap_or((fqcn, ""));
    let shown = format!("{fqcn}:{version}");
    let dest = output.join(namespace).join(name);
    let local = files.remove(fqcn);
    if let Some(Local::Dir(source)) = local {
        println!("Installing '{shown}' to '{}'", dest.display());
        if dest.exists() {
            std::fs::remove_dir_all(&dest).map_err(|e| Failure::error(e.to_string()))?;
        }
        source.install(&dest, verbose)?;
        println!("{shown} was installed successfully");
        return Ok(());
    }
    let (tarball, source) = if let Some(Local::Tarball(tarball)) = local {
        (tarball, None)
    } else {
        let info = index.record(fqcn, version)?;
        let at = index.at(fqcn, version);
        if downloads.is_none() {
            *downloads = Some(download_dir()?);
        }
        let file = index.fetch(fqcn, version, downloads.as_deref().expect("made"))?;
        let (tarball, _) = Tarball::open(&file.display().to_string())?;
        (tarball, Some((info, at)))
    };
    println!("Installing '{shown}' to '{}'", dest.display());
    if dest.exists() {
        std::fs::remove_dir_all(&dest).map_err(|e| Failure::error(e.to_string()))?;
    }
    tarball.install(&dest)?;
    remove_source_metadata(output, namespace, name);
    if let Some((info, at)) = source {
        write_source_metadata(output, &info, &galaxy_yml(&info, &index.servers.list[at]))?;
    }
    println!("{shown} was installed successfully");
    Ok(())
}

/// The directory downloads go to: a new `ansible-local-<pid><random>/tmp<random>` under
/// `<ANSIBLE_HOME>/tmp`, as the reference's `DEFAULT_LOCAL_TMP` and artifacts manager make
/// them. It is removed once the install is over.
pub(super) fn download_dir() -> Result<PathBuf, Failure> {
    let tmp = super::settings::ansible_home().join("tmp");
    std::fs::create_dir_all(&tmp).map_err(|e| Failure::error(e.to_string()))?;
    let local = super::temp_dir(&tmp, &format!("ansible-local-{}", std::process::id()))?;
    super::temp_dir(&local, "tmp")
}

/// The collections installed under `output`, by `namespace.name`, with the version their
/// `MANIFEST.json` gives.
pub fn installed(output: &Path, verbose: u8) -> Vec<(String, String)> {
    let dirs = |dir: &Path| -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        found.sort();
        found
    };
    let mut out = Vec::new();
    for namespace in dirs(output) {
        for collection in dirs(&namespace) {
            let Ok(text) = std::fs::read(collection.join("MANIFEST.json")) else {
                continue;
            };
            let Ok(json) = serde_json::from_slice::<serde_json::Value>(&text) else {
                continue;
            };
            let info = &json["collection_info"];
            if let (Some(ns), Some(name), Some(version)) = (
                info["namespace"].as_str(),
                info["name"].as_str(),
                info["version"].as_str(),
            ) {
                if verbose >= 3 {
                    println!(
                        "Found installed collection {ns}.{name}:{version} at '{}'",
                        collection.display()
                    );
                }
                out.push((format!("{ns}.{name}"), version.to_string()));
            }
        }
    }
    out
}

/// Removes the `<namespace>.<name>-*.info` directories an earlier install from a server left.
fn remove_source_metadata(output: &Path, namespace: &str, name: &str) {
    let prefix = format!("{namespace}.{name}-");
    for entry in std::fs::read_dir(output).into_iter().flatten().flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if file.starts_with(&prefix) && file.ends_with(".info") {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// `<namespace>.<name>-<version>.info/GALAXY.yml`, the directory `0755` and the file `0644`
/// (`write_source_metadata`).
fn write_source_metadata(output: &Path, info: &VersionInfo, yaml: &str) -> Result<(), Failure> {
    let dir = output.join(format!(
        "{}.{}-{}.info",
        info.namespace, info.name, info.version
    ));
    let io = |e: std::io::Error| Failure::error(e.to_string());
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(io)?;
    }
    std::fs::create_dir(&dir).map_err(io)?;
    let file = dir.join("GALAXY.yml");
    std::fs::write(&file, yaml).map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).map_err(io)?;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).map_err(io)?;
    }
    Ok(())
}
