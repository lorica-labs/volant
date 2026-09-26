// SPDX-License-Identifier: GPL-3.0-or-later
//! Volant controller: loads Ansible content and drives agents.

pub mod action_plugins;
pub mod agent;
pub mod cli;
pub(crate) mod compile;
pub mod config;
pub(crate) mod executor;
pub mod inventory;
pub mod keywords;
pub(crate) mod listing;
pub mod playbook;
pub mod preflight;
pub mod profile;
pub mod python;
pub(crate) mod render;
pub(crate) mod roles;
pub(crate) mod stats;
pub mod template;
pub mod transport;
pub mod union_cache;
pub mod vars;
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
