// SPDX-License-Identifier: GPL-3.0-or-later
//! `role list` and `role remove` (`execute_list_role`, `execute_remove`).

use std::path::{Path, PathBuf};

use super::{Role, is_true, load_yaml, python_str};
use crate::galaxy::Failure;

/// `role list [NAME]`: each roles path, its roles and their versions.
pub fn list(paths: &[PathBuf], name: Option<&str>) -> Result<i32, Failure> {
    let mut warnings = Vec::new();
    let mut usable = false;
    let mut found = false;
    for path in paths {
        if !path.is_dir() {
            warnings.push(format!(
                "- the configured path {} does not exist.",
                path.display()
            ));
            continue;
        }
        usable = true;
        if let Some(name) = name {
            let role = path.join(name);
            if role.is_dir() {
                found = true;
                println!("# {}", path.display());
                show(&role, name);
                break;
            }
            warnings.push(format!("- the role {name} was not found"));
        } else {
            println!("# {}", path.display());
            let mut entries: Vec<PathBuf> = std::fs::read_dir(path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .collect();
            entries.sort();
            for entry in entries {
                let name = entry
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let role = Role {
                    src: name.clone(),
                    name,
                    version: None,
                    scm: None,
                    path: entry,
                };
                if is_true(&role.metadata()) {
                    show(&role.path, &role.name);
                }
            }
        }
    }
    if !found {
        for warning in warnings {
            eprintln!("[WARNING]: {warning}");
        }
    }
    if !usable {
        return Err(Failure::options(
            "- None of the provided paths were usable. Please specify a valid path with --roles-path",
        ));
    }
    Ok(0)
}

/// `_display_role`: `- name, version`, `(unknown version)` without one.
fn show(role: &Path, name: &str) {
    let version = std::fs::read_to_string(role.join("meta/.galaxy_install_info"))
        .ok()
        .map(|t| load_yaml(&t)["version"].clone())
        .filter(is_true)
        .map_or_else(|| "(unknown version)".into(), |v| python_str(&v));
    println!("- {name}, {version}");
}

/// `role remove NAME...`, from the first roles path.
pub fn remove(names: &[String], roles_path: &Path) -> Result<i32, Failure> {
    if names.is_empty() {
        return Err(Failure::options(
            "- you must specify at least one role to remove.",
        ));
    }
    for name in names {
        let role = Role {
            name: name.clone(),
            src: name.clone(),
            version: None,
            scm: None,
            path: roles_path.join(name),
        };
        if role.remove() {
            println!("- successfully removed {name}");
        } else {
            println!("- {name} is not installed, skipping.");
        }
    }
    Ok(0)
}
