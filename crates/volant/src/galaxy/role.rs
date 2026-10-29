// SPDX-License-Identifier: GPL-3.0-or-later
//! `role install`, `role list` and `role remove`, as ansible-galaxy 2.19.12 runs them
//! (`_execute_install_role`, `GalaxyRole.install`, `execute_list_role`, `execute_remove`).
//! A role comes from a Galaxy server's API v1, a tarball, a URL or a git repository; its archive's
//! top directory is dropped, and `meta/.galaxy_install_info` records the version.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::Failure;
use super::api::Server;
use super::api::servers::Servers;
use super::requirements::{Role as Wanted, role as parse_role};

mod archive;
mod listing;

use archive::{extract, scm_archive};
pub use listing::{list, remove};

/// The options of `role install`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Options {
    pub force: bool,
    pub force_with_deps: bool,
    pub no_deps: bool,
    pub ignore_errors: bool,
    pub keep_scm_meta: bool,
    pub verbose: u8,
}

/// A role being installed: what the requirement names and where it goes.
#[derive(Clone)]
struct Role {
    name: String,
    src: String,
    version: Option<String>,
    scm: Option<String>,
    path: PathBuf,
}

impl Role {
    fn new(wanted: &Wanted, roles_path: &Path) -> Self {
        let name = wanted.name.clone().unwrap_or_default();
        Self {
            src: wanted
                .src
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| name.clone()),
            version: wanted.version.clone(),
            scm: wanted.scm.clone(),
            path: roles_path.join(&name),
            name,
        }
    }

    /// `rolename (version)`, or the name alone.
    fn shown(&self) -> String {
        match self.version.as_deref() {
            Some(v) if !v.is_empty() => format!("{} ({v})", self.name),
            _ => self.name.clone(),
        }
    }

    fn version_or_unspecified(&self) -> &str {
        self.version
            .as_deref()
            .filter(|v| !v.is_empty())
            .unwrap_or("unspecified")
    }

    /// `meta/.galaxy_install_info`, `None` without one.
    fn install_info(&self) -> Option<Value> {
        let text = std::fs::read_to_string(self.path.join("meta/.galaxy_install_info")).ok()?;
        Some(load_yaml(&text)).filter(|info| !info.is_null())
    }

    fn metadata(&self) -> Value {
        ["meta/main.yml", "meta/main.yaml"]
            .iter()
            .find_map(|m| std::fs::read_to_string(self.path.join(m)).ok())
            .map_or(Value::Null, |t| load_yaml(&t))
    }

    /// Removes the role's directory when it holds a role (`GalaxyRole.remove`).
    fn remove(&self) -> bool {
        is_true(&self.metadata()) && std::fs::remove_dir_all(&self.path).is_ok()
    }
}

/// Installs `wanted` under `roles_path`, the first roles path, and returns the exit code. With a
/// requirements file, `only` keeps the roles it names.
pub fn install(
    wanted: &[Wanted],
    only: &[String],
    roles_path: &Path,
    servers: &mut Servers,
    options: Options,
) -> Result<i32, Failure> {
    println!("Starting galaxy role install process");
    let force = options.force || options.force_with_deps;
    let mut queue: Vec<Role> = wanted.iter().map(|w| Role::new(w, roles_path)).collect();
    let mut index = 0;
    while index < queue.len() {
        let mut role = queue[index].clone();
        index += 1;
        if !only.is_empty() && !only.contains(&role.name) {
            continue;
        }
        if let Some(info) = role.install_info() {
            let installed = &info["version"];
            if *installed != json!(role.version) || force {
                if force {
                    println!(
                        "- changing role {} from {} to {}",
                        role.name,
                        python_str(installed),
                        role.version_or_unspecified()
                    );
                    role.remove();
                } else {
                    eprintln!(
                        "[WARNING]: - {} ({}) is already installed - use --force to change version to {}",
                        role.name,
                        python_str(installed),
                        role.version_or_unspecified()
                    );
                    continue;
                }
            } else if !force {
                println!("- {} is already installed, skipping.", role.shown());
                continue;
            }
        }
        let installed = match install_one(&mut role, servers, options) {
            Ok(installed) => installed,
            Err(err) => {
                eprintln!(
                    "[WARNING]: - {} was NOT installed successfully: {}",
                    role.name,
                    err.summary()
                );
                exit_without_ignore(options)?;
                continue;
            }
        };
        if !options.no_deps && installed {
            dependencies(&role, roles_path, &mut queue, options)?;
        }
        if !installed {
            eprintln!("[WARNING]: - {} was NOT installed successfully.", role.name);
            exit_without_ignore(options)?;
        }
    }
    Ok(0)
}

