// SPDX-License-Identifier: GPL-3.0-or-later
//! Volant controller: loads Ansible content and drives agents.

pub mod action_plugins;
pub mod agent;
pub mod askpass;
pub mod bridge;
pub mod cli;
pub(crate) mod collection;
pub(crate) mod compile;
pub(crate) mod config;
pub(crate) mod executor;
pub(crate) mod fact_cache;
pub(crate) mod facts_read;
pub mod inventory;
pub mod keywords;
pub(crate) mod listing;
pub mod playbook;
pub mod preflight;
pub(crate) mod profile;
pub(crate) mod prompt;
pub(crate) mod python;
pub(crate) mod render;
pub(crate) mod roles;
pub(crate) mod runtime;
pub(crate) mod splitter;
pub(crate) mod stats;
pub mod template;
#[cfg(test)]
pub(crate) mod testing;
pub mod transport;
pub(crate) mod union_cache;
pub(crate) mod vars;
pub(crate) mod vault;
pub mod yaml;

/// Version string shown by `--version`: semver, git sha and build date.
pub const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("VOLANT_GIT_SHA"),
    " ",
    env!("VOLANT_BUILD_DATE"),
    ")"
);
