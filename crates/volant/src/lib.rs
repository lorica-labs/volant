// SPDX-License-Identifier: GPL-3.0-or-later
//! Volant controller: loads Ansible content and drives agents.

pub mod agent;
pub mod cli;
pub mod config;
pub mod executor;
pub mod inventory;
pub mod playbook;
pub mod render;
pub mod stats;
pub mod template;
pub mod transport;
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
