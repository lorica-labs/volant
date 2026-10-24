// SPDX-License-Identifier: GPL-3.0-or-later
//! The `shims` command: symbolic links that start Volant under the ansible-core program names.

use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};

use super::program::TOOLS;

/// The name a shim points at.
const BINARY: &str = "volant";

#[derive(Args, Debug)]
pub struct ShimsArgs {
    /// The directory of the links. Default: `$VOLANT_BIN_DIR`, then `~/.local/bin`.
    #[arg(long, value_name = "DIR")]
    dir: Option<PathBuf>,
    /// The programs to link. Default: all of them.
    #[arg(value_name = "NAME", value_parser = clap::builder::PossibleValuesParser::new(TOOLS.map(|(name, _)| name)))]
    names: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum ShimsCommand {
    /// Link the ansible-core program names to volant.
    Install {
        #[command(flatten)]
        args: ShimsArgs,
        /// Replace a file or link that is not a Volant link.
        #[arg(long)]
        force: bool,
    },
    /// Remove the links `install` made, and only those.
    Remove {
        #[command(flatten)]
        args: ShimsArgs,
    },
}

/// Runs a `shims` subcommand and returns the exit code.
pub fn shims(command: ShimsCommand) -> i32 {
    match execute(command) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ERROR! {e}");
            1
        }
    }
}

fn execute(command: ShimsCommand) -> Result<i32, String> {
    let (args, force, install) = match command {
        ShimsCommand::Install { args, force } => (args, force, true),
        ShimsCommand::Remove { args } => (args, false, false),
    };
    let dir = match args.dir {
        Some(dir) => dir,
        None => default_dir()?,
    };
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    let names: Vec<&str> = if args.names.is_empty() {
        TOOLS.iter().map(|(name, _)| *name).collect()
    } else {
        args.names.iter().map(String::as_str).collect()
    };
    let target = if install {
        link_target(&dir)?
    } else {
        PathBuf::new()
    };
    let mut refused = false;
    for name in names {
        let path = dir.join(name);
        let line = if install {
            install_one(&dir, &path, name, &target, force)?
        } else {
            remove_one(&path)?
        };
        refused |= line.refused;
        println!("{name}: {}", line.text);
    }
    Ok(i32::from(refused))
}

struct Line {
    text: String,
    refused: bool,
}

fn said(text: impl Into<String>) -> Line {
    Line {
        text: text.into(),
        refused: false,
    }
}

fn default_dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("VOLANT_BIN_DIR").filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".local/bin"))
        .ok_or_else(|| "neither VOLANT_BIN_DIR nor HOME is set: pass --dir".to_string())
}

/// `volant` when `dir` holds an entry of that name, so an upgrade of that link carries the shims
/// along; otherwise the path of the running binary.
fn link_target(dir: &Path) -> Result<PathBuf, String> {
    if dir.join(BINARY).symlink_metadata().is_ok() {
        return Ok(PathBuf::from(BINARY));
    }
    std::env::current_exe().map_err(|e| format!("cannot find the running binary: {e}"))
}

/// Whether `path` is a symbolic link to a file called `volant`.
fn is_volant_link(path: &Path) -> bool {
    std::fs::read_link(path)
        .ok()
        .and_then(|target| target.file_name().map(|name| name == BINARY))
        .unwrap_or(false)
}

fn install_one(
    dir: &Path,
    path: &Path,
    name: &str,
    target: &Path,
    force: bool,
) -> Result<Line, String> {
    let replaced = match path.symlink_metadata() {
        Ok(_) if is_volant_link(path) => return Ok(said("already installed")),
        Ok(_) if !force => {
            return Ok(Line {
                text: format!(
                    "not replaced, {} exists and is not a Volant link",
                    path.display()
                ),
                refused: true,
            });
        }
        Ok(_) => {
            std::fs::remove_file(path)
                .map_err(|e| format!("cannot replace {}: {e}", path.display()))?;
            true
        }
        Err(e) if e.kind() == ErrorKind::NotFound => false,
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    link(target, path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    let mut text = (if replaced { "replaced" } else { "installed" }).to_string();
    if let Some(note) = path_note(dir, name, std::env::var_os("PATH").unwrap_or_default()) {
        text.push_str(". ");
        text.push_str(&note);
    }
    Ok(said(text))
}

fn remove_one(path: &Path) -> Result<Line, String> {
    match path.symlink_metadata() {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(said("not installed")),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        Ok(_) if is_volant_link(path) => {
            std::fs::remove_file(path)
                .map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
            Ok(said("removed"))
        }
        Ok(_) => Ok(Line {
            text: "not a Volant link, left alone".to_string(),
            refused: true,
        }),
    }
}

#[cfg(unix)]
fn link(target: &Path, path: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

#[cfg(not(unix))]
fn link(_target: &Path, _path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "shims are only supported on Linux and macOS",
    ))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// The sentence that says a real program of the same name is on `search_path` (the value of
/// `PATH`), and which one runs first.
fn path_note(dir: &Path, name: &str, search_path: OsString) -> Option<String> {
    let mut seen_dir = false;
    for entry in std::env::split_paths(&search_path).filter(|e| !e.as_os_str().is_empty()) {
        if same_dir(&entry, dir) {
            seen_dir = true;
            continue;
        }
        let found = entry.join(name);
        if !is_executable(&found) {
            continue;
        }
        let later = std::env::split_paths(&search_path).any(|e| same_dir(&e, dir));
        let found = found.display();
        let dir = dir.display();
        return Some(if seen_dir {
            format!("note: {found} is hidden by this link, {dir} comes first in PATH")
        } else if later {
            format!("note: {found} still runs first, {dir} comes later in PATH")
        } else {
            format!("note: {found} still runs first, {dir} is not in PATH")
        });
    }
    None
}
