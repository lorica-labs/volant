// SPDX-License-Identifier: GPL-3.0-or-later
//! The `galaxy` command: the options of `ansible-galaxy` 2.19.12, its install directories and the
//! checks it makes before any work. The subcommands themselves are refused until they are native.

mod args;

pub use args::GalaxyArgs;
use args::{Collection, Kind, Role};

use crate::config::Config;
use crate::galaxy::artifact::{Meta, Tarball};
use crate::galaxy::install;
use crate::galaxy::requirements::{self, Requirements};
use crate::galaxy::{self, Failure};

/// Runs `args` and returns the exit code.
#[must_use]
pub fn galaxy(args: GalaxyArgs) -> i32 {
    if args.version {
        println!("{}", super::version::block("ansible-galaxy", &[]));
        return 0;
    }
    let Some(kind) = args.kind else {
        return 2;
    };
    match run(&kind, args.verbose) {
        Ok(code) => code,
        Err(failure) => {
            eprintln!("[ERROR]: {}", failure.message);
            failure.code
        }
    }
}

/// The checks the reference makes before the subcommand's work, then the refusal of that work.
fn run(kind: &Kind, verbose: u8) -> Result<i32, Failure> {
    let config = Config::load().map_err(|e| Failure::error(e.to_string()))?;
    let name = match kind {
        Kind::Collection(Collection::Install(install)) => {
            return collection_install(install, &config, verbose);
        }
        Kind::Collection(Collection::Download {
            collection,
            requirements,
            ..
        }) => {
            if let Some(file) =
                galaxy::one_of_collections_requirements(collection, requirements.as_deref())?
            {
                requirements_of(&file, false, verbose)?;
            }
            "collection download"
        }
        Kind::Collection(Collection::Verify { requirements, .. }) => {
            if let Some(file) = requirements {
                requirements_of(&galaxy::requirements_file(file)?, false, verbose)?;
            }
            "collection verify"
        }
        Kind::Collection(Collection::Init { .. }) => "collection init",
        Kind::Collection(Collection::Build { .. }) => "collection build",
        Kind::Collection(Collection::List { .. }) => "collection list",
        Kind::Role(Role::Install {
            role_name,
            requirements,
            ..
        }) => {
            if let Some(file) =
                galaxy::one_of_roles_requirements(role_name, requirements.as_deref())?
                && requirements_of(&file, true, verbose)?.roles.is_empty()
            {
                println!("Skipping install, no requirements found");
                return Ok(0);
            }
            "role install"
        }
        Kind::Role(Role::List { roles_path, .. }) => {
            let paths = galaxy::roles_paths(roles_path.as_deref(), &config.roles_path);
            let (warnings, failure) = galaxy::missing_roles_paths(&paths);
            for warning in warnings {
                eprintln!("[WARNING]: {warning}");
            }
            if let Some(failure) = failure {
                return Err(failure);
            }
            "role list"
        }
        Kind::Role(Role::Init { .. }) => "role init",
        Kind::Role(Role::Remove { .. }) => "role remove",
        Kind::Collection(Collection::Publish { .. }) => return Err(refused("collection publish")),
        Kind::Role(Role::Delete { .. }) => return Err(refused("role delete")),
        Kind::Role(Role::Search { .. }) => return Err(refused("role search")),
        Kind::Role(Role::Import { .. }) => return Err(refused("role import")),
        Kind::Role(Role::Setup { .. }) => return Err(refused("role setup")),
        Kind::Role(Role::Info { .. }) => return Err(refused("role info")),
    };
    Err(Failure::error(format!(
        "volant galaxy {name} is not supported yet"
    )))
}

/// `collection install`: tarballs natively. Every other source, a dependency to resolve and a
/// signature to check are refused until they are native.
fn collection_install(
    install: &args::CollectionInstall,
    config: &Config,
    verbose: u8,
) -> Result<i32, Failure> {
    let wanted = match galaxy::one_of_collections_requirements(
        &install.collection_name,
        install.requirements.as_deref(),
    )? {
        Some(file) => requirements_of(&file, false, verbose)?.collections,
        None => install
            .collection_name
            .iter()
            .map(|name| requirements::from_string(name))
            .collect::<Result<_, _>>()?,
    };
    if wanted.is_empty() {
        println!("Skipping install, no requirements found");
        return Ok(0);
    }
    let not_yet = |what: &str| {
        Failure::error(format!(
            "volant galaxy collection install {what} is not supported yet"
        ))
    };
    if install.signatures.keyring.is_some() || !install.signatures.signatures.is_empty() {
        return Err(not_yet("with signatures"));
    }
    let mut tarballs: Vec<(Tarball, Meta)> = Vec::new();
    for wanted in &wanted {
        if wanted.kind != requirements::Kind::File {
            return Err(not_yet("from a Galaxy server, a directory, a URL or git"));
        }
        let (tarball, meta) = Tarball::open(wanted.source.as_deref().unwrap_or_default())?;
        if !install.no_deps && !meta.dependencies.is_empty() {
            return Err(not_yet("of a collection with dependencies"));
        }
        if tarballs.iter().any(|(_, m)| m.fqcn() == meta.fqcn()) {
            return Err(not_yet("of two versions of one collection"));
        }
        tarballs.push((tarball, meta));
    }
    // The runtime's own directory, which Volant appends to the search path, is not one the
    // reference would name.
    let runtime = crate::runtime::collections::active_dir();
    let configured: Vec<_> = config
        .collections_path
        .iter()
        .filter(|p| Some(*p) != runtime.as_ref())
        .cloned()
        .collect();
    let path = match &install.collections_path {
        Some(path) => {
            let path = galaxy::resolve(path);
            if let Some(warning) = galaxy::collections_path_warning(&path, &configured) {
                eprintln!("[WARNING]: {warning}");
            }
            path
        }
        None => configured.first().cloned().unwrap_or_default(),
    };
    let options = install::Options {
        force: install.force,
        force_with_deps: install.force_with_deps,
        ignore_errors: install.ignore_errors,
        upgrade: install.upgrade,
    };
    install::collections(&tarballs, &galaxy::collections_root(&path), options)
}

/// The requirements file `-r` names, announced at `-vvv` as the reference does.
fn requirements_of(
    file: &std::path::Path,
    old_format: bool,
    verbose: u8,
) -> Result<Requirements, Failure> {
    if verbose >= 3 {
        println!("Reading requirement file at '{}'", file.display());
    }
    requirements::read(file, old_format)
}

/// A subcommand that needs a Galaxy account or server-side data, which stays out of Volant.
fn refused(name: &str) -> Failure {
    Failure::error(format!("volant galaxy {name} is not supported"))
}
