// SPDX-License-Identifier: GPL-3.0-or-later
//! The `playbook` command, with the same arguments as `ansible-playbook` for the subset that
//! exists, the `runtime` command and the `shims` command.

mod announce;
mod args;
mod hosts;
mod inventory;
mod passwords;
mod program;
mod run;
mod runtime;
mod shims;
mod signals;

pub use args::PlaybookArgs;
pub use inventory::{InventoryArgs, inventory};
pub use program::{TOOLS, subcommand_for};
pub use run::run;
pub use runtime::{RuntimeCommand, runtime};
pub use shims::{ShimsCommand, shims};

#[cfg(test)]
mod tests;
