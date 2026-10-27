// SPDX-License-Identifier: GPL-3.0-or-later
//! The `galaxy` command: the options of `ansible-galaxy` 2.19.12, its install directories and the
//! checks it makes before any work. The subcommands themselves are refused until they are native.

use clap::{ArgAction, Args, Parser, Subcommand};

use crate::config::Config;
use crate::galaxy::requirements::{self, Requirements};
use crate::galaxy::{self, Failure};

/// Install and list roles and collections, with the arguments of ansible-galaxy.
#[derive(Parser, Debug)]
#[command(disable_version_flag = true, subcommand_required = true)]
pub struct GalaxyArgs {
    /// Show the program's version, configuration file and search paths, and exit.
    #[arg(long)]
    version: bool,
    /// More output; repeat for more. Taken before or after the subcommand.
    #[arg(short = 'v', long = "verbose", action = ArgAction::Count, global = true)]
    verbose: u8,
    #[command(subcommand)]
    kind: Option<Kind>,
}

#[derive(Subcommand, Debug)]
enum Kind {
    /// Manage an Ansible Galaxy collection.
    #[command(subcommand)]
    Collection(Collection),
    /// Manage an Ansible Galaxy role.
    #[command(subcommand)]
    Role(Role),
}

/// The options every subcommand takes.
#[derive(Args, Debug)]
struct Common {
    /// The Galaxy API server URL.
    #[arg(short = 's', long = "server", value_name = "API_SERVER")]
    server: Option<String>,
    /// The Ansible Galaxy API key.
    #[arg(long = "token", visible_alias = "api-key", value_name = "API_KEY")]
    token: Option<String>,
    /// Ignore SSL certificate validation errors.
    #[arg(short = 'c', long = "ignore-certs")]
    ignore_certs: bool,
    /// The time to wait for operations against the galaxy server, defaults to 60s.
    #[arg(long, value_name = "TIMEOUT")]
    timeout: Option<u64>,
}

/// The server response cache options.
#[derive(Args, Debug)]
struct Cache {
    /// Clear the existing server response cache.
    #[arg(long)]
    clear_response_cache: bool,
    /// Do not use the server response cache.
    #[arg(long)]
    no_cache: bool,
}

const STATUS_CODES: [&str; 16] = [
    "EXPSIG",
    "EXPKEYSIG",
    "REVKEYSIG",
    "BADSIG",
    "ERRSIG",
    "NO_PUBKEY",
    "MISSING_PASSPHRASE",
    "BAD_PASSPHRASE",
    "NODATA",
    "UNEXPECTED",
    "ERROR",
    "FAILURE",
    "BADARMOR",
    "KEYEXPIRED",
    "KEYREVOKED",
    "NO_SECKEY",
];

