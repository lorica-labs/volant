// SPDX-License-Identifier: GPL-3.0-or-later
//! The `galaxy` command: the options of `ansible-galaxy` 2.19.12, its install directories and the
//! checks it makes before any work. The subcommands themselves are refused until they are native.

mod args;

pub use args::GalaxyArgs;
use args::{Collection, Kind, Role};

use crate::config::Config;
use crate::galaxy::api::servers::Servers;
use crate::galaxy::api::{Files, Server};
use crate::galaxy::build::Source;
use crate::galaxy::requirements::{self, Requirements};
use crate::galaxy::settings::{self, Settings};
use crate::galaxy::{self, Failure};
use crate::galaxy::{download, install, inventory};

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
            eprintln!("[ERROR]: {}", failure.shown());
            failure.code
        }
    }
}

/// The checks the reference makes before the subcommand's work, then the refusal of that work.
#[expect(clippy::too_many_lines)]
fn run(kind: &Kind, verbose: u8) -> Result<i32, Failure> {
    let config = Config::load().map_err(|e| Failure::error(e.to_string()))?;
    let name = match kind {
        Kind::Collection(Collection::Install(install)) => {
            return collection_install(install, &config, verbose);
        }
        Kind::Collection(Collection::Download {
            common,
            cache,
            no_deps,
            download_path,
            requirements,
            pre,
            collection,
        }) => {
            let options = install::Options {
                no_deps: *no_deps,
                pre: *pre,
                ..install::Options::default()
            };
            let cache = (cache.no_cache, cache.clear_response_cache);
            return collection_download(
                common,
                (collection, requirements.as_deref()),
                download_path.as_deref(),
                (options, cache),
                &config,
                verbose,
            );
        }
        Kind::Collection(Collection::Verify {
            common,
            collections_path,
            ignore_errors,
            offline,
            requirements,
            collection_name,
            ..
        }) => {
            let flags = (*offline, *ignore_errors);
            return collection_verify(
                common,
                collections_path,
                requirements.as_deref(),
                collection_name,
                flags,
                &config,
                verbose,
            );
        }
        Kind::Collection(Collection::Init { .. }) => "collection init",
        Kind::Collection(Collection::Build {
            force,
            output_path,
            collection,
            ..
        }) => return collection_build(collection, output_path.as_deref(), *force, verbose),
        Kind::Collection(Collection::List {
            collections_path,
            format,
            collection,
            ..
        }) => {
            let given: Vec<_> = collections_path
                .iter()
                .map(|p| galaxy::resolve(p))
                .collect();
            return inventory::list(
                &given,
                &configured_collections(&config),
                collection.as_deref(),
                format.as_deref().unwrap_or("human"),
            );
        }
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

/// `collection build`: each source tree, `.` by default, built into `--output-path`, `./` by
/// default (`execute_build`).
fn collection_build(
    sources: &[String],
    output: Option<&str>,
    force: bool,
    verbose: u8,
) -> Result<i32, Failure> {
    let output = galaxy::resolve(output.unwrap_or("./"));
    if output.is_file() {
        return Err(Failure::error(format!(
            "- the output collection directory {} is a file - aborting",
            output.display()
        )));
    }
    std::fs::create_dir_all(&output).map_err(|e| Failure::error(e.to_string()))?;
    let here = [".".to_string()];
    let sources = if sources.is_empty() {
        &here[..]
    } else {
        sources
    };
    for source in sources {
        Source::read(&galaxy::resolve(source))?.build(&output, force, verbose)?;
    }
    Ok(0)
}

/// `collection verify`: the collections named or listed, checked against their manifests or
/// the servers' copies (`execute_verify`).
fn collection_verify(
    common: &args::Common,
    collections_path: &[String],
    requirements: Option<&str>,
    collection_name: &[String],
    (offline, ignore_errors): (bool, bool),
    config: &Config,
    verbose: u8,
) -> Result<i32, Failure> {
    let wanted = match galaxy::one_of_collections_requirements(collection_name, requirements)? {
        Some(file) => requirements_of(&file, false, verbose)?.collections,
        None => collection_name
            .iter()
            .map(|name| requirements::from_string(name))
            .collect::<Result<_, _>>()?,
    };
    let mut paths: Vec<_> = collections_path
        .iter()
        .map(|p| galaxy::resolve(p))
        .collect();
    for path in configured_collections(config) {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    // `verify` has no cache options, and the reference runs it without the response cache.
    let mut servers = servers(common, (true, false), config, verbose, offline);
    let options = inventory::Verify {
        paths: &paths,
        offline,
        ignore_errors,
    };
    inventory::verify(&wanted, &mut servers, &options)
}

/// `collection download`: the collections resolved and saved with a `requirements.yml`
/// (`execute_download`).
fn collection_download(
    common: &args::Common,
    (collection, requirements): (&[String], Option<&str>),
    download_path: Option<&str>,
    (options, cache): (install::Options, (bool, bool)),
    config: &Config,
    verbose: u8,
) -> Result<i32, Failure> {
    let wanted = match galaxy::one_of_collections_requirements(collection, requirements)? {
        Some(file) => requirements_of(&file, false, verbose)?.collections,
        None => collection
            .iter()
            .map(|name| requirements::from_string(name))
            .collect::<Result<_, _>>()?,
    };
    let (wanted, _clones) = install::prepare(&wanted, common.ignore_certs, verbose)?;
    let output = galaxy::resolve(download_path.unwrap_or("./collections"));
    std::fs::create_dir_all(&output).map_err(|e| Failure::error(e.to_string()))?;
    let mut servers = servers(common, cache, config, verbose, false);
    download::collections(wanted, &output, &mut servers, options, verbose)
}

/// `collection install`: tarballs and names from Galaxy servers. Directories, URLs, git and
/// signatures are refused until they are native.
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
    let (wanted, _clones) = install::prepare(&wanted, install.common.ignore_certs, verbose)?;
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
        no_deps: install.no_deps,
        pre: install.pre,
    };
    let mut servers = servers(
        &install.common,
        (install.cache.no_cache, install.cache.clear_response_cache),
        config,
        verbose,
        install.offline,
    );
    install::collections(
        wanted,
        &galaxy::collections_root(&path),
        &mut servers,
        options,
        verbose,
    )
}

/// The configured collections paths, without the runtime's own directory, which Volant appends
/// and the reference would not name.
fn configured_collections(config: &Config) -> Vec<std::path::PathBuf> {
    let runtime = crate::runtime::collections::active_dir();
    config
        .collections_path
        .iter()
        .filter(|p| Some(*p) != runtime.as_ref())
        .cloned()
        .collect()
}

/// The servers a run asks, from the options, the environment and `ansible.cfg`. None under
/// `--offline`.
fn servers(
    common: &args::Common,
    (no_cache, clear_cache): (bool, bool),
    config: &Config,
    verbose: u8,
    offline: bool,
) -> Servers {
    if offline {
        return Servers { list: Vec::new() };
    }
    let env = |name: &str| std::env::var(name).ok();
    let settings = Settings::new(&config.galaxy, &env);
    let options = settings::Options {
        server: common.server.as_deref(),
        token: common.token.as_deref(),
        ignore_certs: common.ignore_certs,
        timeout: common.timeout,
    };
    let cache_dir = settings.cache_dir();
    let token_path = settings.token_path();
    let files = Files {
        cache_dir: (!no_cache).then_some(cache_dir.as_path()),
        clear_cache,
        token_path: &token_path,
    };
    Servers {
        list: settings
            .servers(&options)
            .iter()
            .map(|s| Server::new(s, &files, verbose))
            .collect(),
    }
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
