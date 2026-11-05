// SPDX-License-Identifier: GPL-3.0-or-later
//! The collections a command names, read before anything is resolved: tarballs opened, source
//! directories read, git repositories cloned, along with the git repositories they depend on.

use std::collections::HashMap;

use super::Failure;
use super::artifact::{Meta, Tarball};
use super::build::Source;
use super::requirements::{Collection, Kind};
use super::resolve::Request;

/// A collection asked for: a tarball read already, or a name and a range for the servers.
pub enum Wanted {
    Tarball(Tarball, Meta),
    Dir(Source),
    /// A collection of a git repository the command line or a file names.
    Git(Source),
    /// A collection of a git repository another collection depends on: nobody asked for it
    /// directly.
    GitDependency(Source),
    Galaxy {
        fqcn: String,
        range: String,
    },
}

impl Wanted {
    pub(super) fn request(&self) -> Option<Request> {
        match self {
            Self::Tarball(_, meta) => Some(Request::direct(&meta.fqcn(), &meta.version)),
            Self::Dir(source) | Self::Git(source) => {
                Some(Request::direct(&source.fqcn(), &source.version()))
            }
            Self::Galaxy { fqcn, range } => Some(Request::direct(fqcn, range)),
            Self::GitDependency(_) => None,
        }
    }

    fn dependencies(&self) -> Vec<(String, String)> {
        match self {
            Self::Tarball(_, meta) => meta.dependencies.clone(),
            Self::Dir(source) | Self::Git(source) | Self::GitDependency(source) => {
                source.dependencies()
            }
            Self::Galaxy { .. } => Vec::new(),
        }
    }

    fn set_dependencies(&mut self, dependencies: Vec<(String, String)>) {
        match self {
            Self::Tarball(_, meta) => meta.dependencies = dependencies,
            Self::Dir(source) | Self::Git(source) | Self::GitDependency(source) => {
                source.set_dependencies(dependencies);
            }
            Self::Galaxy { .. } => {}
        }
    }
}

/// The requirements the command line or a file gave, read: a tarball is opened and a git source
/// cloned now, so their errors come before any output, as the reference reads them while
/// parsing. A git repository that a collection depends on is cloned too, unless `no_deps`. The
/// clones are removed when the returned directories are dropped.
pub fn prepare(
    collections: &[Collection],
    ignore_certs: bool,
    no_deps: bool,
) -> Result<(Vec<Wanted>, Vec<tempfile::TempDir>), Failure> {
    let mut wanted = Vec::new();
    let mut clones = Vec::new();
    for c in collections {
        let source = c.source.as_deref().unwrap_or_default();
        let one = match c.kind {
            Kind::File => {
                let (tarball, meta) = Tarball::open(source)?;
                Wanted::Tarball(tarball, meta)
            }
            Kind::Dir => Wanted::Dir(Source::read(&super::resolve(source))?),
            Kind::Git => {
                let checkout = super::git::checkout(source, &c.version, ignore_certs)?;
                for dir in super::git::collections_in(&checkout.collection)? {
                    wanted.push(Wanted::Git(Source::read(&dir)?));
                }
                clones.push(checkout.clone);
                continue;
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
        };
        wanted.push(one);
    }
    if !no_deps {
        follow_git_dependencies(&mut wanted, &mut clones, ignore_certs)?;
    }
    Ok((wanted, clones))
}

/// Clones each git repository a collection of `wanted` depends on, once per URL, and those its
/// collections depend on, then names the dependencies by the collections they turned out to be.
/// Such a dependency takes any version: a repository holds the one it holds.
fn follow_git_dependencies(
    wanted: &mut Vec<Wanted>,
    clones: &mut Vec<tempfile::TempDir>,
    ignore_certs: bool,
) -> Result<(), Failure> {
    let mut named: HashMap<String, Vec<String>> = HashMap::new();
    let mut next = 0;
    while let Some(item) = wanted.get(next) {
        next += 1;
        let git = item.dependencies().into_iter();
        for (pointer, range) in git.filter(|(name, _)| super::requirements::is_git(name)) {
            if named.contains_key(&pointer) {
                continue;
            }
            let checkout = super::git::checkout(&pointer, &range, ignore_certs)?;
            let mut names = Vec::new();
            for dir in super::git::collections_in(&checkout.collection)? {
                let source = Source::read(&dir)?;
                names.push(source.fqcn());
                wanted.push(Wanted::GitDependency(source));
            }
            clones.push(checkout.clone);
            named.insert(pointer, names);
        }
    }
    for item in wanted.iter_mut() {
        let dependencies = item.dependencies();
        item.set_dependencies(
            dependencies
                .into_iter()
                .flat_map(|(name, range)| match named.get(&name) {
                    Some(names) => names.iter().map(|n| (n.clone(), "*".into())).collect(),
                    None => vec![(name, range)],
                })
                .collect(),
        );
    }
    Ok(())
}

/// The check the reference makes while it resolves, of the collections read from a directory.
pub(super) fn check_versions(wanted: &[Wanted]) -> Result<(), Failure> {
    for item in wanted {
        if let Wanted::Dir(source) | Wanted::Git(source) | Wanted::GitDependency(source) = item {
            source.check_version()?;
        }
    }
    Ok(())
}