fn exit_without_ignore(options: Options) -> Result<(), Failure> {
    if options.ignore_errors {
        return Ok(());
    }
    Err(Failure::error(
        "- you can use --ignore-errors to skip failed roles and finish processing the list.",
    ))
}

/// Queues the dependencies `meta/main.yml` and `meta/requirements.yml` name.
fn dependencies(
    role: &Role,
    roles_path: &Path,
    queue: &mut Vec<Role>,
    options: Options,
) -> Result<(), Failure> {
    let meta = role.metadata();
    if !is_true(&meta) {
        eprintln!(
            "[WARNING]: Meta file {} is empty. Skipping dependencies.",
            role.path.display()
        );
        return Ok(());
    }
    let mut listed = match &meta["dependencies"] {
        Value::Array(items) => items.clone(),
        other if !is_true(other) => Vec::new(),
        other => {
            return Err(Failure::error(format!(
                "Expected role dependencies to be a list. Role {} has meta/main.yml with dependencies {}",
                role.shown(),
                python_repr(other)
            )));
        }
    };
    for file in ["meta/requirements.yml", "meta/requirements.yaml"] {
        if let Ok(text) = std::fs::read_to_string(role.path.join(file)) {
            listed.extend(load_yaml(&text).as_array().cloned().unwrap_or_default());
            break;
        }
    }
    for dep in listed {
        let dep_role = Role::new(&parse_role(&dep)?, roles_path);
        if !dep_role.name.contains('.') && !dep_role.src.contains('.') && dep_role.scm.is_none() {
            continue;
        }
        let Some(info) = dep_role.install_info() else {
            if queue.iter().any(|r| r.name == dep_role.name) {
                println!(
                    "- dependency {} already pending installation.",
                    dep_role.name
                );
            } else {
                println!("- adding dependency: {}", dep_role.shown());
                queue.push(dep_role);
            }
            continue;
        };
        let installed = &info["version"];
        if *installed != json!(dep_role.version) {
            if options.force_with_deps {
                println!(
                    "- changing dependent role {} from {} to {}",
                    dep_role.name,
                    python_str(installed),
                    dep_role.version_or_unspecified()
                );
                dep_role.remove();
                queue.push(dep_role);
            } else {
                eprintln!(
                    "[WARNING]: - dependency {} ({}) from role {} differs from already installed version ({}), skipping",
                    dep_role.shown(),
                    python_str(&json!(dep_role.version)),
                    role.name,
                    python_str(installed)
                );
            }
        } else if options.force_with_deps {
            queue.push(dep_role);
        } else {
            println!(
                "- dependency {} is already installed, skipping.",
                dep_role.name
            );
        }
    }
    Ok(())
}

/// Fetches the role's archive and extracts it (`GalaxyRole.install`): `false` when the download
/// failed.
fn install_one(role: &mut Role, servers: &mut Servers, options: Options) -> Result<bool, Failure> {
    let holder = tempfile::tempdir().map_err(|e| Failure::error(e.to_string()))?;
    let archive = if role.scm.is_some() {
        Some(scm_archive(role, holder.path(), options)?)
    } else if role.src.is_empty() {
        return Err(Failure::error("No valid role data found"));
    } else if Path::new(&role.src).is_file() {
        Some(PathBuf::from(&role.src))
    } else if role.src.contains("://") {
        println!("- downloading role from {}", role.src);
        let src = role.src.clone();
        server(servers)?.fetch(&src, holder.path())
    } else {
        from_galaxy(role, server(servers)?, holder.path())?
    };
    let Some(archive) = archive else {
        return Ok(false);
    };
    extract(role, &archive, options.force)?;
    println!("- {} was installed successfully", role.shown());
    Ok(true)
}