/// The GnuPG signature options.
#[derive(Args, Debug)]
struct Signatures {
    /// The keyring used during signature verification.
    #[arg(long, value_name = "KEYRING")]
    keyring: Option<String>,
    /// An additional signature source to verify the authenticity of the MANIFEST.json.
    #[arg(long = "signature", value_name = "SIGNATURES", action = ArgAction::Append)]
    signatures: Vec<String>,
    /// The number of signatures that must successfully verify the collection.
    #[arg(long, value_name = "REQUIRED_VALID_SIGNATURE_COUNT")]
    required_valid_signature_count: Option<String>,
    /// A status code to ignore during signature verification.
    #[arg(long, value_parser = STATUS_CODES, action = ArgAction::Append)]
    ignore_signature_status_code: Vec<String>,
    /// Status codes to ignore during signature verification.
    #[arg(long, value_parser = STATUS_CODES, num_args = 1.., action = ArgAction::Append)]
    ignore_signature_status_codes: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum Collection {
    /// Download collections and their dependencies as a tarball for an offline install.
    #[command(args_override_self = true)]
    Download {
        #[command(flatten)]
        common: Common,
        #[command(flatten)]
        cache: Cache,
        /// Don't download collection(s) listed as dependencies.
        #[arg(short = 'n', long)]
        no_deps: bool,
        /// The directory to download the collections to.
        #[arg(short = 'p', long, value_name = "DOWNLOAD_PATH")]
        download_path: Option<String>,
        /// A file containing a list of collections to be downloaded.
        #[arg(short = 'r', long = "requirements-file", value_name = "REQUIREMENTS")]
        requirements: Option<String>,
        /// Include pre-release versions.
        #[arg(long)]
        pre: bool,
        collection: Vec<String>,
    },
    /// Initialize new collection with the base structure of a collection.
    #[command(args_override_self = true)]
    Init {
        #[command(flatten)]
        common: Common,
        /// Force overwriting an existing collection.
        #[arg(short = 'f', long)]
        force: bool,
        /// The path in which the skeleton collection will be created.
        #[arg(long, value_name = "INIT_PATH")]
        init_path: Option<String>,
        /// The path to a collection skeleton that the new collection should be based upon.
        #[arg(long, value_name = "COLLECTION_SKELETON")]
        collection_skeleton: Option<String>,
        /// Extra variables for the skeleton templates.
        #[arg(short = 'e', long = "extra-vars", value_name = "EXTRA_VARS", action = ArgAction::Append)]
        extra_vars: Vec<String>,
        collection_name: String,
    },
    /// Build an Ansible collection artifact that can be published to Ansible Galaxy.
    #[command(args_override_self = true)]
    Build {
        #[command(flatten)]
        common: Common,
        /// Force overwriting an existing collection artifact.
        #[arg(short = 'f', long)]
        force: bool,
        /// The path in which the collection is built to.
        #[arg(long, value_name = "OUTPUT_PATH")]
        output_path: Option<String>,
        collection: Vec<String>,
    },
    /// Publish a collection artifact to Ansible Galaxy.
    #[command(args_override_self = true)]
    Publish {
        #[command(flatten)]
        common: Common,
        /// Don't wait for import validation results.
        #[arg(long)]
        no_wait: bool,
        /// The time to wait for the collection import process to finish.
        #[arg(long, value_name = "IMPORT_TIMEOUT")]
        import_timeout: Option<String>,
        collection_path: String,
    },
    /// Install collection(s) from file(s), URL(s) or Ansible Galaxy.
    #[command(args_override_self = true)]
    Install(Box<CollectionInstall>),
    /// Show the name and version of each collection installed in the collections_path.
    #[command(args_override_self = true)]
    List {
        #[command(flatten)]
        common: Common,
        /// One or more directories to search for collections.
        #[arg(short = 'p', long = "collections-path", value_name = "COLLECTIONS_PATH", action = ArgAction::Append)]
        collections_path: Vec<String>,
        /// Format to display the list of collections in.
        #[arg(long, value_parser = ["human", "yaml", "json"])]
        format: Option<String>,
        collection: Option<String>,
    },
    /// Compare checksums with the collection(s) found on the server and the installed copy.
    #[command(args_override_self = true)]
    Verify {
        #[command(flatten)]
        common: Common,
        /// One or more directories to search for collections.
        #[arg(short = 'p', long = "collections-path", value_name = "COLLECTIONS_PATH", action = ArgAction::Append)]
        collections_path: Vec<String>,
        /// Ignore errors during verification and continue with the next specified collection.
        #[arg(short = 'i', long)]
        ignore_errors: bool,
        /// Validate collection integrity locally without contacting server.
        #[arg(long)]
        offline: bool,
        /// A file containing a list of collections to be verified.
        #[arg(short = 'r', long = "requirements-file", value_name = "REQUIREMENTS")]
        requirements: Option<String>,
        #[command(flatten)]
        signatures: Signatures,
        collection_name: Vec<String>,
    },
}

#[derive(Args, Debug)]
struct CollectionInstall {
    #[command(flatten)]
    common: Common,
    /// Force overwriting an existing collection.
    #[arg(short = 'f', long)]
    force: bool,
    #[command(flatten)]
    cache: Cache,
    /// Ignore errors during installation and continue with the next specified collection.
    #[arg(short = 'i', long)]
    ignore_errors: bool,
    /// Don't download collections listed as dependencies.
    #[arg(short = 'n', long, conflicts_with = "force_with_deps")]
    no_deps: bool,
    /// Force overwriting an existing collection and its dependencies.
    #[arg(long)]
    force_with_deps: bool,
    /// The path to the directory containing your collections.
    #[arg(
        short = 'p',
        long = "collections-path",
        value_name = "COLLECTIONS_PATH"
    )]
    collections_path: Option<String>,
    /// A file containing a list of collections to be installed.
    #[arg(short = 'r', long = "requirements-file", value_name = "REQUIREMENTS")]
    requirements: Option<String>,
    /// Include pre-release versions.
    #[arg(long)]
    pre: bool,
    /// Upgrade installed collection artifacts.
    #[arg(short = 'U', long)]
    upgrade: bool,
    /// Disable GPG signature verification when installing collections from a Galaxy server.
    #[arg(long)]
    disable_gpg_verify: bool,
    #[command(flatten)]
    signatures: Signatures,
    /// Install collection artifacts (tarballs) without contacting any distribution servers.
    #[arg(long)]
    offline: bool,
    collection_name: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum Role {
    /// Initialize new role with the base structure of a role.
    #[command(args_override_self = true)]
    Init {
        #[command(flatten)]
        common: Common,
        /// Force overwriting an existing role.
        #[arg(short = 'f', long)]
        force: bool,
        /// Don't query the galaxy API when creating roles.
        #[arg(long)]
        offline: bool,
        /// The path in which the skeleton role will be created.
        #[arg(long, value_name = "INIT_PATH")]
        init_path: Option<String>,
        /// The path to a role skeleton that the new role should be based upon.
        #[arg(long, value_name = "ROLE_SKELETON")]
        role_skeleton: Option<String>,
        /// Initialize using an alternate role type.
        #[arg(long = "type", value_name = "ROLE_TYPE")]
        role_type: Option<String>,
        /// Extra variables for the skeleton templates.
        #[arg(short = 'e', long = "extra-vars", value_name = "EXTRA_VARS", action = ArgAction::Append)]
        extra_vars: Vec<String>,
        role_name: String,
    },
    /// Delete roles from roles_path.
    #[command(args_override_self = true)]
    Remove {
        #[command(flatten)]
        common: Common,
        /// The path to the directory containing your roles.
        #[arg(short = 'p', long = "roles-path", value_name = "ROLES_PATH")]
        roles_path: Option<String>,
        #[arg(required = true)]
        role: Vec<String>,
    },
    /// Removes the role from Galaxy. It does not remove or alter the actual GitHub repository.
    #[command(args_override_self = true)]
    Delete {
        #[command(flatten)]
        common: Common,
        github_user: String,
        github_repo: String,
    },
    /// Show the name and version of each role installed in the roles_path.
    #[command(args_override_self = true)]
    List {
        #[command(flatten)]
        common: Common,
        /// The path to the directory containing your roles.
        #[arg(short = 'p', long = "roles-path", value_name = "ROLES_PATH")]
        roles_path: Option<String>,
        role: Option<String>,
    },
    /// Search the Galaxy database by tags, platforms, author and multiple keywords.
    #[command(args_override_self = true)]
    Search {
        #[command(flatten)]
        common: Common,
        /// List of OS platforms to filter by.
        #[arg(long, value_name = "PLATFORMS")]
        platforms: Option<String>,
        /// List of galaxy tags to filter by.
        #[arg(long, value_name = "GALAXY_TAGS")]
        galaxy_tags: Option<String>,
        /// GitHub username.
        #[arg(long, value_name = "AUTHOR")]
        author: Option<String>,
        searchterm: Vec<String>,
    },
    /// Import a role into a galaxy server.
    #[command(args_override_self = true)]
    Import {
        #[command(flatten)]
        common: Common,
        /// Don't wait for import results.
        #[arg(long)]
        no_wait: bool,
        /// The name of a branch to import.
        #[arg(long, value_name = "REFERENCE")]
        branch: Option<String>,
        /// The name the role should have, if different than the repo name.
        #[arg(long, value_name = "ROLE_NAME")]
        role_name: Option<String>,
        /// Check the status of the most recent import request for given github_user/github_repo.
        #[arg(long)]
        status: bool,
        github_user: String,
        github_repo: String,
    },
    /// Manage the integration between Galaxy and the given source.
    #[command(args_override_self = true)]
    Setup {
        #[command(flatten)]
        common: Common,
        /// The path to the directory containing your roles.
        #[arg(short = 'p', long = "roles-path", value_name = "ROLES_PATH")]
        roles_path: Option<String>,
        /// Remove the integration matching the provided ID value.
        #[arg(long = "remove", value_name = "REMOVE_ID")]
        remove: Option<String>,
        /// List all of your integrations.
        #[arg(long)]
        list: bool,
        source: String,
        github_user: String,
        github_repo: String,
        secret: String,
    },
    /// View more details about a specific role.
    #[command(args_override_self = true)]
    Info {
        #[command(flatten)]
        common: Common,
        /// The path to the directory containing your roles.
        #[arg(short = 'p', long = "roles-path", value_name = "ROLES_PATH")]
        roles_path: Option<String>,
        /// Don't query the galaxy API when creating roles.
        #[arg(long)]
        offline: bool,
        #[arg(value_name = "role_name[,version]", required = true)]
        role: Vec<String>,
    },
    /// Install role(s) from file(s), URL(s) or Ansible Galaxy.
    #[command(args_override_self = true)]
    Install {
        #[command(flatten)]
        common: Common,
        /// Force overwriting an existing role.
        #[arg(short = 'f', long)]
        force: bool,
        /// The path to the directory containing your roles.
        #[arg(short = 'p', long = "roles-path", value_name = "ROLES_PATH")]
        roles_path: Option<String>,
        /// Ignore errors and continue with the next specified role.
        #[arg(short = 'i', long)]
        ignore_errors: bool,
        /// Don't download roles listed as dependencies.
        #[arg(short = 'n', long, conflicts_with = "force_with_deps")]
        no_deps: bool,
        /// Force overwriting an existing role and its dependencies.
        #[arg(long)]
        force_with_deps: bool,
        /// A file containing a list of roles to be installed.
        #[arg(short = 'r', long = "role-file", value_name = "REQUIREMENTS")]
        requirements: Option<String>,
        /// Use tar instead of the scm archive option when packaging the role.
        #[arg(short = 'g', long)]
        keep_scm_meta: bool,
        role_name: Vec<String>,
    },
}

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
            if let Some(file) = galaxy::one_of_collections_requirements(
                &install.collection_name,
                install.requirements.as_deref(),
            )? && requirements_of(&file, false, verbose)?
                .collections
                .is_empty()
            {
                println!("Skipping install, no requirements found");
                return Ok(0);
            }
            // The runtime's own directory, which Volant appends to the search path, is not one
            // the reference would name.
            let runtime = crate::runtime::collections::active_dir();
            let configured: Vec<_> = config
                .collections_path
                .iter()
                .filter(|p| Some(*p) != runtime.as_ref())
                .cloned()
                .collect();
            if let Some(path) = &install.collections_path
                && let Some(warning) =
                    galaxy::collections_path_warning(&galaxy::resolve(path), &configured)
            {
                eprintln!("[WARNING]: {warning}");
            }
            "collection install"
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
