// SPDX-License-Identifier: GPL-3.0-or-later
//! The `playbook` command, with the same arguments as `ansible-playbook` for the subset that
//! exists, the `adhoc` command, which is `ansible`, the `runtime` command, the `shims` command and the
//! `vault` command.

mod adhoc;
mod adhoc_options;
mod adhoc_usage;
mod announce;
mod args;
mod compat;
mod console;
mod galaxy;
mod hosts;
mod inventory;
#[cfg(unix)]
mod locale;
mod passwords;
mod plays;
mod program;
mod pull;
mod run;
mod runtime;
mod setup;
mod shims;
mod signals;
mod tool;
mod vault;
mod vault_args;
mod vault_edit;
mod vault_string;
mod version;

pub use crate::lock::run as lock;
pub use adhoc::adhoc;
pub use adhoc_usage::usage_error as adhoc_usage_error;
pub use args::{AdhocArgs, BecomeArgs, ConnectionArgs, HostsArgs, PlaybookArgs, VaultSecretArgs};
pub use compat::{CompatCommand, compat};
pub use console::{ConsoleArgs, console};
pub use galaxy::{GalaxyArgs, galaxy};
pub use inventory::{InventoryArgs, inventory};
#[cfg(unix)]
pub use locale::check_locale;
pub(crate) use plays::replay;
pub use plays::run;
pub use program::{TOOLS, subcommand_for};
pub use pull::{PullArgs, pull};
pub use runtime::{RuntimeCommand, runtime};
pub use shims::{ShimsCommand, shims};
pub use tool::{ToolArgs, config, doc};
pub use vault::{VaultArgs, vault};

#[cfg(test)]
mod tests;
