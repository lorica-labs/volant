// SPDX-License-Identifier: GPL-3.0-or-later
//! The `playbook` command: the same arguments as `ansible-playbook`, for the subset that exists.

mod announce;
mod args;
mod hosts;
mod passwords;
mod run;

pub use args::PlaybookArgs;
pub use run::run;

#[cfg(test)]
mod tests;