/// A role a Galaxy server knows: its version picked, then its archive downloaded.
fn from_galaxy(role: &mut Role, api: &mut Server, dir: &Path) -> Result<Option<PathBuf>, Failure> {
    let Some(data) = api.lookup_role(&role.src)? else {
        return Err(Failure::error(format!(
            "- sorry, {} was not found on {}.",
            role.src,
            api.api_server()
        )));
    };
    let versions = api.role_versions(&data["id"]);
    let names: Vec<String> = versions.iter().map(|v| text_of(&v["name"])).collect();
    match role.version.as_deref() {
        None | Some("") => {
            role.version = Some(if let Some(highest) = highest(&names) {
                highest
            } else if let Some(branch) = data["github_branch"].as_str().filter(|b| !b.is_empty()) {
                branch.to_string()
            } else {
                "master".to_string()
            });
        }
        Some(v) if v != "master" && !versions.is_empty() && !names.iter().any(|n| n == v) => {
            return Err(Failure::error(format!(
                "- the specified version ({v}) of {} was not found in the list of available versions ({}).",
                role.name,
                python_repr(&Value::Array(versions))
            )));
        }
        Some(_) => {}
    }
    let version = role.version.clone().unwrap_or_default();
    let mut download_url = None;
    for v in versions.iter().filter(|v| text_of(&v["name"]) == version) {
        if let Some(source) = v.get("source") {
            role.src = text_of(source);
        }
        if let Some(url) = v.get("download_url") {
            download_url = Some(text_of(url));
        }
    }
    let url = match (
        download_url,
        data.get("github_user"),
        data.get("github_repo"),
    ) {
        (Some(url), ..) => url,
        (None, Some(user), Some(repo)) => format!(
            "https://github.com/{}/{}/archive/{version}.tar.gz",
            text_of(user),
            text_of(repo)
        ),
        _ => role.src.clone(),
    };
    println!("- downloading role from {url}");
    Ok(api.fetch(&url, dir))
}

fn server(servers: &mut Servers) -> Result<&mut Server, Failure> {
    servers
        .list
        .first_mut()
        .ok_or_else(|| Failure::error("No valid role data found"))
}

/// The highest of `names` as `LooseVersion` orders them: numbers and words compared part by part.
fn highest(names: &[String]) -> Option<String> {
    fn parts(v: &str) -> Vec<(u64, String)> {
        let mut out = Vec::new();
        let mut chars = v.chars().peekable();
        while let Some(&c) = chars.peek() {
            let digit = c.is_ascii_digit();
            let mut part = String::new();
            while let Some(&c) = chars.peek() {
                if c == '.' || c.is_ascii_digit() != digit {
                    break;
                }
                part.push(c);
                chars.next();
            }
            if part.is_empty() {
                chars.next();
            } else if digit {
                out.push((part.parse().unwrap_or(0), String::new()));
            } else {
                out.push((u64::MAX, part));
            }
        }
        out
    }
    names.iter().max_by(|a, b| parts(a).cmp(&parts(b))).cloned()
}

fn load_yaml(text: &str) -> Value {
    crate::yaml::load(text, "role")
        .ok()
        .and_then(|docs| docs.first().and_then(|d| crate::yaml::to_json(d).ok()))
        .unwrap_or(Value::Null)
}

/// Python's truth of a value.
fn is_true(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Python's `str` of a value.
fn python_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => python_repr(other),
    }
}

/// Python's `repr` of a value.
fn python_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(python_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(k, v)| format!("'{k}': {}", python_repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}
