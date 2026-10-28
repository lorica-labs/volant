// SPDX-License-Identifier: GPL-3.0-or-later
//! A collection in a git repository, as ansible-galaxy 2.19.12 fetches one (`parse_scm`,
//! `_extract_collection_from_git`): `git+<url>[,<ref>][#/<subdir>]`, cloned with the system
//! `git`, checked out at the ref, then installed as a source directory. git's own output is shown
//! at `-vvv` only, since its wording changes between git versions.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::Failure;

/// A clone, removed when dropped, and the directory in it that holds the collection.
pub struct Checkout {
    pub clone: tempfile::TempDir,
    pub collection: PathBuf,
}

/// `parse_scm`: the name, the ref, the URL git is given, and the subdirectory.
pub fn parse(source: &str, version: &str) -> (String, String, String, String) {
    let (pointer, version) = match source.split_once(',') {
        Some((pointer, version)) => (pointer, version.to_string()),
        None if version == "*" || version.is_empty() => (source, "HEAD".to_string()),
        None => (source, version.to_string()),
    };
    let path = pointer.strip_prefix("git+").unwrap_or(pointer);
    let (path, fragment) = path.split_once('#').unwrap_or((path, ""));
    let fragment = fragment.trim_matches('/').to_string();
    let name = if path.ends_with("/.git") {
        path.rsplit('/').nth(1).unwrap_or_default().to_string()
    } else if !path.contains("://") && !path.contains('@') {
        path.to_string()
    } else {
        let last = path.rsplit('/').next().unwrap_or_default();
        last.strip_suffix(".git").unwrap_or(last).to_string()
    };
    (name, version, path.to_string(), fragment)
}

/// The URL with any password replaced, for a message.
pub fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let (authority, tail) = rest.split_once('/').map_or((rest, ""), |(a, t)| (a, t));
    match authority.rsplit_once('@') {
        Some((credentials, host)) if credentials.contains(':') => {
            let user = credentials.split(':').next().unwrap_or_default();
            let tail = if rest.contains('/') {
                format!("/{tail}")
            } else {
                String::new()
            };
            format!("{scheme}://{user}:********@{host}{tail}")
        }
        _ => url.to_string(),
    }
}

/// Clones `source` and checks out its ref.
pub fn checkout(
    source: &str,
    version: &str,
    ignore_certs: bool,
    verbose: u8,
) -> Result<Checkout, Failure> {
    let (name, version, url, fragment) = parse(source, version);
    let shown = redact(&url);
    let Some(git) = find("git") else {
        return Err(Failure::caused(
            format!(
                "Could not find git executable to extract the collection from the Git repository `{shown}`"
            ),
            format!(
                "Failed to find required executable \"git\" in paths: {}",
                search_path().join(":")
            ),
        ));
    };
    let clone = tempfile::Builder::new()
        .prefix(&name)
        .tempdir()
        .map_err(|e| Failure::error(e.to_string()))?;
    let target = clone.path().display().to_string();
    let git_text = git.display().to_string();
    let mut argv = vec![git_text.clone(), "clone".to_string()];
    if version == "HEAD" {
        argv.push("--depth=1".into());
    }
    argv.extend([url.clone(), target]);
    if ignore_certs {
        argv.extend(["-c".to_string(), "http.sslVerify=false".to_string()]);
    }
    run(&argv, None, verbose).map_err(|status| {
        Failure::caused(
            format!("Failed to clone a Git repository from `{shown}`"),
            exit_text(&argv, status, &url, &shown, false),
        )
    })?;
    let switch = vec![git_text, "checkout".to_string(), version.clone()];
    run(&switch, Some(clone.path()), verbose).map_err(|status| {
        Failure::caused(
            format!(
                "Failed to switch a cloned Git repo `{shown}` to the requested revision `{version}`"
            ),
            exit_text(&switch, status, &url, &shown, true),
        )
    })?;
    let collection = if fragment.is_empty() {
        clone.path().to_path_buf()
    } else {
        clone.path().join(&fragment)
    };
    Ok(Checkout { clone, collection })
}

/// Runs `argv`, its output shown at `-vvv` only; `Err` holds the exit code.
fn run(argv: &[String], cwd: Option<&Path>, verbose: u8) -> Result<(), i32> {
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).stdin(Stdio::null());
    if verbose < 3 {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    match command.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(status.code().unwrap_or(1)),
        Err(_) => Err(1),
    }
}

/// `CalledProcessError`'s text, with the URL's password replaced. The reference passes the
/// clone command as a list and the checkout as a tuple, which Python prints differently.
fn exit_text(argv: &[String], status: i32, url: &str, shown: &str, tuple: bool) -> String {
    let list: Vec<String> = argv
        .iter()
        .map(|a| format!("'{}'", a.replace(url, shown)))
        .collect();
    let (open, close) = if tuple { ('(', ')') } else { ('[', ']') };
    format!(
        "Command '{open}{}{close}' returned non-zero exit status {status}.",
        list.join(", ")
    )
}

/// The directories `get_bin_path` searches: `PATH`, then the `sbin` ones it adds.
fn search_path() -> Vec<String> {
    let mut paths: Vec<String> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    for extra in ["/sbin", "/usr/sbin", "/usr/local/sbin"] {
        if !paths.iter().any(|p| p == extra) {
            paths.push(extra.into());
        }
    }
    paths
}

fn find(program: &str) -> Option<PathBuf> {
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{program}.exe"), program.to_string()]
    } else {
        vec![program.to_string()]
    };
    let paths = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    paths
        .iter()
        .map(PathBuf::as_path)
        .chain(["/sbin", "/usr/sbin", "/usr/local/sbin"].map(Path::new))
        .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
        .find(|candidate| candidate.is_file())
}
