// SPDX-License-Identifier: GPL-3.0-or-later
//! Volant controller: loads Ansible content and drives agents.

/// The controller allocates on every thread of its runtime for every task of every host. Measured
/// on a thousand local hosts and 20 tasks: the static musl build took 12.2 s with musl's own
/// `malloc` and 3.0 s with this one, and the glibc build 4.3 s with glibc's and 3.0 s with this one.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
pub(crate) mod galaxy;
pub mod inventory;
pub mod keywords;
pub(crate) mod listing;
pub(crate) mod lock;
pub mod playbook;
pub mod preflight;
pub(crate) mod profile;
pub(crate) mod prompt;
pub(crate) mod python;
pub(crate) mod render;
pub(crate) mod roles;
pub(crate) mod runtime;
pub mod shadow;
pub(crate) mod splitter;
pub(crate) mod stats;
pub mod tags;
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
