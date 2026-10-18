// SPDX-License-Identifier: GPL-3.0-or-later
//! The `runtime` command: install, show and remove the Python runtime Volant manages.

use clap::Subcommand;

use crate::runtime::install::{self, installed};
use crate::runtime::manifest::Manifest;
use crate::runtime::{runtimes_dir, status};

#[derive(Subcommand, Debug)]
pub enum RuntimeCommand {
    /// Download and install Volant's Python and ansible-core, checked against pinned hashes.
    Install,
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

fn execute(command: RuntimeCommand) -> Result<(), String> {
    let runtimes = runtimes_dir()?;
    let manifest = Manifest::pinned()?;
    match command {
        RuntimeCommand::Install => {
            let done = install::install(&runtimes, &manifest)?;
            println!("{} is installed in {}", done.id, done.dir.display());
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
