// SPDX-License-Identifier: GPL-3.0-or-later
//! The `runtime` command: install, show and remove the Python runtime Volant manages.

use std::path::Path;

use clap::{Parser as _, Subcommand};

use crate::runtime::install::{self, installed};
use crate::runtime::manifest::Manifest;
use crate::runtime::pack;
use crate::runtime::{collections, extras};
use crate::runtime::{runtimes_dir, status};

#[derive(Subcommand, Debug)]
pub enum RuntimeCommand {
    /// Download and install Volant's Python and ansible-core, checked against pinned hashes.
    Install {
        /// Also install collections into the runtime: names (`namespace.name[:version]`) or a
        /// requirements file ending in .yml or .yaml.
        #[arg(long, num_args = 1.., value_name = "NAME_OR_FILE")]
        collections: Vec<String>,
        /// Also install optional wheel sets: `windows` (pywinrm, pypsrp), `network` (paramiko,
        /// ansible-pylibssh, ncclient). Comma-separated.
        #[arg(long, value_delimiter = ',', value_name = "SET")]
        extras: Vec<String>,
        /// Install from a file `volant runtime pack` made, with no download.
        #[arg(long, value_name = "FILE")]
        from: Option<std::path::PathBuf>,
        /// Refuse the file given to --from unless its SHA-256 is this.
        #[arg(long, value_name = "HASH", requires = "from")]
        sha256: Option<String>,
    },
    /// Pack the installed runtime into a file another host of the same platform can install from.
    Pack {
        /// The file to write.
        file: std::path::PathBuf,
        /// Install these optional wheel sets first, so the file carries them.
        #[arg(long, value_delimiter = ',', value_name = "SET")]
        extras: Vec<String>,
    },
    /// Show the managed runtime and whether it is installed.
    Status,
    /// Print the managed runtime's Python interpreter.
    Path,
    /// Delete the managed runtime, the runtime named, or every runtime with --all.
    Remove {
        /// The runtime to delete, as `volant runtime status` names it.
        #[arg(conflicts_with = "all")]
        name: Option<String>,
        /// Delete every runtime.
        #[arg(long)]
        all: bool,
    },
}

/// Runs a `runtime` subcommand and returns the exit code.
pub fn runtime(command: RuntimeCommand) -> i32 {
    match execute(command) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("ERROR! {e}");
            1
        }
    }
}

/// `volant galaxy collection install -p <target>` with `args`, the native install.
fn install_collections<'a>(
    target: &Path,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<(), String> {
    let target = target.to_string_lossy().into_owned();
    let argv = ["galaxy", "collection", "install", "-p"]
        .into_iter()
        .map(str::to_owned)
        .chain([target])
        .chain(args.into_iter().map(str::to_owned));
    let parsed = super::GalaxyArgs::try_parse_from(argv).map_err(|e| e.to_string())?;
    match super::galaxy(parsed) {
        0 => Ok(()),
        _ => Err("the collections could not be installed: check the names and the network, then run the command again".to_owned()),
    }
}

fn execute(command: RuntimeCommand) -> Result<(), String> {
    let runtimes = runtimes_dir()?;
    let manifest = Manifest::pinned()?;
    match command {
        RuntimeCommand::Install {
            collections,
            extras,
            from,
            sha256,
        } => {
            extras::known(&extras)?;
            let done = match &from {
                Some(file) => {
                    let found = pack::verify(file, sha256.as_deref())?;
                    println!("sha256 {found}");
                    pack::install_from(&runtimes, &manifest, file)?
                }
                None => install::install(&runtimes, &manifest)?,
            };
            println!("{} is installed in {}", done.id, done.dir.display());
            if from.is_some() {
                let have = extras::installed_sets(&done.dir);
                if let Some(set) = extras.iter().find(|set| !have.contains(set)) {
                    return Err(format!(
                        "{} holds no {set:?} extras: pack it again with `volant runtime pack                          --extras {set}` on a host that has a network",
                        from.as_deref().unwrap_or(Path::new("")).display()
                    ));
                }
            }
            extras::install(&runtimes, &manifest, &done, &extras)?;
            if !extras.is_empty() {
                println!("extras: {}", extras::installed_sets(&done.dir).join(", "));
            }
            let (names, files) = collections::split(&collections, Path::is_file);
            let target = collections::dir(&done.dir);
            if !names.is_empty() {
                if let Some(bad) = names.iter().find(|name| name.starts_with('-')) {
                    return Err(format!(
                        "'{bad}' is not a collection name: write namespace.name, optionally with a version"
                    ));
                }
                install_collections(&target, names.iter().map(String::as_str))?;
            }
            for file in &files {
                let file = file.to_string_lossy();
                install_collections(&target, ["-r", file.as_ref()])?;
            }
        }
        RuntimeCommand::Pack { file, extras } => {
            if !extras.is_empty() {
                let done = installed(&runtimes, &manifest.id()).ok_or(
                    "Volant's Python runtime is not installed: run `volant runtime install` first",
                )?;
                extras::install(&runtimes, &manifest, &done, &extras)?;
            }
            let hash = pack::pack(&runtimes, &manifest, &file)?;
            println!("packed {}", file.display());
            println!("sha256 {hash}");
        }
        RuntimeCommand::Status => {
            let config = crate::config::Config::load().unwrap_or_else(|e| {
                eprintln!("[WARNING]: the configuration cannot be read, so this uses the default runtime mode: {e:#}");
                crate::config::Config::default()
            });
            crate::runtime::resolve::configured(config);
            print!("{}", status(&runtimes, &manifest));
        }
        RuntimeCommand::Path => {
            let done = installed(&runtimes, &manifest.id()).ok_or(
                "Volant's Python runtime is not installed: run `volant runtime install` first",
            )?;
            println!("{}", done.python.display());
        }
        RuntimeCommand::Remove { all: true, .. } => {
            for id in install::remove_all(&runtimes)? {
                println!("removed {id}");
            }
        }
        RuntimeCommand::Remove { name, all: false } => {
            let id = name.unwrap_or_else(|| manifest.id());
            if install::remove(&runtimes, &id)? {
                println!("removed {id}");
            } else {
                println!("{id} is not installed");
            }
        }
    }
    Ok(())
}
