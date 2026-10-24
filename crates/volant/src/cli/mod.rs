// SPDX-License-Identifier: GPL-3.0-or-later
//! The `playbook` command, with the same arguments as `ansible-playbook` for the subset that
//! exists, the `runtime` command, the `shims` command and the `vault` command.

mod announce;
mod args;
mod hosts;
mod passwords;
mod program;
mod run;
mod runtime;
mod shims;
mod signals;
mod vault;

pub use args::PlaybookArgs;
pub use program::{TOOLS, subcommand_for};
pub use run::run;
pub use runtime::{RuntimeCommand, runtime};
pub use shims::{ShimsCommand, shims};
pub use vault::{VaultArgs, vault};

#[cfg(test)]
mod tests;
