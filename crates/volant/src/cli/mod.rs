// SPDX-License-Identifier: GPL-3.0-or-later
//! The `playbook` command, with the same arguments as `ansible-playbook` for the subset that
//! exists, and the `runtime` command.

mod announce;
mod args;
mod hosts;
mod passwords;
mod run;
mod runtime;

pub use args::PlaybookArgs;
pub use run::run;
pub use runtime::{RuntimeCommand, runtime};

#[cfg(test)]
mod tests;
