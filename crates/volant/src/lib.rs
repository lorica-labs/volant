// SPDX-License-Identifier: GPL-3.0-or-later
//! Volant controller: loads Ansible content and drives agents.

pub mod inventory;

/// Version string shown by `--version`: semver, git sha and build date.
pub const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("VOLANT_GIT_SHA"),
    " ",
    env!("VOLANT_BUILD_DATE"),
    ")"
);
