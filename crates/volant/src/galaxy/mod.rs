// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant galaxy`: the paths and checks the subcommands of ansible-galaxy 2.19.12
//! (`cli/galaxy.py`) share, with the reference's messages word for word.

use std::path::{Path, PathBuf};

pub mod artifact;
#[cfg(test)]
mod artifact_tests;
pub mod install;
#[cfg(test)]
mod requirement_tests;
pub mod requirements;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the install from a Galaxy server is its first caller"
    )
)]
pub mod resolve;
#[cfg(test)]
mod resolve_tests;
#[cfg(test)]
mod tests;
// `allow`, not `expect`: its only caller is the resolver, which older compilers count as a use
// although nothing calls the resolver yet.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "the install from a Galaxy server is its first caller"
    )
)]
pub mod version;

/// A failure, printed as `[ERROR]: <message>` and ending the run with `code`.
#[derive(Debug, PartialEq, Eq)]
pub struct Failure {
    pub message: String,
    pub code: i32,
}

impl Failure {
    /// An `AnsibleError` of the reference: exit 1.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 1,
        }
    }

    /// An `AnsibleOptionsError` of the reference: exit 5.
    pub fn options(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 5,
        }
    }
}

/// A path option as the reference reads it: `~` expanded, then absolute against the working
/// directory, `.` and `..` folded out.
pub fn resolve(path: &str) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_default();
    crate::roles::folded(&crate::config::expand_home(path, &cwd))
}

/// The `ansible_collections` directory a collections path stands for: the path itself when it
/// already ends with it (`validate_collection_path`).
pub fn collections_root(path: &Path) -> PathBuf {
    if path
        .file_name()
        .is_some_and(|name| name == "ansible_collections")
    {
        path.to_path_buf()
    } else {
        path.join("ansible_collections")
    }
}

/// The warning `collection install -p PATH` prints when no configured collections path lies
/// under `PATH`. The reference compares the texts, so `/a/b` covers `/a/bc` too. Its collection
/// finder also reads `PATH` once `PATH/ansible_collections` exists, and the warning then takes
/// `PATH` for part of the Python package.
pub fn collections_path_warning(path: &Path, configured: &[PathBuf]) -> Option<String> {
    let text = path.to_string_lossy();
    let managed = configured
        .iter()
        .any(|p| collections_root(p).to_string_lossy().starts_with(&*text));
    if managed {
        return None;
    }
    if collections_root(path).is_dir() {
        return Some(format!(
            "The specified collections path '{text}' appears to be part of the pip Ansible package. Managing these directly with ansible-galaxy could break the Ansible package. Install collections to a configured collections path, which will take precedence over collections found in the PYTHONPATH."
        ));
    }
    let joined: Vec<_> = configured.iter().map(|p| p.to_string_lossy()).collect();
    Some(format!(
        "The specified collections path '{text}' is not part of the configured Ansible collections paths '{}'. The installed collection will not be picked up in an Ansible run, unless within a playbook-adjacent collections directory.",
        joined.join(":")
    ))
}

/// The roles paths a role subcommand reads: `-p` first, then the configured ones. `role install`
/// and `role remove` act on the first.
pub fn roles_paths(option: Option<&str>, configured: &[PathBuf]) -> Vec<PathBuf> {
    option
        .map(resolve)
        .into_iter()
        .chain(configured.iter().cloned())
        .collect()
}

/// What `role list` says of `paths`: a warning for each one that does not exist, then the error
/// when none does.
pub fn missing_roles_paths(paths: &[PathBuf]) -> (Vec<String>, Option<Failure>) {
    let warnings: Vec<String> = paths
        .iter()
        .filter(|p| !p.exists())
        .map(|p| format!("- the configured path {} does not exist.", p.display()))
        .collect();
    let failure = (warnings.len() == paths.len()).then(|| {
        Failure::options(
            "- None of the provided paths were usable. Please specify a valid path with --roles-path",
        )
    });
    (warnings, failure)
}

/// A requirements file named by `-r`, once it exists.
pub fn requirements_file(path: &str) -> Result<PathBuf, Failure> {
    let resolved = resolve(path);
    if resolved.is_file() {
        Ok(resolved)
    } else {
        Err(Failure::error(format!(
            "The requirements file '{}' does not exist.",
            resolved.display()
        )))
    }
}

/// The collections or requirements file a `collection install`, `download` or `verify` works
/// from: one or the other, as the reference checks before anything else.
pub fn one_of_collections_requirements(
    names: &[String],
    requirements: Option<&str>,
) -> Result<Option<PathBuf>, Failure> {
    match (names.is_empty(), requirements) {
        (false, Some(_)) => Err(Failure::error(
            "The positional collection_name arg and --requirements-file are mutually exclusive.",
        )),
        (true, None) => Err(Failure::error(
            "You must specify a collection name or a requirements file.",
        )),
        (_, Some(file)) => requirements_file(file).map(Some),
        (false, None) => Ok(None),
    }
}

/// The roles or requirements file a `role install` works from.
pub fn one_of_roles_requirements(
    names: &[String],
    requirements: Option<&str>,
) -> Result<Option<PathBuf>, Failure> {
    match requirements {
        None if names.is_empty() => Err(Failure::options(
            "- you must specify a user/role name or a roles file",
        )),
        None => Ok(None),
        Some(file) if !(file.ends_with(".yml") || file.ends_with(".yaml")) => Err(Failure::error(
            "Invalid role requirements file, it must end with a .yml or .yaml extension",
        )),
        Some(file) => requirements_file(file).map(Some),
    }
}
